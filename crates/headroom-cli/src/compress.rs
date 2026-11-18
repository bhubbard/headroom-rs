//! `headroom compress` — standalone CLI compression over files or stdin.

use std::io::Read as _;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};
use headroom_core::sdk::{compress, compress_text, CompressConfig, SdkMode};
use headroom_core::tokenizer::get_tokenizer;
use serde_json::{json, Value};

/// Compression mode accepted by `headroom compress --mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CliCompressMode {
    /// Full content-aware compression pipeline.
    Auto,
    /// Deterministic lossless-only compaction without CCR markers.
    Lossless,
    /// Disable compression and measure token counts only.
    Passthrough,
}

/// Arguments for `headroom compress`.
#[derive(Debug, Args)]
pub struct CompressArgs {
    /// File path to compress (reads stdin when omitted or `-`).
    pub input: Option<PathBuf>,

    /// Target model used for tokenizer selection.
    #[arg(long, default_value = "gpt-4o")]
    pub model: String,

    /// Compression mode.
    #[arg(long, value_enum, default_value_t = CliCompressMode::Auto)]
    pub mode: CliCompressMode,

    /// Extractive prose keep ratio in `(0.0, 1.0]` when compressing plain text.
    #[arg(long)]
    pub target_ratio: Option<f64>,

    /// Optional relevance query context.
    #[arg(long)]
    pub context: Option<String>,

    /// Print token-savings statistics to stderr.
    #[arg(long, default_value_t = false)]
    pub stats: bool,

    /// Emit structured JSON summary (`compressed`, `tokens_before`, `tokens_after`, ...).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// Outcome of compressing raw text or chat-message JSON.
#[derive(Debug, Clone)]
pub struct LocalCompressionOutput {
    pub compressed: String,
    pub tokens_before: usize,
    pub tokens_after: usize,
    pub tokens_saved: usize,
    pub compression_ratio: f64,
    pub strategy: String,
}

/// Compress raw text or OpenAI/Anthropic JSON message payloads locally in-process.
pub fn compress_payload(
    raw: &str,
    model: &str,
    mode: CliCompressMode,
    target_ratio: Option<f64>,
    context: Option<&str>,
) -> Result<LocalCompressionOutput> {
    if mode == CliCompressMode::Passthrough {
        let tok = get_tokenizer(model);
        let tokens = tok.count_text(raw);
        return Ok(LocalCompressionOutput {
            compressed: raw.to_string(),
            tokens_before: tokens,
            tokens_after: tokens,
            tokens_saved: 0,
            compression_ratio: 1.0,
            strategy: "passthrough".to_string(),
        });
    }

    let sdk_mode = match mode {
        CliCompressMode::Auto => SdkMode::Standard,
        CliCompressMode::Lossless => SdkMode::AgentLossless,
        CliCompressMode::Passthrough => SdkMode::Standard,
    };

    let mut builder = CompressConfig::builder();
    builder
        .model(model)
        .mode(sdk_mode)
        .min_tokens_to_compress(32)
        .target_ratio(target_ratio);
    if let Some(ctx) = context {
        builder.context(ctx);
    }
    let cfg = builder
        .build()
        .context("invalid compression configuration")?;

    let trimmed = raw.trim();
    if trimmed.starts_with('[') || trimmed.starts_with('{') {
        if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
            match parsed {
                Value::Array(ref msgs)
                    if msgs
                        .iter()
                        .all(|m| m.is_object() && m.get("role").is_some()) =>
                {
                    let res = compress(msgs, &cfg).context("failed to compress messages")?;
                    let out = serde_json::to_string_pretty(&res.messages)
                        .context("failed to serialize compressed messages")?;
                    return Ok(LocalCompressionOutput {
                        compressed: out,
                        tokens_before: res.tokens_before,
                        tokens_after: res.tokens_after,
                        tokens_saved: res.tokens_saved,
                        compression_ratio: res.compression_ratio,
                        strategy: if res.transforms_applied.is_empty() {
                            "messages".to_string()
                        } else {
                            res.transforms_applied.join(",")
                        },
                    });
                }
                Value::Object(mut obj) => {
                    if let Some(msgs) = obj
                        .get("messages")
                        .and_then(Value::as_array)
                        .filter(|arr| arr.iter().all(|m| m.is_object() && m.get("role").is_some()))
                        .cloned()
                    {
                        let res = compress(&msgs, &cfg).context("failed to compress messages")?;
                        obj.insert("messages".to_string(), Value::Array(res.messages));
                        let out = serde_json::to_string_pretty(&Value::Object(obj))
                            .context("failed to serialize compressed payload")?;
                        return Ok(LocalCompressionOutput {
                            compressed: out,
                            tokens_before: res.tokens_before,
                            tokens_after: res.tokens_after,
                            tokens_saved: res.tokens_saved,
                            compression_ratio: res.compression_ratio,
                            strategy: if res.transforms_applied.is_empty() {
                                "messages".to_string()
                            } else {
                                res.transforms_applied.join(",")
                            },
                        });
                    }
                }
                _ => {}
            }
        }
    }

    let res = compress_text(raw, &cfg, None).context("failed to compress text")?;
    Ok(LocalCompressionOutput {
        compressed: res.compressed,
        tokens_before: res.original_tokens,
        tokens_after: res.compressed_tokens,
        tokens_saved: res.tokens_saved,
        compression_ratio: res.compression_ratio,
        strategy: res.strategy,
    })
}

/// Execute `headroom compress`.
pub fn run(args: CompressArgs) -> Result<()> {
    let raw = match args.input {
        Some(ref path) if path.as_os_str() != "-" => std::fs::read_to_string(path)
            .with_context(|| format!("failed reading {}", path.display()))?,
        _ => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("failed reading stdin")?;
            buf
        }
    };

    let out = compress_payload(
        &raw,
        &args.model,
        args.mode,
        args.target_ratio,
        args.context.as_deref(),
    )?;

    if args.stats {
        let pct = if out.tokens_before == 0 {
            0.0
        } else {
            (out.tokens_saved as f64 / out.tokens_before as f64) * 100.0
        };
        eprintln!(
            "headroom compress: {} -> {} tokens ({} saved, {pct:.1}%, strategy={})",
            out.tokens_before, out.tokens_after, out.tokens_saved, out.strategy
        );
    }

    if args.json {
        let payload = json!({
            "compressed": out.compressed,
            "tokens_before": out.tokens_before,
            "tokens_after": out.tokens_after,
            "tokens_saved": out.tokens_saved,
            "compression_ratio": out.compression_ratio,
            "strategy": out.strategy,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!("{}", out.compressed);
    }

    Ok(())
}
