//! Error types for `headroom-mcp`.

use thiserror::Error;

/// Errors surfaced by the Headroom MCP server, memory store, or agent registrars.
#[derive(Debug, Error)]
pub enum McpError {
    /// I/O error reading/writing stdio or configuration files.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// SQLite database error in memory or CCR storage.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// JSON serialization or parsing error.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// TOML serialization or parsing error.
    #[error("toml error: {0}")]
    Toml(String),
    /// Compression failure inside `headroom-core`.
    #[error("compression error: {0}")]
    Compression(#[from] headroom_core::CompressError),
    /// Invalid user/tool input parameter.
    #[error("invalid argument: {0}")]
    InvalidInput(String),
}
