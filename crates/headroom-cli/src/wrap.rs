//! `headroom wrap` & `headroom unwrap` — launch any agent behind an automatic Headroom proxy.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::proxy::{
    build_proxy_config, CcrSetting, CompressionSetting, MessageLogSetting, ProxyLaunchOptions,
};
use crate::workspace::wrap_sessions_dir;

/// Arguments for `headroom wrap`.
#[derive(Debug, Args)]
pub struct WrapArgs {
    /// Agent alias or executable path (`claude`, `codex`, `cursor`, `gemini`, `aider`, ...).
    pub agent: String,

    /// Proxy port (`0` selects an available ephemeral port and starts a dedicated proxy).
    #[arg(long, default_value_t = 8787)]
    pub port: u16,

    /// Upstream LLM API base URL.
    #[arg(long, default_value = "https://api.anthropic.com")]
    pub upstream: String,

    /// Accepted for CLI compatibility with legacy RTK wrappers.
    #[arg(long, default_value_t = false)]
    pub no_rtk: bool,

    /// Proxy log level (`error`, `warn`, `info`, `debug`, `trace`).
    #[arg(long, default_value = "warn")]
    pub log_level: String,

    /// Arguments forwarded verbatim to the target agent/command.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

/// Arguments for `headroom unwrap`.
#[derive(Debug, Args)]
pub struct UnwrapArgs {
    /// Remove all recorded wrap session markers under the workspace directory.
    #[arg(long, default_value_t = false)]
    pub all: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WrapSessionRecord {
    pid: u32,
    port: u16,
    agent: String,
    started_at: String,
}

struct WrapSessionGuard {
    session_file: Option<PathBuf>,
    proxy_task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for WrapSessionGuard {
    fn drop(&mut self) {
        if let Some(ref path) = self.session_file {
            let _ = std::fs::remove_file(path);
        }
        if let Some(task) = self.proxy_task.take() {
            task.abort();
        }
    }
}

async fn probe_healthz(client: &reqwest::Client, port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/healthz");
    matches!(client.get(&url).send().await, Ok(resp) if resp.status().is_success())
}

/// Resolve built-in agent aliases when no explicit slash/path is passed.
#[must_use]
pub fn resolve_agent_command(agent: &str) -> &str {
    match agent.trim() {
        "claude-code" => "claude",
        other => other,
    }
}

/// Execute `headroom wrap`.
pub async fn run_wrap(args: WrapArgs) -> Result<ExitCode> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(400))
        .build()
        .context("failed to build HTTP client")?;

    let mut proxy_task = None;
    let actual_port = if args.port != 0 && probe_healthz(&client, args.port).await {
        args.port
    } else {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", args.port))
            .await
            .with_context(|| format!("failed binding local wrap proxy on port {}", args.port))?;
        let bound_port = listener.local_addr()?.port();
        let cfg = build_proxy_config(&ProxyLaunchOptions {
            host: "127.0.0.1".to_string(),
            port: bound_port,
            upstream: args.upstream.clone(),
            compression: CompressionSetting::Enabled,
            ccr: CcrSetting::Enabled,
            ccr_db_path: None,
            stats_path: None,
            log_messages: MessageLogSetting::Disabled,
            log_level: args.log_level.clone(),
        })?;

        let handle = tokio::spawn(async move {
            let _ = headroom_proxy::serve_on_listener(listener, cfg).await;
        });

        let mut ready = false;
        for _ in 0..200 {
            if probe_healthz(&client, bound_port).await {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if !ready {
            handle.abort();
            anyhow::bail!("wrapped proxy failed to become healthy on 127.0.0.1:{bound_port}");
        }
        proxy_task = Some(handle);
        bound_port
    };

    let pid = std::process::id();
    let sessions_dir = wrap_sessions_dir()?;
    std::fs::create_dir_all(&sessions_dir)
        .with_context(|| format!("failed creating {}", sessions_dir.display()))?;
    let session_file = sessions_dir.join(format!("{pid}.json"));
    let started_at =
        humantime::format_rfc3339_seconds(SystemTime::now().max(SystemTime::UNIX_EPOCH))
            .to_string();
    let record = WrapSessionRecord {
        pid,
        port: actual_port,
        agent: args.agent.clone(),
        started_at,
    };
    std::fs::write(&session_file, serde_json::to_string_pretty(&record)?)
        .with_context(|| format!("failed writing {}", session_file.display()))?;

    let _guard = WrapSessionGuard {
        session_file: Some(session_file),
        proxy_task,
    };

    let proxy_base = format!("http://127.0.0.1:{actual_port}");
    let bin = resolve_agent_command(&args.agent);
    let mut cmd = Command::new(bin);
    cmd.args(&args.args)
        .env("ANTHROPIC_BASE_URL", &proxy_base)
        .env("OPENAI_BASE_URL", format!("{proxy_base}/v1"))
        .env("HEADROOM_PROXY_URL", &proxy_base);
    if args.no_rtk {
        cmd.env("HEADROOM_NO_RTK", "1");
    }

    let status = cmd
        .status()
        .await
        .with_context(|| format!("failed launching agent command {bin:?}"))?;

    let code = status
        .code()
        .and_then(|c| u8::try_from(c).ok())
        .unwrap_or(1);
    Ok(ExitCode::from(code))
}

/// Execute `headroom unwrap`.
pub fn run_unwrap(_args: UnwrapArgs) -> Result<()> {
    let dir = wrap_sessions_dir()?;
    let mut removed = 0usize;
    if dir.exists() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json")
                && std::fs::remove_file(&path).is_ok()
            {
                removed += 1;
            }
        }
    }
    println!(
        "Cleaned {removed} headroom wrap session file(s) from {}",
        dir.display()
    );
    println!("If any shell exports remain active, run:");
    println!("  unset ANTHROPIC_BASE_URL OPENAI_BASE_URL HEADROOM_PROXY_URL");
    Ok(())
}
