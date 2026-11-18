# headroom-rs

Pure-Rust port of [`headroomlabs-ai/headroom`](https://github.com/headroomlabs-ai/headroom) (Apache-2.0) — a context optimization, reverse-proxy, CCR retrieval, and MCP tool suite with zero Python runtime dependencies.

## Workspace Crates

| Crate | Purpose |
| --- | --- |
| `crates/headroom-core` | Content-aware compression SDK (`compress`, `compress_with_ccr`, `compress_text`), live-zone dispatcher, content detector, `SmartCrusher`, `TextCrusher`, `LogCompressor`, `SearchCompressor`, `DiffCompressor`, `CodeAwareCompressor`, `HtmlExtractor`, `TabularCompressor`, `ConfigCompressor`, lossless compaction, recursive JSON routing, tokenizers, and pluggable CCR stores (`InMemoryCcrStore`, `SqliteCcrStore`). |
| `crates/headroom-proxy` | Transparent HTTP/SSE/WebSocket reverse proxy (`headroom-proxy` binary + library) with live-zone compression for Anthropic, OpenAI Chat/Responses, AWS Bedrock, Google Vertex, optional in-proxy CCR `/v1/retrieve`, `/transformations/feed`, `/stats`, `/dashboard`, and `/metrics`. |
| `crates/headroom-mcp` | Native stdio MCP JSON-RPC 2.0 server (`headroom_compress`, `headroom_retrieve`/`ccr_retrieve`, `headroom_stats`, FTS5 `memory_*`) and non-destructive registrars for Antigravity, Claude Code, Codex, and Cursor. |
| `crates/headroom-cli` | Unified `headroom` binary (`compress`, `proxy`, `wrap`, `unwrap`, `mcp`, `doctor`, `savings`, `inspect`). |
| `crates/headroom-simulators` | Local API simulator harnesses used by proxy integration tests. |

> **Note on optional ML feature:** `headroom-core` keeps ONNX/ML kompress support opt-in behind `--features ml` so default builds stay lightweight pure Rust.

## Build & Install

```sh
cargo build --release -p headroom-cli -p headroom-proxy
# Install the unified `headroom` CLI locally:
cargo install --path crates/headroom-cli
```

## CLI Quick Reference

```sh
# Standalone compression (files or stdin; JSON chat messages or raw text/logs/CSV/HTML)
headroom compress payload.json --json
cat build.log | headroom compress --stats

# Reverse proxy with compression, CCR storage, and savings ledger enabled
headroom proxy --port 8787 --upstream https://api.anthropic.com --log-messages

# Wrap any coding agent behind an automatic Headroom proxy
headroom wrap claude -- --model sonnet
headroom unwrap --all

# Inspect token savings and unified transformation diffs
headroom savings --by-model
headroom inspect --limit 5
headroom inspect --input build.log

# Diagnostics (workspace, proxy /healthz, URL spoofing checks, SQLite CCR, MCP status)
headroom doctor

# MCP server and IDE/CLI config registration
headroom mcp install --target all
headroom mcp status
headroom mcp uninstall --target all
headroom mcp serve
```

## Rust SDK Example

```rust
use headroom_core::sdk::{compress_text, CompressConfig, SdkMode};

let cfg = CompressConfig::builder()
    .model("gpt-4o")
    .mode(SdkMode::Standard)
    .build()?;
let result = compress_text(raw_payload, &cfg, None)?;
println!("saved {} tokens via {}", result.tokens_saved, result.strategy);
```

## Verification

```sh
make fmt
make lint
make test
make doc
make release
```

## License & Attribution

Licensed under the Apache License, Version 2.0 ([LICENSE](LICENSE), [NOTICE](NOTICE)). Derived from [`headroomlabs-ai/headroom`](https://github.com/headroomlabs-ai/headroom).
