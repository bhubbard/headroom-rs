//! headroom-mcp: Pure-Rust MCP stdio server, FTS5 memory store, and agent registrars.

#![forbid(unsafe_code)]

pub mod error;
pub mod memory;
pub mod protocol;
pub mod registry;
pub mod server;

pub use error::McpError;
pub use memory::{MemoryEntry, MemoryStore};
pub use protocol::{
    CallToolResult, JsonRpcError, JsonRpcRequest, JsonRpcResponse, TextContent, Tool,
};
pub use registry::{
    check_status, install_mcp, uninstall_mcp, AgentStatus, AgentTarget, OverwritePolicy,
    RegisterResult, RegisterStatus, ServerSpec,
};
pub use server::{McpServer, McpServerConfig, McpSessionStats};
