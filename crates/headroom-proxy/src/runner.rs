//! Library entrypoints for booting and serving the Headroom proxy.

use std::fmt;
use std::net::SocketAddr;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::inbound_auth::{check_bind_policy, BindPolicy, OPEN_BIND_ACK_ENV};
use crate::proxy::{build_app, AppState};

/// Returned when a non-loopback bind lacks `HEADROOM_PROXY_TOKEN` or
/// `HEADROOM_ALLOW_UNAUTHENTICATED_BIND`.
#[derive(Debug, Clone)]
pub struct BindPolicyError(pub String);

impl fmt::Display for BindPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BindPolicyError {}

/// Initialize JSON structured tracing at `level`. Safe to call repeatedly.
pub fn init_tracing(level: &str) {
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    let json_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(false)
        .with_span_list(false);
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(json_layer)
        .try_init();
}

/// Bind `config.listen`, verify bind policy, and run the HTTP/SSE/WebSocket server until shutdown.
pub async fn run_server(config: Config) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ack = std::env::var(OPEN_BIND_ACK_ENV).ok();
    match check_bind_policy(config.listen, config.proxy_token.is_some(), ack.as_deref()) {
        Ok(BindPolicy::Ok) => {}
        Ok(BindPolicy::OpenAcknowledged) => tracing::warn!(
            event = "proxy_open_bind",
            listen = %config.listen,
            "non-loopback listen with no HEADROOM_PROXY_TOKEN; every peer that \
             reaches this port is relayed upstream as a loopback caller. \
             Acknowledged via HEADROOM_ALLOW_UNAUTHENTICATED_BIND"
        ),
        Err(msg) => return Err(Box::new(BindPolicyError(msg))),
    }

    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    serve_on_listener(listener, config).await
}

/// Serve the proxy on an already-bound `TcpListener` until shutdown signal.
pub async fn serve_on_listener(
    listener: tokio::net::TcpListener,
    config: Config,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing::info!(
        listen = %config.listen,
        upstream = %config.upstream,
        upstream_timeout_s = config.upstream_timeout.as_secs(),
        upstream_connect_timeout_s = config.upstream_connect_timeout.as_secs(),
        max_body_bytes = config.max_body_bytes,
        rewrite_host = config.rewrite_host,
        graceful_shutdown_timeout_s = config.graceful_shutdown_timeout.as_secs(),
        rollout_channel = config.rollout.config.channel.as_str(),
        rollout_features_enabled = ?config.rollout.enabled(),
        rollout_features_disabled = ?config.rollout.config.disabled,
        unsafe_allow_unstable_features = config.rollout.config.unsafe_allow_unstable,
        rollout_registry_digest = %config.rollout.registry_digest,
        rollout_snapshot_digest = %config.rollout.snapshot_digest(),
        qualification_eligible = config.rollout.qualification_eligible(),
        "headroom-proxy starting"
    );

    if config.beta_header_sticky.is_enabled() && !config.compression {
        tracing::warn!(
            event = "beta_header_sticky_inactive",
            beta_header_sticky = config.beta_header_sticky.as_str(),
            compression = config.compression,
            "beta-header stickiness is enabled but the compression \
             interceptor is off; enable --compression (or \
             HEADROOM_PROXY_COMPRESSION=1) to activate it"
        );
    }

    if config.compression {
        if let Err(e) = std::thread::Builder::new()
            .name("live-zone-warmup".into())
            .spawn(|| {
                let ready = headroom_core::transforms::live_zone::warm_live_zone_compressors();
                tracing::info!(event = "live_zone_warmup_complete", kompress_ready = ready);
            })
        {
            tracing::warn!(event = "live_zone_warmup_spawn_failed", error = %e);
        }
    }

    let mut state = AppState::new(config.clone())?;

    if config.enable_bedrock_native {
        match load_bedrock_credentials(&config).await {
            Ok(source) => {
                let first = source.get().await;
                state = state.with_bedrock_credentials_source(source);
                match first {
                    Ok(_) => tracing::info!(
                        event = "bedrock_credentials_loaded",
                        region = %config.bedrock_region,
                        profile = ?config.aws_profile,
                        "AWS credentials resolved for Bedrock SigV4 signing"
                    ),
                    Err(e) => tracing::warn!(
                        event = "bedrock_credentials_unavailable",
                        region = %config.bedrock_region,
                        profile = ?config.aws_profile,
                        error = %e,
                        "AWS credentials not available at startup; each Bedrock request asks the provider again until it succeeds"
                    ),
                }
            }
            Err(e) => {
                tracing::warn!(
                    event = "bedrock_credentials_unavailable",
                    region = %config.bedrock_region,
                    profile = ?config.aws_profile,
                    error = %e,
                    "AWS credentials not available at startup; Bedrock invoke will 5xx until creds are configured"
                );
            }
        }
    }

    if config.compression && matches!(config.compression_mode, crate::config::CompressionMode::Off)
    {
        tracing::warn!(
            event = "compression_mode_off",
            "--compression is on but --compression-mode is `off`: bodies \
             pass through unmodified and the dashboard's compression \
             savings will read 0. Set --compression-mode live_zone to \
             engage the dispatcher."
        );
    }

    let stats_ledger = state.stats.clone();
    if config.stats {
        crate::observability::pricing::warm();
        tracing::info!(
            event = "stats_enabled",
            path = ?stats_ledger.path().map(|p| p.display().to_string()),
            "native savings stats active (/stats, /dashboard)"
        );
        crate::observability::ledger::spawn_flusher(stats_ledger.clone());
    }

    let app = build_app(state).into_make_service_with_connect_info::<SocketAddr>();
    tracing::info!(addr = %listener.local_addr()?, "listening");

    let grace = config.graceful_shutdown_timeout;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!(
                timeout_s = grace.as_secs(),
                "draining in-flight requests before exit"
            );
            tokio::time::sleep(grace).await;
        })
        .await?;

    stats_ledger.flush().await;
    Ok(())
}

async fn load_bedrock_credentials(
    config: &Config,
) -> Result<crate::bedrock::BedrockCredentials, Box<dyn std::error::Error + Send + Sync>> {
    use aws_config::BehaviorVersion;

    let mut loader = aws_config::defaults(BehaviorVersion::latest())
        .region(aws_config::Region::new(config.bedrock_region.clone()));
    if let Some(profile) = config.aws_profile.as_deref() {
        loader = loader.profile_name(profile);
    }
    let aws_config = loader.load().await;
    let creds_provider = aws_config
        .credentials_provider()
        .ok_or("no credentials provider configured")?;
    Ok(crate::bedrock::BedrockCredentials::new(creds_provider))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
