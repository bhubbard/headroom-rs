//! High-level library SDK for compressing raw text and LLM message arrays.
//!
//! Port of `headroom/compress.py`. Routes inputs through Headroom's content-aware
//! compression pipeline (`SmartCrusher`, `TabularCompressor`, `HtmlExtractor`,
//! `DiffCompressor`, `LogCompressor`, `SearchCompressor`, `CodeAwareCompressor`,
//! `ConfigCompressor`, `recursive_json`, `lossless_compaction`, and `TextCrusher`)
//! with strict tokenizer-verified token reduction and optional CCR stashing.

use std::sync::Arc;

use serde_json::Value;
use thiserror::Error;

use crate::ccr::{compute_key, marker_for, CcrStore};
use crate::tokenizer::{get_tokenizer, Tokenizer};
use crate::transforms::code_compressor::{CodeAwareCompressor, CodeCompressorConfig};
use crate::transforms::config_compressor::{
    detect_config_flavor, CcrPolicy, ConfigCompressor, ConfigCompressorConfig, ConfigFlavor,
    SchemaFoldPolicy,
};
use crate::transforms::content_detector::{detect_content_type, ContentType};
use crate::transforms::diff_compressor::{DiffCompressor, DiffCompressorConfig};
use crate::transforms::html_extractor::{HtmlExtractor, HtmlExtractorConfig};
use crate::transforms::log_compressor::{LogCompressor, LogCompressorConfig};
use crate::transforms::lossless_compaction::{compact_lossless, LosslessKind};
use crate::transforms::pipeline::JsonMinifier;
use crate::transforms::recursive_json::route_embedded_json;
use crate::transforms::search_compressor::{SearchCompressor, SearchCompressorConfig};
use crate::transforms::smart_crusher::{SmartCrusher, SmartCrusherBuilder, SmartCrusherConfig};
use crate::transforms::tabular_compressor::{TabularCompressor, TabularCompressorConfig};
use crate::transforms::text_crusher::{TextCrusher, TextCrusherConfig};

/// Controls whether user-role messages are compressed by [`compress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UserMessagePolicy {
    /// Leave user messages untouched (default for coding agents).
    #[default]
    Skip,
    /// Compress eligible user messages as well (document/RAG pipelines).
    Compress,
}

/// Controls whether system-role messages are compressed by [`compress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SystemMessagePolicy {
    /// Leave system prompts untouched (cache-safe default).
    #[default]
    Preserve,
    /// Allow compression of large system messages.
    Compress,
}

/// Compression aggressiveness and reversibility regime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SdkMode {
    /// Full content-aware compression with optional CCR markers.
    #[default]
    Standard,
    /// Deterministic lossless-only densification with no CCR markers or lossy drops.
    AgentLossless,
}

/// Errors returned by [`compress`], [`compress_with_ccr`], or [`compress_text`].
#[derive(Debug, Error)]
pub enum CompressError {
    /// Provided `target_ratio` was outside `(0.0, 1.0]`.
    #[error("invalid target_ratio {0}; must be finite and in (0.0, 1.0]")]
    InvalidTargetRatio(f64),
}

/// Options governing [`compress`], [`compress_with_ccr`], and [`compress_text`].
#[derive(Debug, Clone, PartialEq)]
pub struct CompressConfig {
    /// Model name used to select the tokenizer (`"gpt-4o"`, `"claude-sonnet-4-5"`, ...).
    pub model: String,
    /// Policy for user-role messages.
    pub user_messages: UserMessagePolicy,
    /// Policy for system-role messages.
    pub system_messages: SystemMessagePolicy,
    /// Standard (lossy + CCR) vs AgentLossless (reversible/deterministic only).
    pub mode: SdkMode,
    /// Number of trailing messages whose source code blocks stay protected.
    pub protect_recent: usize,
    /// Leading cached prefix count that must remain byte-untouched.
    pub frozen_message_count: usize,
    /// Optional keep ratio in `(0.0, 1.0]` enabling extractive [`TextCrusher`] on plain text.
    pub target_ratio: Option<f64>,
    /// Minimum tokens in a payload before attempting compression.
    pub min_tokens_to_compress: usize,
    /// Optional user query context for relevance-aware selection.
    pub context: String,
}

impl Default for CompressConfig {
    fn default() -> Self {
        Self {
            model: "gpt-4o".to_string(),
            user_messages: UserMessagePolicy::Skip,
            system_messages: SystemMessagePolicy::Preserve,
            mode: SdkMode::Standard,
            protect_recent: 4,
            frozen_message_count: 0,
            target_ratio: None,
            min_tokens_to_compress: 64,
            context: String::new(),
        }
    }
}

impl CompressConfig {
    /// Create a default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a builder initialized from defaults.
    #[must_use]
    pub fn builder() -> CompressConfigBuilder {
        CompressConfigBuilder::default()
    }

    fn validate(&self) -> Result<(), CompressError> {
        if let Some(ratio) = self.target_ratio {
            if !ratio.is_finite() || ratio <= 0.0 || ratio > 1.0 {
                return Err(CompressError::InvalidTargetRatio(ratio));
            }
        }
        Ok(())
    }
}

/// Builder for [`CompressConfig`].
#[derive(Debug, Clone, Default)]
pub struct CompressConfigBuilder {
    inner: CompressConfig,
}

impl CompressConfigBuilder {
    /// Target model identifier for token counting.
    pub fn model(&mut self, model: impl Into<String>) -> &mut Self {
        self.inner.model = model.into();
        self
    }

    /// Policy for user messages.
    pub fn user_messages(&mut self, policy: UserMessagePolicy) -> &mut Self {
        self.inner.user_messages = policy;
        self
    }

    /// Policy for system messages.
    pub fn system_messages(&mut self, policy: SystemMessagePolicy) -> &mut Self {
        self.inner.system_messages = policy;
        self
    }

    /// Standard vs AgentLossless regime.
    pub fn mode(&mut self, mode: SdkMode) -> &mut Self {
        self.inner.mode = mode;
        self
    }

    /// Number of recent messages protected from source-code compression.
    pub fn protect_recent(&mut self, count: usize) -> &mut Self {
        self.inner.protect_recent = count;
        self
    }

    /// Leading cached message count never modified by [`compress`].
    pub fn frozen_message_count(&mut self, count: usize) -> &mut Self {
        self.inner.frozen_message_count = count;
        self
    }

    /// Extractive prose keep-ratio in `(0.0, 1.0]`.
    pub fn target_ratio(&mut self, ratio: Option<f64>) -> &mut Self {
        self.inner.target_ratio = ratio;
        self
    }

    /// Minimum tokens required before attempting compression.
    pub fn min_tokens_to_compress(&mut self, tokens: usize) -> &mut Self {
        self.inner.min_tokens_to_compress = tokens;
        self
    }

    /// Relevance query context string.
    pub fn context(&mut self, context: impl Into<String>) -> &mut Self {
        self.inner.context = context.into();
        self
    }

    /// Validate and produce [`CompressConfig`].
    ///
    /// # Errors
    /// Returns [`CompressError::InvalidTargetRatio`] if `target_ratio` is invalid.
    pub fn build(&self) -> Result<CompressConfig, CompressError> {
        self.inner.validate()?;
        Ok(self.inner.clone())
    }
}

/// Result of compressing a single text payload via [`compress_text`].
#[derive(Debug, Clone, PartialEq)]
pub struct TextCompressResult {
    pub compressed: String,
    pub original_tokens: usize,
    pub compressed_tokens: usize,
    pub tokens_saved: usize,
    pub compression_ratio: f64,
    pub strategy: String,
    pub ccr_hash: Option<String>,
    pub was_modified: bool,
}

/// Result of compressing an OpenAI or Anthropic message slice via [`compress`].
#[derive(Debug, Clone, PartialEq)]
pub struct CompressResult {
    pub messages: Vec<Value>,
    pub tokens_before: usize,
    pub tokens_after: usize,
    pub tokens_saved: usize,
    pub compression_ratio: f64,
    pub transforms_applied: Vec<String>,
}

/// Compress raw `content` text using Headroom's content-aware transform stack.
///
/// # Errors
/// Returns [`CompressError::InvalidTargetRatio`] when `config.target_ratio` is out of range.
pub fn compress_text(
    content: &str,
    config: &CompressConfig,
    ccr_store: Option<&dyn CcrStore>,
) -> Result<TextCompressResult, CompressError> {
    config.validate()?;
    let tokenizer = get_tokenizer(&config.model);
    let original_tokens = tokenizer.count_text(content);
    if content.trim().is_empty() || original_tokens < config.min_tokens_to_compress {
        return Ok(passthrough_text(content, original_tokens));
    }

    let outcome = dispatch_text(
        content,
        config,
        ccr_store,
        tokenizer.as_ref(),
        AllowCodeCompression::Yes,
    );
    let Some((candidate, strategy, pre_ccr_hash, already_ccr_marked)) = outcome else {
        return Ok(passthrough_text(content, original_tokens));
    };

    let (final_text, ccr_hash) = finalize_ccr(
        content,
        &candidate,
        config.mode,
        ccr_store,
        pre_ccr_hash,
        already_ccr_marked,
    );

    let compressed_tokens = tokenizer.count_text(&final_text);
    if compressed_tokens >= original_tokens {
        return Ok(passthrough_text(content, original_tokens));
    }

    let tokens_saved = original_tokens - compressed_tokens;
    let compression_ratio = compressed_tokens as f64 / original_tokens.max(1) as f64;
    Ok(TextCompressResult {
        compressed: final_text,
        original_tokens,
        compressed_tokens,
        tokens_saved,
        compression_ratio,
        strategy,
        ccr_hash,
        was_modified: true,
    })
}

/// Compress a slice of OpenAI or Anthropic chat messages without a CCR store.
///
/// # Errors
/// Returns [`CompressError::InvalidTargetRatio`] when `config.target_ratio` is out of range.
pub fn compress(
    messages: &[Value],
    config: &CompressConfig,
) -> Result<CompressResult, CompressError> {
    compress_with_ccr(messages, config, None)
}

/// Compress a slice of OpenAI or Anthropic chat messages, stashing originals in `ccr_store`.
///
/// # Errors
/// Returns [`CompressError::InvalidTargetRatio`] when `config.target_ratio` is out of range.
pub fn compress_with_ccr(
    messages: &[Value],
    config: &CompressConfig,
    ccr_store: Option<&dyn CcrStore>,
) -> Result<CompressResult, CompressError> {
    config.validate()?;
    if messages.is_empty() {
        return Ok(CompressResult {
            messages: Vec::new(),
            tokens_before: 0,
            tokens_after: 0,
            tokens_saved: 0,
            compression_ratio: 1.0,
            transforms_applied: Vec::new(),
        });
    }

    let tokenizer = get_tokenizer(&config.model);
    let mut cfg = config.clone();
    if cfg.context.is_empty() {
        cfg.context = extract_user_query(messages);
    }

    let total = messages.len();
    let code_protect_floor = total.saturating_sub(cfg.protect_recent);

    let mut out_messages = Vec::with_capacity(total);
    let mut tokens_before = 0usize;
    let mut tokens_after = 0usize;
    let mut transforms_applied: Vec<String> = Vec::new();

    for (idx, msg) in messages.iter().enumerate() {
        let mut msg_clone = msg.clone();
        let before_msg_tokens = count_message_tokens(msg, tokenizer.as_ref());
        tokens_before += before_msg_tokens;

        if idx < cfg.frozen_message_count {
            tokens_after += before_msg_tokens;
            out_messages.push(msg_clone);
            continue;
        }

        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        let allow_code = if idx >= code_protect_floor {
            AllowCodeCompression::No
        } else {
            AllowCodeCompression::Yes
        };

        let mut modified = false;
        match role {
            "system" | "developer" => {
                if cfg.system_messages == SystemMessagePolicy::Compress {
                    modified = compress_message_content(
                        &mut msg_clone,
                        &cfg,
                        ccr_store,
                        tokenizer.as_ref(),
                        allow_code,
                        &mut transforms_applied,
                    );
                }
            }
            "tool" => {
                modified = compress_message_content(
                    &mut msg_clone,
                    &cfg,
                    ccr_store,
                    tokenizer.as_ref(),
                    allow_code,
                    &mut transforms_applied,
                );
            }
            "user" => {
                modified = compress_user_or_anthropic_blocks(
                    &mut msg_clone,
                    &cfg,
                    ccr_store,
                    tokenizer.as_ref(),
                    allow_code,
                    &mut transforms_applied,
                );
            }
            _ => {}
        }

        if modified {
            let after_msg_tokens = count_message_tokens(&msg_clone, tokenizer.as_ref());
            if after_msg_tokens < before_msg_tokens {
                tokens_after += after_msg_tokens;
                out_messages.push(msg_clone);
                continue;
            }
        }

        tokens_after += before_msg_tokens;
        out_messages.push(msg.clone());
    }

    let tokens_saved = tokens_before.saturating_sub(tokens_after);
    let compression_ratio = if tokens_before == 0 {
        1.0
    } else {
        tokens_after as f64 / tokens_before as f64
    };

    Ok(CompressResult {
        messages: out_messages,
        tokens_before,
        tokens_after,
        tokens_saved,
        compression_ratio,
        transforms_applied,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AllowCodeCompression {
    Yes,
    No,
}

fn passthrough_text(content: &str, original_tokens: usize) -> TextCompressResult {
    TextCompressResult {
        compressed: content.to_string(),
        original_tokens,
        compressed_tokens: original_tokens,
        tokens_saved: 0,
        compression_ratio: 1.0,
        strategy: "passthrough".to_string(),
        ccr_hash: None,
        was_modified: false,
    }
}

fn finalize_ccr(
    original: &str,
    candidate: &str,
    mode: SdkMode,
    ccr_store: Option<&dyn CcrStore>,
    pre_ccr_hash: Option<String>,
    already_marked: bool,
) -> (String, Option<String>) {
    if mode == SdkMode::AgentLossless {
        return (candidate.to_string(), None);
    }
    let Some(store) = ccr_store else {
        return (candidate.to_string(), pre_ccr_hash);
    };
    let hash = pre_ccr_hash.unwrap_or_else(|| compute_key(original.as_bytes()));
    store.put(&hash, original);
    if already_marked || candidate.contains("<<ccr:") || candidate.contains("Retrieve original:") {
        return (candidate.to_string(), Some(hash));
    }
    let marker = marker_for(&hash);
    let out = if candidate.ends_with('\n') {
        format!("{candidate}{marker}")
    } else {
        format!("{candidate}\n{marker}")
    };
    (out, Some(hash))
}

fn build_smart_crusher(mode: SdkMode) -> SmartCrusher {
    let mut cfg = SmartCrusherConfig::default();
    if mode == SdkMode::AgentLossless {
        cfg.lossless_only = true;
        cfg.enable_ccr_marker = false;
    }
    SmartCrusher::new(cfg)
}

fn dispatch_text(
    content: &str,
    config: &CompressConfig,
    ccr_store: Option<&dyn CcrStore>,
    tokenizer: &dyn Tokenizer,
    allow_code: AllowCodeCompression,
) -> Option<(String, String, Option<String>, bool)> {
    let detected = detect_content_type(content);
    let trimmed = content.trim();

    match detected.content_type {
        ContentType::JsonArray => {
            let crusher = build_smart_crusher(config.mode);
            let res = crusher.crush(content, &config.context, 1.0);
            if res.was_modified && res.compressed != content {
                let marked = res.compressed.contains("<<ccr:");
                return Some((res.compressed, "smart_crusher".to_string(), None, marked));
            }
            if let Ok(Some(minified)) = JsonMinifier.minify(content) {
                if minified != content {
                    return Some((minified, "json_minifier".to_string(), None, true));
                }
            }
            None
        }
        ContentType::Tabular => {
            let res = TabularCompressor::new(TabularCompressorConfig::default())
                .compress(content, &config.context);
            if res.was_modified && res.compressed != content {
                let marked = res.compressed.contains("<<ccr:");
                if config.mode == SdkMode::AgentLossless && marked {
                    return None;
                }
                return Some((res.compressed, "tabular".to_string(), None, marked));
            }
            None
        }
        ContentType::Html => {
            if config.mode == SdkMode::AgentLossless {
                return None;
            }
            let res = HtmlExtractor::new(HtmlExtractorConfig::default()).extract(content, None);
            if !res.extracted.is_empty() && res.extracted.len() < content.len() {
                Some((res.extracted, "html_extractor".to_string(), None, false))
            } else {
                None
            }
        }
        ContentType::GitDiff => {
            let lossless = compact_lossless(content, LosslessKind::Diff);
            if config.mode == SdkMode::AgentLossless {
                return (lossless != content)
                    .then(|| (lossless, "lossless_diff".to_string(), None, true));
            }
            let diff = DiffCompressor::new(DiffCompressorConfig::default())
                .compress(&lossless, &config.context);
            if diff.compressed != content {
                return Some((diff.compressed, "diff_compressor".to_string(), None, false));
            }
            (lossless != content).then(|| (lossless, "lossless_diff".to_string(), None, true))
        }
        ContentType::BuildOutput => {
            let lossless = compact_lossless(content, LosslessKind::Log);
            if config.mode == SdkMode::AgentLossless {
                return (lossless != content)
                    .then(|| (lossless, "lossless_log".to_string(), None, true));
            }
            let (res, _) =
                LogCompressor::new(LogCompressorConfig::default()).compress(&lossless, 1.0);
            if res.compressed != content {
                return Some((res.compressed, "log_compressor".to_string(), None, false));
            }
            (lossless != content).then(|| (lossless, "lossless_log".to_string(), None, true))
        }
        ContentType::SearchResults => {
            let lossless = compact_lossless(content, LosslessKind::Search);
            if config.mode == SdkMode::AgentLossless {
                return (lossless != content)
                    .then(|| (lossless, "lossless_search".to_string(), None, true));
            }
            let (res, _) = SearchCompressor::new(SearchCompressorConfig::default()).compress(
                &lossless,
                &config.context,
                1.0,
            );
            if res.compressed != content {
                return Some((res.compressed, "search_compressor".to_string(), None, false));
            }
            (lossless != content).then(|| (lossless, "lossless_search".to_string(), None, true))
        }
        ContentType::SourceCode => {
            if allow_code == AllowCodeCompression::No || config.mode == SdkMode::AgentLossless {
                return None;
            }
            let res = CodeAwareCompressor::new(CodeCompressorConfig::default()).compress(content);
            if res.compressed != content {
                Some((res.compressed, "code_compressor".to_string(), None, false))
            } else {
                None
            }
        }
        ContentType::PlainText => {
            if trimmed.starts_with('{') && serde_json::from_str::<Value>(trimmed).is_ok() {
                let crusher = build_smart_crusher(config.mode);
                let res = crusher.crush(content, &config.context, 1.0);
                if res.was_modified && res.compressed != content {
                    let marked = res.compressed.contains("<<ccr:");
                    return Some((res.compressed, "smart_crusher".to_string(), None, marked));
                }
                if let Ok(Some(minified)) = JsonMinifier.minify(content) {
                    if minified != content {
                        return Some((minified, "json_minifier".to_string(), None, true));
                    }
                }
            }

            if detect_config_flavor(content) != ConfigFlavor::Unknown {
                let cfg_ccr = if config.mode == SdkMode::AgentLossless {
                    CcrPolicy::Disabled
                } else {
                    CcrPolicy::Enabled
                };
                let compressor = ConfigCompressor::new(ConfigCompressorConfig {
                    ccr_policy: cfg_ccr,
                    schema_fold: SchemaFoldPolicy::Enabled,
                    min_savings_chars: 1,
                });
                let store_ref = if config.mode == SdkMode::AgentLossless {
                    None
                } else {
                    ccr_store
                };
                let res = compressor.compress(content, &config.context, store_ref);
                if res.was_modified && res.compressed != content {
                    let already_marked = res.ccr_hash.is_some();
                    return Some((res.compressed, res.strategy, res.ccr_hash, already_marked));
                }
            }

            let crusher = Arc::new(
                SmartCrusherBuilder::new(SmartCrusherConfig {
                    lossless_only: config.mode == SdkMode::AgentLossless,
                    enable_ccr_marker: config.mode == SdkMode::Standard,
                    ..SmartCrusherConfig::default()
                })
                .with_default_oss_setup()
                .build(),
            );

            if let Some((spliced, reversible)) = route_embedded_json(
                content,
                |chunk| {
                    let r = crusher.crush(chunk, &config.context, 1.0);
                    if r.was_modified && r.compressed != chunk {
                        let rev = !r.compressed.contains("<<ccr:");
                        Some((r.compressed, rev))
                    } else {
                        None
                    }
                },
                |s| tokenizer.count_text(s),
            ) {
                if config.mode == SdkMode::Standard || reversible {
                    return Some((spliced, "recursive_json".to_string(), None, !reversible));
                }
            }

            let path_folded = compact_lossless(content, LosslessKind::Paths);
            if path_folded != content {
                return Some((path_folded, "lossless_paths".to_string(), None, true));
            }

            let text_folded = compact_lossless(content, LosslessKind::Text);
            let working = if text_folded != content {
                text_folded
            } else {
                content.to_string()
            };

            if config.mode == SdkMode::Standard && config.target_ratio.is_some() {
                let crusher = TextCrusher::new(TextCrusherConfig::default());
                let res = crusher.compress(&working, &config.context, config.target_ratio);
                if res.compressed != working && res.compressed.len() < working.len() {
                    return Some((res.compressed, "text_crusher".to_string(), None, false));
                }
            }

            (working != content).then(|| (working, "lossless_text".to_string(), None, true))
        }
    }
}

fn compress_message_content(
    msg: &mut Value,
    config: &CompressConfig,
    ccr_store: Option<&dyn CcrStore>,
    tokenizer: &dyn Tokenizer,
    allow_code: AllowCodeCompression,
    transforms_applied: &mut Vec<String>,
) -> bool {
    let Some(content_val) = msg.get_mut("content") else {
        return false;
    };
    match content_val {
        Value::String(text) => {
            if let Ok(res) = compress_text_inner(text, config, ccr_store, tokenizer, allow_code) {
                if res.was_modified {
                    *text = res.compressed;
                    record_strategy(transforms_applied, &res.strategy);
                    return true;
                }
            }
            false
        }
        Value::Array(blocks) => compress_block_array(
            blocks,
            config,
            ccr_store,
            tokenizer,
            allow_code,
            true,
            transforms_applied,
        ),
        _ => false,
    }
}

fn compress_user_or_anthropic_blocks(
    msg: &mut Value,
    config: &CompressConfig,
    ccr_store: Option<&dyn CcrStore>,
    tokenizer: &dyn Tokenizer,
    allow_code: AllowCodeCompression,
    transforms_applied: &mut Vec<String>,
) -> bool {
    let Some(content_val) = msg.get_mut("content") else {
        return false;
    };
    match content_val {
        Value::String(text) => {
            if config.user_messages != UserMessagePolicy::Compress {
                return false;
            }
            if let Ok(res) = compress_text_inner(text, config, ccr_store, tokenizer, allow_code) {
                if res.was_modified {
                    *text = res.compressed;
                    record_strategy(transforms_applied, &res.strategy);
                    return true;
                }
            }
            false
        }
        Value::Array(blocks) => compress_block_array(
            blocks,
            config,
            ccr_store,
            tokenizer,
            allow_code,
            config.user_messages == UserMessagePolicy::Compress,
            transforms_applied,
        ),
        _ => false,
    }
}

fn compress_block_array(
    blocks: &mut [Value],
    config: &CompressConfig,
    ccr_store: Option<&dyn CcrStore>,
    tokenizer: &dyn Tokenizer,
    allow_code: AllowCodeCompression,
    compress_plain_text_blocks: bool,
    transforms_applied: &mut Vec<String>,
) -> bool {
    let mut any_modified = false;
    for block in blocks {
        let Some(obj) = block.as_object_mut() else {
            continue;
        };
        let block_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
        match block_type {
            "tool_result" => {
                if let Some(inner) = obj.get_mut("content") {
                    match inner {
                        Value::String(s) => {
                            if let Ok(res) =
                                compress_text_inner(s, config, ccr_store, tokenizer, allow_code)
                            {
                                if res.was_modified {
                                    *s = res.compressed;
                                    record_strategy(transforms_applied, &res.strategy);
                                    any_modified = true;
                                }
                            }
                        }
                        Value::Array(sub_blocks) => {
                            if compress_block_array(
                                sub_blocks,
                                config,
                                ccr_store,
                                tokenizer,
                                allow_code,
                                true,
                                transforms_applied,
                            ) {
                                any_modified = true;
                            }
                        }
                        _ => {}
                    }
                }
            }
            "text" | "input_text" if compress_plain_text_blocks => {
                if let Some(Value::String(s)) = obj.get_mut("text") {
                    if let Ok(res) =
                        compress_text_inner(s, config, ccr_store, tokenizer, allow_code)
                    {
                        if res.was_modified {
                            *s = res.compressed;
                            record_strategy(transforms_applied, &res.strategy);
                            any_modified = true;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    any_modified
}

fn compress_text_inner(
    content: &str,
    config: &CompressConfig,
    ccr_store: Option<&dyn CcrStore>,
    tokenizer: &dyn Tokenizer,
    allow_code: AllowCodeCompression,
) -> Result<TextCompressResult, CompressError> {
    let original_tokens = tokenizer.count_text(content);
    if content.trim().is_empty() || original_tokens < config.min_tokens_to_compress {
        return Ok(passthrough_text(content, original_tokens));
    }
    let Some((candidate, strategy, pre_ccr_hash, already_ccr_marked)) =
        dispatch_text(content, config, ccr_store, tokenizer, allow_code)
    else {
        return Ok(passthrough_text(content, original_tokens));
    };
    let (final_text, ccr_hash) = finalize_ccr(
        content,
        &candidate,
        config.mode,
        ccr_store,
        pre_ccr_hash,
        already_ccr_marked,
    );
    let compressed_tokens = tokenizer.count_text(&final_text);
    if compressed_tokens >= original_tokens {
        return Ok(passthrough_text(content, original_tokens));
    }
    let tokens_saved = original_tokens - compressed_tokens;
    let compression_ratio = compressed_tokens as f64 / original_tokens.max(1) as f64;
    Ok(TextCompressResult {
        compressed: final_text,
        original_tokens,
        compressed_tokens,
        tokens_saved,
        compression_ratio,
        strategy,
        ccr_hash,
        was_modified: true,
    })
}

fn record_strategy(list: &mut Vec<String>, strategy: &str) {
    if !list.iter().any(|s| s == strategy) {
        list.push(strategy.to_string());
    }
}

fn extract_user_query(messages: &[Value]) -> String {
    for msg in messages.iter().rev() {
        if msg.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        if let Some(s) = msg.get("content").and_then(Value::as_str) {
            return s.to_string();
        }
        if let Some(arr) = msg.get("content").and_then(Value::as_array) {
            for block in arr {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        return t.to_string();
                    }
                }
            }
        }
    }
    String::new()
}

fn count_message_tokens(msg: &Value, tokenizer: &dyn Tokenizer) -> usize {
    match msg.get("content") {
        Some(Value::String(s)) => tokenizer.count_text(s),
        Some(Value::Array(arr)) => arr
            .iter()
            .map(|b| match b.get("text").or_else(|| b.get("content")) {
                Some(Value::String(s)) => tokenizer.count_text(s),
                Some(other) => tokenizer.count_text(&other.to_string()),
                None => tokenizer.count_text(&b.to_string()),
            })
            .sum(),
        Some(other) => tokenizer.count_text(&other.to_string()),
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ccr::InMemoryCcrStore;
    use serde_json::json;

    #[test]
    fn rejects_invalid_target_ratio() {
        let mut builder = CompressConfig::builder();
        builder.target_ratio(Some(0.0));
        assert!(matches!(
            builder.build(),
            Err(CompressError::InvalidTargetRatio(_))
        ));
    }

    #[test]
    fn compress_text_stashes_in_ccr_store() {
        let store = InMemoryCcrStore::new();
        let rows: Vec<Value> = (0..100)
            .map(|i| json!({"id": i, "status": "ok", "service": "billing", "latency_ms": 12}))
            .collect();
        let raw = serde_json::to_string(&rows).unwrap();
        let res = compress_text(&raw, &CompressConfig::default(), Some(&store)).unwrap();
        assert!(res.was_modified);
        assert!(res.tokens_saved > 0);
        let hash = res.ccr_hash.expect("ccr hash present");
        assert_eq!(store.get(&hash).as_deref(), Some(raw.as_str()));
    }

    #[test]
    fn compress_messages_respects_frozen_and_user_policies() {
        let big_rows: Vec<Value> = (0..60)
            .map(|i| json!({"id": i, "kind": "row", "ok": true}))
            .collect();
        let big_json = serde_json::to_string(&big_rows).unwrap();
        let msgs = vec![
            json!({"role": "system", "content": big_json.clone()}),
            json!({"role": "user", "content": big_json.clone()}),
            json!({"role": "tool", "content": big_json.clone()}),
        ];
        let cfg = CompressConfig {
            frozen_message_count: 1,
            ..CompressConfig::default()
        };
        let res = compress(&msgs, &cfg).unwrap();
        assert_eq!(res.messages[0]["content"], big_json);
        assert_eq!(res.messages[1]["content"], big_json);
        assert!(res.tokens_saved > 0);
        assert!(!res.transforms_applied.is_empty());
    }
}
