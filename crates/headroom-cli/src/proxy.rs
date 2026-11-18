//! `headroom proxy` — run the pure-Rust HTTP/SSE/WebSocket reverse proxy.

use std::net::IpAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args, Parser};
use headroom_proxy::config::{CliArgs, CompressionMode};
use headroom_proxy::{init_tracing, run_server, BindPolicyError, Config};

use crate::workspace::{default_ccr_db_path, default_stats_path};

/// Arguments for `headroom proxy`.
#[derive(Debug, Args)]
pub struct ProxyArgs {
    /// Port to listen on.
    #[arg(long, default_value_t = 8787)]
    pub port: u16,

    /// Host IP/interface to bind.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Upstream LLM API base URL.
    #[arg(long, default_value = "https://api.anthropic.com")]
    pub upstream: String,

    /// Disable live-zone compression and forward byte-for-byte.
    #[arg(long, default_value_t = false)]
    pub no_compress: bool,

    /// Disable in-process CCR storage and `/v1/retrieve` endpoints.
    #[arg(long, default_value_t = false)]
    pub no_ccr: bool,

    /// Override SQLite database path for CCR storage.
    #[arg(long)]
    pub ccr_db_path: Option<PathBuf>,

    /// Override ledger JSON persistence path.
    #[arg(long)]
    pub stats_path: Option<PathBuf>,

    /// Keep a ring buffer of recent before/after payloads for `/transformations/feed`.
    #[arg(long, default_value_t = false)]
    pub log_messages: bool,

    /// Log level filter (`error`, `warn`, `info`, `debug`, `trace`).
    #[arg(long, default_value = "info")]
    pub log_level: String,
}

/// Whether live-zone compression should run in the proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionSetting {
    Enabled,
    Disabled,
}

/// Whether in-process CCR storage should run in the proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcrSetting {
    Enabled,
    Disabled,
}

/// Whether request transformation diffs should be buffered in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageLogSetting {
    Enabled,
    Disabled,
}

/// Typed launch options for building a proxy [`Config`].
#[derive(Debug, Clone)]
pub struct ProxyLaunchOptions {
    pub host: String,
    pub port: u16,
    pub upstream: String,
    pub compression: CompressionSetting,
    pub ccr: CcrSetting,
    pub ccr_db_path: Option<PathBuf>,
    pub stats_path: Option<PathBuf>,
    pub log_messages: MessageLogSetting,
    pub log_level: String,
}

/// Build a [`Config`] from typed [`ProxyLaunchOptions`].
pub fn build_proxy_config(opts: &ProxyLaunchOptions) -> Result<Config> {
    let ip: IpAddr = opts
        .host
        .parse()
        .with_context(|| format!("invalid listen host IP {:?}", opts.host))?;
    let listen = std::net::SocketAddr::new(ip, opts.port).to_string();

    let cli_args = CliArgs::try_parse_from([
        "headroom-proxy",
        "--listen",
        &listen,
        "--upstream",
        &opts.upstream,
    ])
    .context("invalid proxy arguments")?;

    let mut cfg = Config::from_cli(cli_args);
    let enable_comp = opts.compression == CompressionSetting::Enabled;
    let enable_ccr = opts.ccr == CcrSetting::Enabled;
    cfg.compression = enable_comp;
    cfg.compression_mode = if enable_comp {
        CompressionMode::LiveZone
    } else {
        CompressionMode::Off
    };
    cfg.ccr = enable_ccr;
    cfg.ccr_db_path = if enable_ccr {
        opts.ccr_db_path.clone().or_else(default_ccr_db_path)
    } else {
        None
    };
    cfg.stats = true;
    if let Some(p) = opts.stats_path.clone().or_else(default_stats_path) {
        cfg.stats_path = Some(p);
    }
    cfg.log_messages = opts.log_messages == MessageLogSetting::Enabled;
    cfg.log_level = opts.log_level.clone();
    Ok(cfg)
}

/// Execute `headroom proxy`.
pub async fn run(args: ProxyArgs) -> Result<ExitCode> {
    let cfg = build_proxy_config(&ProxyLaunchOptions {
        host: args.host,
        port: args.port,
        upstream: args.upstream,
        compression: if args.no_compress {
            CompressionSetting::Disabled
        } else {
            CompressionSetting::Enabled
        },
        ccr: if args.no_ccr {
            CcrSetting::Disabled
        } else {
            CcrSetting::Enabled
        },
        ccr_db_path: args.ccr_db_path,
        stats_path: args.stats_path,
        log_messages: if args.log_messages {
            MessageLogSetting::Enabled
        } else {
            MessageLogSetting::Disabled
        },
        log_level: args.log_level,
    })?;

    init_tracing(&cfg.log_level);

    if let Err(err) = run_server(cfg).await {
        if let Some(bind_err) = err.downcast_ref::<BindPolicyError>() {
            eprintln!("headroom proxy: {bind_err}");
            return Ok(ExitCode::from(2));
        }
        return Err(anyhow::anyhow!("{err}"));
    }

    Ok(ExitCode::SUCCESS)
}
