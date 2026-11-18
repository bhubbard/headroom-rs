//! `headroom mcp` — stdio MCP server and safe IDE/CLI config registrars.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use headroom_mcp::{
    check_status, install_mcp, uninstall_mcp, AgentTarget, McpServer, McpServerConfig,
    OverwritePolicy, RegisterStatus, ServerSpec,
};
use serde_json::{json, Value};

use crate::workspace::{default_ccr_db_path, default_memory_db_path, effective_home_dir};

/// Target coding-agent config for `mcp install|uninstall|status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum McpTargetArg {
    All,
    Antigravity,
    Claude,
    Codex,
    Cursor,
}

impl McpTargetArg {
    fn into_targets(self) -> Vec<AgentTarget> {
        match self {
            McpTargetArg::All => AgentTarget::ALL.to_vec(),
            McpTargetArg::Antigravity => vec![AgentTarget::Antigravity],
            McpTargetArg::Claude => vec![AgentTarget::Claude],
            McpTargetArg::Codex => vec![AgentTarget::Codex],
            McpTargetArg::Cursor => vec![AgentTarget::Cursor],
        }
    }
}

/// Subcommands under `headroom mcp`.
#[derive(Debug, Subcommand)]
pub enum McpCommand {
    /// Start the MCP stdio JSON-RPC server (`headroom_compress`, `headroom_retrieve`, `memory_*`).
    Serve(McpServeArgs),
    /// Install Headroom MCP server entry into Antigravity / Claude / Codex / Cursor configs.
    Install(McpInstallArgs),
    /// Remove Headroom MCP server entry from agent configs without touching unrelated keys.
    Uninstall(McpUninstallArgs),
    /// Inspect MCP registration status across installed coding agents.
    Status(McpStatusArgs),
}

/// Arguments for `headroom mcp serve`.
#[derive(Debug, Args)]
pub struct McpServeArgs {
    /// Fallback Headroom proxy base URL used if local CCR hash misses.
    #[arg(long, default_value = "http://127.0.0.1:8787")]
    pub proxy_url: String,

    /// Override SQLite database path for FTS5 agent memory.
    #[arg(long)]
    pub memory_db: Option<PathBuf>,

    /// Override SQLite database path for CCR retrieval store.
    #[arg(long)]
    pub ccr_db: Option<PathBuf>,

    /// Keep all CCR and memory stores strictly in-memory for this process.
    #[arg(long, default_value_t = false)]
    pub stateless: bool,
}

/// Arguments for `headroom mcp install`.
#[derive(Debug, Args)]
pub struct McpInstallArgs {
    /// Target agent configuration to update.
    #[arg(long, value_enum, default_value_t = McpTargetArg::All)]
    pub target: McpTargetArg,

    /// Proxy URL written into the registered `HEADROOM_PROXY_URL` env block.
    #[arg(long, default_value = "http://127.0.0.1:8787")]
    pub proxy_url: String,

    /// Overwrite an existing differing `headroom` entry instead of reporting Mismatch.
    #[arg(long, default_value_t = false)]
    pub force: bool,

    /// Emit JSON array instead of text table.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// Arguments for `headroom mcp uninstall`.
#[derive(Debug, Args)]
pub struct McpUninstallArgs {
    /// Target agent configuration to remove from.
    #[arg(long, value_enum, default_value_t = McpTargetArg::All)]
    pub target: McpTargetArg,

    /// Emit JSON array instead of text table.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// Arguments for `headroom mcp status`.
#[derive(Debug, Args)]
pub struct McpStatusArgs {
    /// Target agent configuration to check.
    #[arg(long, value_enum, default_value_t = McpTargetArg::All)]
    pub target: McpTargetArg,

    /// Emit JSON array instead of text table.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

fn register_status_str(status: RegisterStatus) -> &'static str {
    match status {
        RegisterStatus::Registered => "registered",
        RegisterStatus::Updated => "updated",
        RegisterStatus::Already => "already_registered",
        RegisterStatus::Mismatch => "mismatch",
        RegisterStatus::Removed => "removed",
        RegisterStatus::NotPresent => "not_present",
        RegisterStatus::Failed => "failed",
    }
}

/// Execute `headroom mcp ...`.
pub async fn run(cmd: McpCommand) -> Result<()> {
    match cmd {
        McpCommand::Serve(args) => {
            let cfg = McpServerConfig {
                proxy_url: args.proxy_url,
                ccr_db_path: args.ccr_db.or_else(default_ccr_db_path),
                memory_db_path: args.memory_db.or_else(default_memory_db_path),
                stateless: args.stateless,
            };
            let mut server = McpServer::new(cfg).context("failed to initialize MCP server")?;
            server.run_stdio().await.context("MCP stdio server error")?;
        }
        McpCommand::Install(args) => {
            let home = effective_home_dir().context("cannot determine home directory")?;
            let exe = std::env::current_exe().context("cannot resolve current executable")?;
            let spec = ServerSpec::headroom(exe.display().to_string(), args.proxy_url);
            let policy = if args.force {
                OverwritePolicy::Force
            } else {
                OverwritePolicy::PreserveExisting
            };
            let results = install_mcp(&home, &args.target.into_targets(), &spec, policy);
            if args.json {
                let rows: Vec<Value> = results
                    .iter()
                    .map(|r| {
                        json!({
                            "target": r.target.as_str(),
                            "display_name": r.target.display_name(),
                            "status": register_status_str(r.status),
                            "config_path": r.config_path,
                            "detail": r.detail,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                for r in results {
                    println!(
                        "{:<14} {:<18} {} ({})",
                        r.target.as_str(),
                        register_status_str(r.status),
                        r.detail,
                        r.config_path.display()
                    );
                }
            }
        }
        McpCommand::Uninstall(args) => {
            let home = effective_home_dir().context("cannot determine home directory")?;
            let results = uninstall_mcp(&home, &args.target.into_targets(), "headroom");
            if args.json {
                let rows: Vec<Value> = results
                    .iter()
                    .map(|r| {
                        json!({
                            "target": r.target.as_str(),
                            "status": register_status_str(r.status),
                            "config_path": r.config_path,
                            "detail": r.detail,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                for r in results {
                    println!(
                        "{:<14} {:<18} {}",
                        r.target.as_str(),
                        register_status_str(r.status),
                        r.detail
                    );
                }
            }
        }
        McpCommand::Status(args) => {
            let home = effective_home_dir().context("cannot determine home directory")?;
            let statuses = check_status(&home, &args.target.into_targets(), "headroom");
            if args.json {
                let rows: Vec<Value> = statuses
                    .iter()
                    .map(|s| {
                        json!({
                            "target": s.target.as_str(),
                            "display_name": s.target.display_name(),
                            "detected": s.detected,
                            "registered": s.registered,
                            "config_path": s.config_path,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                for s in statuses {
                    println!(
                        "{:<14} detected={:<5} registered={:<5} {}",
                        s.target.as_str(),
                        s.detected,
                        s.registered,
                        s.config_path.display()
                    );
                }
            }
        }
    }
    Ok(())
}
