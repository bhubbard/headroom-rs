//! `headroom doctor` — diagnostics for workspace, proxy, CCR storage, MCP, and URL security.

use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use clap::Args;
use headroom_core::ccr::backends::{from_config, CcrBackendConfig};
use headroom_mcp::{check_status, AgentTarget};
use serde_json::{json, Value};

use crate::workspace::{default_ccr_db_path, effective_home_dir, workspace_dir};

/// Arguments for `headroom doctor`.
#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Proxy port to probe via `/healthz`.
    #[arg(long, default_value_t = 8787)]
    pub port: u16,

    /// Emit structured JSON report.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// Severity outcome of an individual diagnostic check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckLevel {
    Pass,
    Warn,
    Fail,
}

impl CheckLevel {
    fn label(self) -> &'static str {
        match self {
            CheckLevel::Pass => "PASS",
            CheckLevel::Warn => "WARN",
            CheckLevel::Fail => "FAIL",
        }
    }
}

/// Strictly validate an `ANTHROPIC_BASE_URL` or `OPENAI_BASE_URL` override.
///
/// Rejects embedded credentials (`http://127.0.0.1@evil.com`), spoofed domains
/// (`http://localhost.evil.com`), and unsupported URL schemes.
pub fn validate_base_url_safety(raw: &str) -> Result<CheckLevel, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(CheckLevel::Pass);
    }
    let parsed = url::Url::parse(trimmed).map_err(|e| format!("invalid URL {trimmed:?}: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "unsupported scheme {other:?} in {trimmed:?}; expected http or https"
            ))
        }
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(format!(
            "base URL {trimmed:?} contains embedded userinfo/credentials (possible spoofing)"
        ));
    }
    let Some(host) = parsed.host_str() else {
        return Err(format!("base URL {trimmed:?} is missing a host"));
    };
    let lower = host.to_ascii_lowercase();
    let allowed = matches!(
        lower.as_str(),
        "127.0.0.1" | "localhost" | "::1" | "[::1]" | "api.anthropic.com" | "api.openai.com"
    );
    if !allowed {
        return Err(format!(
            "untrusted host {host:?} in base URL {trimmed:?} (expected loopback or official API host)"
        ));
    }
    Ok(CheckLevel::Pass)
}

#[derive(Debug, Clone)]
struct CheckReport {
    name: &'static str,
    level: CheckLevel,
    detail: String,
}

fn check_workspace() -> CheckReport {
    match workspace_dir() {
        Ok(dir) => {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return CheckReport {
                    name: "workspace_dir",
                    level: CheckLevel::Fail,
                    detail: format!("cannot create {}: {e}", dir.display()),
                };
            }
            let probe = dir.join(format!(".doctor_probe_{}", std::process::id()));
            match std::fs::write(&probe, b"ok") {
                Ok(()) => {
                    let _ = std::fs::remove_file(&probe);
                    CheckReport {
                        name: "workspace_dir",
                        level: CheckLevel::Pass,
                        detail: format!("writable ({})", dir.display()),
                    }
                }
                Err(e) => CheckReport {
                    name: "workspace_dir",
                    level: CheckLevel::Fail,
                    detail: format!("not writable ({}): {e}", dir.display()),
                },
            }
        }
        Err(e) => CheckReport {
            name: "workspace_dir",
            level: CheckLevel::Fail,
            detail: e.to_string(),
        },
    }
}

async fn check_proxy(port: u16) -> CheckReport {
    let url = format!("http://127.0.0.1:{port}/healthz");
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return CheckReport {
                name: "proxy_health",
                level: CheckLevel::Warn,
                detail: format!("HTTP client error: {e}"),
            };
        }
    };
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => CheckReport {
            name: "proxy_health",
            level: CheckLevel::Pass,
            detail: format!("healthy at {url}"),
        },
        Ok(resp) => CheckReport {
            name: "proxy_health",
            level: CheckLevel::Warn,
            detail: format!("{url} returned HTTP {}", resp.status()),
        },
        Err(_) => CheckReport {
            name: "proxy_health",
            level: CheckLevel::Warn,
            detail: format!("proxy not reachable at {url} (start with `headroom proxy`)"),
        },
    }
}

fn check_env_urls() -> CheckReport {
    let vars = ["ANTHROPIC_BASE_URL", "OPENAI_BASE_URL"];
    let mut checked = Vec::new();
    for var in vars {
        if let Ok(val) = std::env::var(var) {
            if let Err(err) = validate_base_url_safety(&val) {
                return CheckReport {
                    name: "env_base_urls",
                    level: CheckLevel::Fail,
                    detail: format!("{var}: {err}"),
                };
            }
            if !val.trim().is_empty() {
                checked.push(format!("{var}={val}"));
            }
        }
    }
    if checked.is_empty() {
        CheckReport {
            name: "env_base_urls",
            level: CheckLevel::Pass,
            detail: "no override base URLs set in environment".to_string(),
        }
    } else {
        CheckReport {
            name: "env_base_urls",
            level: CheckLevel::Pass,
            detail: format!("safe ({})", checked.join(", ")),
        }
    }
}

fn check_ccr_db() -> CheckReport {
    let Some(path) = default_ccr_db_path() else {
        return CheckReport {
            name: "ccr_sqlite",
            level: CheckLevel::Fail,
            detail: "unable to determine CCR DB path".to_string(),
        };
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return CheckReport {
                name: "ccr_sqlite",
                level: CheckLevel::Fail,
                detail: format!("failed creating {}: {e}", parent.display()),
            };
        }
    }
    match from_config(&CcrBackendConfig::sqlite_default(path.clone())) {
        Ok(store) => CheckReport {
            name: "ccr_sqlite",
            level: CheckLevel::Pass,
            detail: format!("ready ({}, {} entries)", path.display(), store.len()),
        },
        Err(e) => CheckReport {
            name: "ccr_sqlite",
            level: CheckLevel::Fail,
            detail: format!("SQLite init error ({}): {e}", path.display()),
        },
    }
}

fn check_mcp_registrations() -> CheckReport {
    let Some(home) = effective_home_dir() else {
        return CheckReport {
            name: "mcp_registrations",
            level: CheckLevel::Warn,
            detail: "home directory unresolvable".to_string(),
        };
    };
    let statuses = check_status(&home, &AgentTarget::ALL, "headroom");
    let registered: Vec<&str> = statuses
        .iter()
        .filter(|s| s.registered)
        .map(|s| s.target.as_str())
        .collect();
    if registered.is_empty() {
        CheckReport {
            name: "mcp_registrations",
            level: CheckLevel::Warn,
            detail: "no agents registered (run `headroom mcp install`)".to_string(),
        }
    } else {
        CheckReport {
            name: "mcp_registrations",
            level: CheckLevel::Pass,
            detail: format!("registered for: {}", registered.join(", ")),
        }
    }
}

/// Execute `headroom doctor`, returning `0` (all pass), `1` (warnings), or `2` (failures).
pub async fn run(args: DoctorArgs) -> Result<ExitCode> {
    let checks = vec![
        check_workspace(),
        check_proxy(args.port).await,
        check_env_urls(),
        check_ccr_db(),
        check_mcp_registrations(),
    ];

    let has_fail = checks.iter().any(|c| c.level == CheckLevel::Fail);
    let has_warn = checks.iter().any(|c| c.level == CheckLevel::Warn);
    let code = if has_fail {
        ExitCode::from(2)
    } else if has_warn {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    };

    if args.json {
        let rows: Vec<Value> = checks
            .iter()
            .map(|c| {
                json!({
                    "check": c.name,
                    "status": c.level.label(),
                    "detail": c.detail,
                })
            })
            .collect();
        let report = json!({
            "ok": !has_fail,
            "checks": rows,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for c in &checks {
            println!("[{:<4}] {:<18} {}", c.level.label(), c.name, c.detail);
        }
    }

    Ok(code)
}
