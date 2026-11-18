//! `headroom-cli` library: unified CLI entrypoints and testable helpers.

#![forbid(unsafe_code)]

pub mod compress;
pub mod doctor;
pub mod inspect;
pub mod mcp;
pub mod proxy;
pub mod savings;
pub mod workspace;
pub mod wrap;

use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Pure-Rust Headroom CLI: context compression, reverse proxy, MCP server, and diagnostics.
#[derive(Debug, Parser)]
#[command(name = "headroom", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

/// Top-level `headroom` subcommands.
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Compress raw text, logs, CSV, configs, diffs, or JSON chat messages.
    Compress(compress::CompressArgs),
    /// Run the transparent LLM reverse proxy with compression and CCR enabled.
    Proxy(proxy::ProxyArgs),
    /// Run any coding agent (`claude`, `codex`, `cursor`, ...) behind a local Headroom proxy.
    Wrap(wrap::WrapArgs),
    /// Remove leftover wrap session markers and show shell unset instructions.
    Unwrap(wrap::UnwrapArgs),
    /// Manage or run the native Headroom MCP stdio server and IDE/CLI registrars.
    Mcp {
        #[command(subcommand)]
        command: mcp::McpCommand,
    },
    /// Run installation, proxy, storage, MCP, and security diagnostics.
    Doctor(doctor::DoctorArgs),
    /// Display lifetime/session token savings and cost reductions.
    Savings(savings::SavingsArgs),
    /// Render unified diffs of recent proxy transformations or a local input file.
    Inspect(inspect::InspectArgs),
}

/// Dispatch parsed CLI arguments and return process exit code.
pub async fn dispatch(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Commands::Compress(args) => {
            compress::run(args)?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::Proxy(args) => proxy::run(args).await,
        Commands::Wrap(args) => wrap::run_wrap(args).await,
        Commands::Unwrap(args) => {
            wrap::run_unwrap(args)?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::Mcp { command } => {
            mcp::run(command).await?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::Doctor(args) => doctor::run(args).await,
        Commands::Savings(args) => {
            savings::run(args).await?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::Inspect(args) => {
            inspect::run(args).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}
