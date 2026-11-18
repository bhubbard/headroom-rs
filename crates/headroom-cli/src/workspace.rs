//! Workspace and home-directory resolution shared across CLI subcommands.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Resolve the user's home directory, respecting test-time `HOME` / `USERPROFILE` overrides.
#[must_use]
pub fn effective_home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .or_else(|| {
            std::env::var("USERPROFILE")
                .ok()
                .filter(|h| !h.trim().is_empty())
        })
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
}

/// Resolve Headroom's workspace directory (`$HEADROOM_WORKSPACE_DIR` or `~/.headroom`).
pub fn workspace_dir() -> Result<PathBuf> {
    if let Ok(raw) = std::env::var("HEADROOM_WORKSPACE_DIR") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            if let Some(rest) = trimmed.strip_prefix("~/") {
                let home =
                    effective_home_dir().context("cannot expand '~/' in HEADROOM_WORKSPACE_DIR")?;
                return Ok(home.join(rest));
            }
            if trimmed == "~" {
                return effective_home_dir().context("cannot expand '~' in HEADROOM_WORKSPACE_DIR");
            }
            return Ok(PathBuf::from(trimmed));
        }
    }
    let home = effective_home_dir().context("unable to resolve home directory")?;
    Ok(home.join(".headroom"))
}

/// Default savings ledger file path (`<workspace>/native_stats.json`).
#[must_use]
pub fn default_stats_path() -> Option<PathBuf> {
    workspace_dir().ok().map(|d| d.join("native_stats.json"))
}

/// Default CCR SQLite store path (`<workspace>/ccr.db`).
#[must_use]
pub fn default_ccr_db_path() -> Option<PathBuf> {
    workspace_dir().ok().map(|d| d.join("ccr.db"))
}

/// Default MCP memory SQLite store path (`<workspace>/memory.db`).
#[must_use]
pub fn default_memory_db_path() -> Option<PathBuf> {
    workspace_dir().ok().map(|d| d.join("memory.db"))
}

/// Directory storing active `headroom wrap` session markers (`<workspace>/wrap`).
pub fn wrap_sessions_dir() -> Result<PathBuf> {
    Ok(workspace_dir()?.join("wrap"))
}
