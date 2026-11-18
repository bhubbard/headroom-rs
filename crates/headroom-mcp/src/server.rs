//! JSON-RPC 2.0 stdio MCP server exposing Headroom compression, CCR retrieval,
//! session statistics, and SQLite FTS5 memory tools.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use headroom_core::ccr::backends::{from_config, CcrBackendConfig};
use headroom_core::ccr::{CcrStore, InMemoryCcrStore};
use headroom_core::{compress_text, CompressConfig, SdkMode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::error::McpError;
use crate::memory::MemoryStore;
use crate::protocol::{CallToolResult, JsonRpcRequest, JsonRpcResponse, Tool};

/// Runtime configuration for [`McpServer`].
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    /// Local Headroom proxy URL used for fallback retrieval and live `/stats`.
    pub proxy_url: String,
    /// Path to persistent SQLite CCR database (defaults to `~/.headroom/ccr.db`).
    pub ccr_db_path: Option<PathBuf>,
    /// Path to persistent SQLite memory database (defaults to `~/.headroom/memory.db`).
    pub memory_db_path: Option<PathBuf>,
    /// Keep all CCR and memory state strictly in-memory.
    pub stateless: bool,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        let proxy_url = std::env::var("HEADROOM_PROXY_URL")
            .ok()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| "http://127.0.0.1:8787".to_string());
        let stateless = std::env::var("HEADROOM_STATELESS")
            .ok()
            .is_some_and(|v| matches!(v.trim(), "1" | "true" | "yes"));
        let base_dir = headroom_home_dir();
        Self {
            proxy_url,
            ccr_db_path: base_dir.as_ref().map(|d| d.join("ccr.db")),
            memory_db_path: base_dir.as_ref().map(|d| d.join("memory.db")),
            stateless,
        }
    }
}

/// In-process session telemetry tracked by [`McpServer`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpSessionStats {
    pub compressions: usize,
    pub retrievals: usize,
    pub total_input_tokens: usize,
    pub total_output_tokens: usize,
    pub total_tokens_saved: usize,
}

/// Pure-Rust MCP server communicating over stdio newline-delimited JSON-RPC 2.0.
pub struct McpServer {
    config: McpServerConfig,
    ccr_store: Arc<dyn CcrStore>,
    memory: MemoryStore,
    stats: McpSessionStats,
    http: reqwest::Client,
}

impl McpServer {
    /// Construct a new [`McpServer`] using `config`.
    ///
    /// # Errors
    /// Returns [`McpError`] if SQLite CCR/memory initialization fails.
    pub fn new(config: McpServerConfig) -> Result<Self, McpError> {
        let ccr_store: Arc<dyn CcrStore> = if config.stateless {
            Arc::new(InMemoryCcrStore::new())
        } else if let Some(ref path) = config.ccr_db_path {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            match from_config(&CcrBackendConfig::sqlite_default(path.clone())) {
                Ok(store) => Arc::from(store),
                Err(e) => {
                    tracing::warn!(error = %e, "falling back to in-memory CCR store");
                    Arc::new(InMemoryCcrStore::new())
                }
            }
        } else {
            Arc::new(InMemoryCcrStore::new())
        };

        let memory = if config.stateless {
            MemoryStore::in_memory()?
        } else if let Some(ref path) = config.memory_db_path {
            MemoryStore::open(path)?
        } else {
            MemoryStore::in_memory()?
        };

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .map_err(|e| McpError::InvalidInput(e.to_string()))?;

        Ok(Self {
            config,
            ccr_store,
            memory,
            stats: McpSessionStats::default(),
            http,
        })
    }

    /// Construct a server with an explicitly supplied [`CcrStore`] (useful for tests/embedded mode).
    ///
    /// # Errors
    /// Returns [`McpError`] if in-memory SQLite fails to initialize.
    pub fn with_stores(
        config: McpServerConfig,
        ccr_store: Arc<dyn CcrStore>,
        memory: MemoryStore,
    ) -> Result<Self, McpError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .map_err(|e| McpError::InvalidInput(e.to_string()))?;
        Ok(Self {
            config,
            ccr_store,
            memory,
            stats: McpSessionStats::default(),
            http,
        })
    }

    /// Read-only view of session stats.
    #[must_use]
    pub fn stats(&self) -> &McpSessionStats {
        &self.stats
    }

    /// Serve MCP requests line-by-line over stdin/stdout until EOF.
    ///
    /// # Errors
    /// Returns [`McpError::Io`] on stream write failures.
    pub async fn run_stdio(&mut self) -> Result<(), McpError> {
        let stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let mut lines = BufReader::new(stdin).lines();

        while let Some(line) = lines.next_line().await? {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<JsonRpcRequest>(trimmed) {
                Ok(req) => {
                    if let Some(resp) = self.handle_request(req).await {
                        let encoded = serde_json::to_string(&resp)?;
                        stdout.write_all(encoded.as_bytes()).await?;
                        stdout.write_all(b"\n").await?;
                        stdout.flush().await?;
                    }
                }
                Err(e) => {
                    let resp = JsonRpcResponse::error(None, -32700, format!("Parse error: {e}"));
                    let encoded = serde_json::to_string(&resp)?;
                    stdout.write_all(encoded.as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                    stdout.flush().await?;
                }
            }
        }
        Ok(())
    }

    /// Handle one decoded JSON-RPC request; returns `None` for notifications.
    pub async fn handle_request(&mut self, req: JsonRpcRequest) -> Option<JsonRpcResponse> {
        if req.id.is_none() || req.method.starts_with("notifications/") {
            return None;
        }
        let id = req.id.clone();
        match req.method.as_str() {
            "initialize" => Some(JsonRpcResponse::success(
                id,
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": "headroom",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )),
            "ping" => Some(JsonRpcResponse::success(id, json!({}))),
            "tools/list" => Some(JsonRpcResponse::success(
                id,
                json!({ "tools": self.list_tools() }),
            )),
            "tools/call" => {
                let params = req.params.unwrap_or(Value::Null);
                let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let call_result = self.call_tool(tool_name, arguments).await;
                let value = serde_json::to_value(call_result).unwrap_or_else(|e| {
                    json!({
                        "content": [{"type": "text", "text": format!("serialization error: {e}")}],
                        "isError": true
                    })
                });
                Some(JsonRpcResponse::success(id, value))
            }
            other => Some(JsonRpcResponse::error(
                id,
                -32601,
                format!("Method not found: {other}"),
            )),
        }
    }

    /// Enumerate all MCP tools registered on this server.
    #[must_use]
    pub fn list_tools(&self) -> Vec<Tool> {
        vec![
            Tool {
                name: "headroom_compress".to_string(),
                description: "Compress large JSON, CSV/Markdown tables, logs, diffs, HTML, source code, or configs while storing originals in CCR for retrieval.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "content": { "type": "string", "description": "Payload to compress" },
                        "model": { "type": "string", "description": "Tokenizer model name (default: gpt-4o)" },
                        "context": { "type": "string", "description": "Optional query/context to preserve relevant rows" },
                        "target_ratio": { "type": "number", "description": "Optional keep ratio (0.0..1.0] for plain-text compression" },
                        "lossless_only": { "type": "boolean", "description": "Restrict to deterministic lossless densification without CCR markers" }
                    },
                    "required": ["content"]
                }),
            },
            Tool {
                name: "headroom_retrieve".to_string(),
                description: "Retrieve original uncompressed CCR content by 24-hex hash, with optional line query filter and offset/limit slice.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "hash": { "type": "string", "description": "24-hex CCR hash from <<ccr:HASH>>" },
                        "query": { "type": "string", "description": "Optional case-insensitive substring filter applied to lines" },
                        "offset": { "type": "integer", "description": "Optional character/line slice start offset" },
                        "limit": { "type": "integer", "description": "Optional max chars to return" }
                    },
                    "required": ["hash"]
                }),
            },
            Tool {
                name: "headroom_stats".to_string(),
                description: "Return Headroom MCP session compression statistics and local proxy /stats summary if running.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            Tool {
                name: "memory_add".to_string(),
                description: "Persist a memory note with optional tags and source into local SQLite FTS5 storage.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "content": { "type": "string" },
                        "tags": { "type": "array", "items": { "type": "string" } },
                        "source": { "type": "string" }
                    },
                    "required": ["content"]
                }),
            },
            Tool {
                name: "memory_search".to_string(),
                description: "Search persistent agent memories using SQLite FTS5 BM25 ranking.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "limit": { "type": "integer" }
                    },
                    "required": ["query"]
                }),
            },
            Tool {
                name: "memory_list".to_string(),
                description: "List most recent stored agent memories.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "limit": { "type": "integer" }
                    }
                }),
            },
            Tool {
                name: "memory_delete".to_string(),
                description: "Delete a stored memory record by UUID.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" }
                    },
                    "required": ["id"]
                }),
            },
        ]
    }

    async fn call_tool(&mut self, name: &str, args: Value) -> CallToolResult {
        match name {
            "headroom_compress" => self.tool_compress(args),
            "headroom_retrieve" | "ccr_retrieve" => self.tool_retrieve(args).await,
            "headroom_stats" => self.tool_stats().await,
            "memory_add" => self.tool_memory_add(args),
            "memory_search" => self.tool_memory_search(args),
            "memory_list" => self.tool_memory_list(args),
            "memory_delete" => self.tool_memory_delete(args),
            other => CallToolResult::err(format!("Unknown tool: {other}")),
        }
    }

    fn tool_compress(&mut self, args: Value) -> CallToolResult {
        let Some(content) = args.get("content").and_then(Value::as_str) else {
            return CallToolResult::err("missing required string argument `content`");
        };
        let model = args
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("gpt-4o");
        let context = args.get("context").and_then(Value::as_str).unwrap_or("");
        let target_ratio = args.get("target_ratio").and_then(Value::as_f64);
        let lossless_only = args
            .get("lossless_only")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let mut builder = CompressConfig::builder();
        builder
            .model(model)
            .context(context)
            .target_ratio(target_ratio)
            .min_tokens_to_compress(32)
            .mode(if lossless_only {
                SdkMode::AgentLossless
            } else {
                SdkMode::Standard
            });
        let cfg = match builder.build() {
            Ok(c) => c,
            Err(e) => return CallToolResult::err(e.to_string()),
        };

        let store_opt = if lossless_only {
            None
        } else {
            Some(self.ccr_store.as_ref())
        };
        match compress_text(content, &cfg, store_opt) {
            Ok(res) => {
                self.stats.compressions += 1;
                self.stats.total_input_tokens += res.original_tokens;
                self.stats.total_output_tokens += res.compressed_tokens;
                self.stats.total_tokens_saved += res.tokens_saved;
                let summary = format!(
                    "[headroom: {} -> {} tokens ({} saved, strategy={}, ccr_hash={})]",
                    res.original_tokens,
                    res.compressed_tokens,
                    res.tokens_saved,
                    res.strategy,
                    res.ccr_hash.as_deref().unwrap_or("none")
                );
                CallToolResult::ok(format!("{}\n\n{summary}", res.compressed))
            }
            Err(e) => CallToolResult::err(format!("compression failed: {e}")),
        }
    }

    async fn tool_retrieve(&mut self, args: Value) -> CallToolResult {
        let Some(raw_hash) = args.get("hash").and_then(Value::as_str) else {
            return CallToolResult::err("missing required string argument `hash`");
        };
        let hash = raw_hash.trim();
        if hash.is_empty() {
            return CallToolResult::err("`hash` cannot be empty");
        }

        let local = self.ccr_store.get(hash);
        let original = match local {
            Some(payload) => payload,
            None => match self.fetch_from_proxy_retrieve(hash).await {
                Ok(Some(payload)) => payload,
                Ok(None) => {
                    return CallToolResult::err(format!(
                        "CCR hash {hash:?} not found in local store or proxy ({})",
                        self.config.proxy_url
                    ));
                }
                Err(err) => {
                    return CallToolResult::err(format!(
                        "CCR hash {hash:?} not found locally and proxy lookup failed: {err}"
                    ));
                }
            },
        };

        self.stats.retrievals += 1;

        let filtered = if let Some(q) = args
            .get("query")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            let q_lower = q.to_ascii_lowercase();
            let matching: Vec<&str> = original
                .lines()
                .filter(|ln| ln.to_ascii_lowercase().contains(&q_lower))
                .collect();
            if matching.is_empty() {
                format!(
                    "(no lines matched query {q:?}; total {} bytes)",
                    original.len()
                )
            } else {
                matching.join("\n")
            }
        } else {
            original
        };

        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(0);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok());

        let sliced = slice_chars(&filtered, offset, limit);
        CallToolResult::ok(sliced)
    }

    async fn fetch_from_proxy_retrieve(&self, hash: &str) -> Result<Option<String>, String> {
        let base = self.config.proxy_url.trim_end_matches('/');
        let url = format!("{base}/v1/retrieve/{hash}");
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        let body: Value = resp.json().await.map_err(|e| e.to_string())?;
        Ok(body
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    async fn tool_stats(&self) -> CallToolResult {
        let base = self.config.proxy_url.trim_end_matches('/');
        let proxy_stats: Value = match self.http.get(format!("{base}/stats")).send().await {
            Ok(r) if r.status().is_success() => r
                .json::<Value>()
                .await
                .unwrap_or_else(|_| json!({"status": "parse_error"})),
            Ok(r) => json!({
                "url": self.config.proxy_url,
                "status": "error",
                "http_status": r.status().as_u16()
            }),
            Err(e) => json!({
                "url": self.config.proxy_url,
                "status": "unreachable",
                "error": e.to_string()
            }),
        };

        let payload = json!({
            "mcp_session": self.stats,
            "ccr_entries": self.ccr_store.len(),
            "proxy": proxy_stats,
        });
        CallToolResult::ok(
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string()),
        )
    }

    fn tool_memory_add(&self, args: Value) -> CallToolResult {
        let Some(content) = args.get("content").and_then(Value::as_str) else {
            return CallToolResult::err("missing required string argument `content`");
        };
        let tags: Vec<String> = args
            .get("tags")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let source = args.get("source").and_then(Value::as_str);

        match self.memory.add(content, &tags, source) {
            Ok(entry) => CallToolResult::ok(
                serde_json::to_string_pretty(&entry).unwrap_or_else(|_| entry.id.clone()),
            ),
            Err(e) => CallToolResult::err(e.to_string()),
        }
    }

    fn tool_memory_search(&self, args: Value) -> CallToolResult {
        let query = args.get("query").and_then(Value::as_str).unwrap_or("");
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(20);
        match self.memory.search(query, limit) {
            Ok(entries) => CallToolResult::ok(
                serde_json::to_string_pretty(&json!({"count": entries.len(), "memories": entries}))
                    .unwrap_or_default(),
            ),
            Err(e) => CallToolResult::err(e.to_string()),
        }
    }

    fn tool_memory_list(&self, args: Value) -> CallToolResult {
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(20);
        match self.memory.list(limit) {
            Ok(entries) => CallToolResult::ok(
                serde_json::to_string_pretty(&json!({"count": entries.len(), "memories": entries}))
                    .unwrap_or_default(),
            ),
            Err(e) => CallToolResult::err(e.to_string()),
        }
    }

    fn tool_memory_delete(&self, args: Value) -> CallToolResult {
        let Some(id) = args.get("id").and_then(Value::as_str) else {
            return CallToolResult::err("missing required string argument `id`");
        };
        match self.memory.delete(id) {
            Ok(deleted) => CallToolResult::ok(json!({"id": id, "deleted": deleted}).to_string()),
            Err(e) => CallToolResult::err(e.to_string()),
        }
    }
}

fn slice_chars(input: &str, offset: usize, limit: Option<usize>) -> String {
    let chars = input.chars().skip(offset);
    match limit {
        Some(n) => chars.take(n).collect(),
        None => chars.collect(),
    }
}

fn headroom_home_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("HEADROOM_WORKSPACE_DIR") {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    dirs::home_dir().map(|h| h.join(".headroom"))
}
