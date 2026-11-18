//! Structured-config compressor for YAML, TOML, and INI tool outputs.
//!
//! Port of `headroom/transforms/config_compressor.py`. Compresses configuration
//! payloads across three tiers:
//! 1. Reversible lossless run/stanza folding via [`compact_lossless`].
//! 2. Safe comment/blank-line elision backed by a [`CcrStore`] retrieval marker.
//! 3. TOML `[[array-of-tables]]` schema folding via [`SmartCrusher`].

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::lossless_compaction::{compact_lossless, LosslessKind};
use super::smart_crusher::{SmartCrusher, SmartCrusherConfig};
use crate::ccr::{compute_key, CcrStore};

static CONFIG_COMMENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[#;]").expect("valid comment regex"));
static CONFIG_SECTION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\[+[^\]\r\n]+\]+(?:\s*[#;].*)?$").expect("valid section"));
static TOML_ASSIGN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*(?:[A-Za-z0-9_-]+|"[^"\r\n]+"|'[^'\r\n]+')\s*="#).expect("toml assign")
});
static INI_ASSIGN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[A-Za-z0-9_.-]+\s*[:=]").expect("ini assign"));
static YAML_KEY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*(?:-\s+)?(?:[A-Za-z0-9_.-]+|"[^"\r\n]+"|'[^'\r\n]+')\s*:(?:\s|$)"#)
        .expect("yaml key")
});
static YAML_LIST_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*-\s+\S").expect("yaml list"));
static YAML_DOC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:---|\.\.\.)(?:\s|$)").expect("yaml doc"));
static YAML_BLOCK_SCALAR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)(?:^|[ \t:])[|>][1-9+-]*[ \t]*(?:#[^\r\n]*)?\r?$").expect("yaml scalar")
});

/// Configuration file format detected by [`detect_config_flavor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigFlavor {
    Yaml,
    Toml,
    Ini,
    Unknown,
}

impl ConfigFlavor {
    /// Lowercase tag string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ConfigFlavor::Yaml => "yaml",
            ConfigFlavor::Toml => "toml",
            ConfigFlavor::Ini => "ini",
            ConfigFlavor::Unknown => "unknown",
        }
    }
}

/// Whether CCR-backed comment elision may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CcrPolicy {
    #[default]
    Enabled,
    Disabled,
}

/// Whether TOML `[[array-of-tables]]` schema folding may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SchemaFoldPolicy {
    #[default]
    Enabled,
    Disabled,
}

/// Configuration for [`ConfigCompressor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigCompressorConfig {
    pub ccr_policy: CcrPolicy,
    pub schema_fold: SchemaFoldPolicy,
    pub min_savings_chars: usize,
}

impl Default for ConfigCompressorConfig {
    fn default() -> Self {
        Self {
            ccr_policy: CcrPolicy::Enabled,
            schema_fold: SchemaFoldPolicy::Enabled,
            min_savings_chars: 1,
        }
    }
}

/// Result returned by [`ConfigCompressor::compress`].
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigCompressionResult {
    pub compressed: String,
    pub original: String,
    pub was_modified: bool,
    pub flavor: ConfigFlavor,
    pub lines_elided: usize,
    pub ccr_hash: Option<String>,
    pub strategy: String,
}

impl ConfigCompressionResult {
    /// Byte length ratio (`compressed / original`).
    #[must_use]
    pub fn compression_ratio(&self) -> f64 {
        if self.original.is_empty() {
            return 0.0;
        }
        self.compressed.len() as f64 / self.original.len() as f64
    }
}

/// Detect whether `content` is YAML, TOML, INI, or not a structured config.
#[must_use]
pub fn detect_config_flavor(content: &str) -> ConfigFlavor {
    let trimmed = content.trim_start();
    let Some(first) = trimmed.chars().next() else {
        return ConfigFlavor::Unknown;
    };
    if first == '{' || first == '<' {
        return ConfigFlavor::Unknown;
    }

    let lines: Vec<&str> = content.split('\n').take(200).collect();
    let non_empty: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|ln| !ln.trim().is_empty())
        .collect();
    if non_empty.len() < 3 {
        return ConfigFlavor::Unknown;
    }
    let body: Vec<&str> = non_empty
        .iter()
        .copied()
        .filter(|ln| !CONFIG_COMMENT_RE.is_match(ln))
        .collect();
    if body.len() < 3 {
        return ConfigFlavor::Unknown;
    }

    let sections = body
        .iter()
        .filter(|ln| CONFIG_SECTION_RE.is_match(ln))
        .count();
    if sections >= 1 {
        let assigns = body
            .iter()
            .filter(|ln| TOML_ASSIGN_RE.is_match(ln) || INI_ASSIGN_RE.is_match(ln))
            .count();
        if assigns >= 2 && (sections + assigns) as f64 / body.len() as f64 >= 0.6 {
            if content.len() <= 1_000_000 && toml::from_str::<toml::Value>(content).is_ok() {
                return ConfigFlavor::Toml;
            }
            if looks_like_ini(&body) {
                return ConfigFlavor::Ini;
            }
        }
    }

    if lines.first().is_some_and(|l| l.trim() == "---") {
        for idx in 1..lines.len().min(60) {
            if matches!(lines[idx].trim(), "---" | "...") {
                let tail: Vec<&str> = lines[idx + 1..]
                    .iter()
                    .copied()
                    .filter(|ln| !ln.trim().is_empty())
                    .collect();
                if !tail.is_empty() {
                    let tail_yaml = tail
                        .iter()
                        .filter(|ln| YAML_KEY_RE.is_match(ln) || YAML_LIST_RE.is_match(ln))
                        .count();
                    if (tail_yaml as f64 / tail.len() as f64) < 0.3 {
                        return ConfigFlavor::Unknown;
                    }
                }
                break;
            }
        }
    }

    let yaml_keys = body.iter().filter(|ln| YAML_KEY_RE.is_match(ln)).count();
    let yaml_lists = body
        .iter()
        .filter(|ln| YAML_LIST_RE.is_match(ln) && !YAML_KEY_RE.is_match(ln))
        .count();
    let doc_marks = body
        .iter()
        .filter(|ln| YAML_DOC_RE.is_match(ln.trim()))
        .count();

    if yaml_keys < 3 {
        return ConfigFlavor::Unknown;
    }
    let share = (yaml_keys + yaml_lists + doc_marks) as f64 / body.len() as f64;
    if share < 0.6 {
        return ConfigFlavor::Unknown;
    }
    let enders = body
        .iter()
        .filter(|ln| {
            let t = ln.trim_end();
            t.ends_with('.') || t.ends_with('!') || t.ends_with('?')
        })
        .count();
    if enders as f64 / body.len() as f64 >= 0.5 {
        return ConfigFlavor::Unknown;
    }
    let avg_words = body
        .iter()
        .map(|ln| ln.split_whitespace().count())
        .sum::<usize>() as f64
        / body.len() as f64;
    if avg_words > 8.0 {
        return ConfigFlavor::Unknown;
    }
    let indents: HashSet<usize> = body
        .iter()
        .filter(|ln| YAML_KEY_RE.is_match(ln) || YAML_LIST_RE.is_match(ln))
        .map(|ln| ln.len() - ln.trim_start_matches(' ').len())
        .collect();
    if indents.len() < 2 && doc_marks == 0 && yaml_lists < 3 {
        return ConfigFlavor::Unknown;
    }
    ConfigFlavor::Yaml
}

fn looks_like_ini(body: &[&str]) -> bool {
    let has_section = body
        .iter()
        .any(|ln| ln.trim().starts_with('[') && ln.trim().ends_with(']'));
    let has_assign = body.iter().any(|ln| INI_ASSIGN_RE.is_match(ln));
    has_section && has_assign
}

/// Structured-config compressor for YAML, TOML, and INI inputs.
#[derive(Debug, Clone, Default)]
pub struct ConfigCompressor {
    config: ConfigCompressorConfig,
}

impl ConfigCompressor {
    /// Construct a [`ConfigCompressor`] with `config`.
    #[must_use]
    pub fn new(config: ConfigCompressorConfig) -> Self {
        Self { config }
    }

    /// Compress `content` using reversible folding plus optional CCR-backed comment elision/schema fold.
    #[must_use]
    pub fn compress(
        &self,
        content: &str,
        context: &str,
        ccr_store: Option<&dyn CcrStore>,
    ) -> ConfigCompressionResult {
        let flavor = detect_config_flavor(content);
        if flavor == ConfigFlavor::Unknown {
            return ConfigCompressionResult {
                compressed: content.to_string(),
                original: content.to_string(),
                was_modified: false,
                flavor,
                lines_elided: 0,
                ccr_hash: None,
                strategy: "config".to_string(),
            };
        }

        let schema_fold = if ccr_store.is_some()
            && self.config.ccr_policy == CcrPolicy::Enabled
            && self.config.schema_fold == SchemaFoldPolicy::Enabled
        {
            self.schema_fold(content, flavor, context, ccr_store)
        } else {
            None
        };

        let mut working = content.to_string();
        let mut lines_elided = 0usize;
        let mut ccr_hash: Option<String> = None;

        if let Some(store) = ccr_store.filter(|_| {
            self.config.ccr_policy == CcrPolicy::Enabled && elision_safe(content, flavor)
        }) {
            let (stripped, elided) = strip_comment_lines(content, flavor);
            if elided > 0 {
                let hash = compute_key(content.as_bytes());
                store.put(&hash, content);
                let marker = format!(
                    "[{elided} comment/blank lines elided. Retrieve original: hash={hash}]"
                );
                working = if stripped.ends_with('\n') {
                    format!("{stripped}{marker}")
                } else {
                    format!("{stripped}\n{marker}")
                };
                lines_elided = elided;
                ccr_hash = Some(hash);
            }
        }

        let compressed = compact_lossless(&working, LosslessKind::Config);

        if let Some((folded, fold_hash)) = schema_fold {
            if folded.len() < compressed.len() {
                return ConfigCompressionResult {
                    compressed: folded,
                    original: content.to_string(),
                    was_modified: true,
                    flavor,
                    lines_elided: 0,
                    ccr_hash: Some(fold_hash),
                    strategy: "config_schema_fold".to_string(),
                };
            }
        }

        let savings = content.len().saturating_sub(compressed.len());
        if savings < self.config.min_savings_chars {
            return ConfigCompressionResult {
                compressed: content.to_string(),
                original: content.to_string(),
                was_modified: false,
                flavor,
                lines_elided: 0,
                ccr_hash: None,
                strategy: "config".to_string(),
            };
        }

        ConfigCompressionResult {
            compressed,
            original: content.to_string(),
            was_modified: true,
            flavor,
            lines_elided,
            ccr_hash,
            strategy: "config".to_string(),
        }
    }

    fn schema_fold(
        &self,
        content: &str,
        flavor: ConfigFlavor,
        context: &str,
        ccr_store: Option<&dyn CcrStore>,
    ) -> Option<(String, String)> {
        if flavor != ConfigFlavor::Toml || !content.contains("[[") {
            return None;
        }
        let store = ccr_store?;
        let parsed: toml::Value = toml::from_str(content).ok()?;
        let json_val = toml_to_json(parsed);
        let json_str = serde_json::to_string(&json_val).ok()?;

        let crusher = SmartCrusher::new(SmartCrusherConfig::default());
        let result = crusher.crush(&json_str, context, 1.0);
        if !result.was_modified || result.strategy == "passthrough" {
            return None;
        }
        let hash = compute_key(content.as_bytes());
        store.put(&hash, content);
        let folded = format!(
            "{}\n[config folded to schema. Retrieve original: hash={hash}]",
            result.compressed
        );
        if content.len().saturating_sub(folded.len()) < self.config.min_savings_chars {
            return None;
        }
        Some((folded, hash))
    }
}

fn toml_to_json(val: toml::Value) -> Value {
    match val {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(i) => Value::Number(i.into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(f)
            .map_or_else(|| Value::String(f.to_string()), Value::Number),
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Datetime(dt) => Value::String(dt.to_string()),
        toml::Value::Array(arr) => Value::Array(arr.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(tbl) => {
            let mut map = Map::with_capacity(tbl.len());
            for (k, v) in tbl {
                map.insert(k, toml_to_json(v));
            }
            Value::Object(map)
        }
    }
}

fn elision_safe(content: &str, flavor: ConfigFlavor) -> bool {
    match flavor {
        ConfigFlavor::Yaml => !YAML_BLOCK_SCALAR_RE.is_match(content),
        ConfigFlavor::Toml => !content.contains("\"\"\"") && !content.contains("'''"),
        ConfigFlavor::Ini | ConfigFlavor::Unknown => true,
    }
}

fn strip_comment_lines(content: &str, flavor: ConfigFlavor) -> (String, usize) {
    let keep_blanks = flavor == ConfigFlavor::Ini;
    let had_trailing = content.ends_with('\n');
    let body = if had_trailing {
        &content[..content.len() - 1]
    } else {
        content
    };
    let mut kept: Vec<&str> = Vec::new();
    let mut elided = 0usize;
    for line in body.split('\n') {
        let is_comment = match flavor {
            ConfigFlavor::Ini => line.starts_with('#') || line.starts_with(';'),
            _ => line.trim_start().starts_with('#'),
        };
        if is_comment || (!keep_blanks && line.trim().is_empty()) {
            elided += 1;
        } else {
            kept.push(line);
        }
    }
    let mut out = kept.join("\n");
    if had_trailing {
        out.push('\n');
    }
    (out, elided)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ccr::InMemoryCcrStore;

    #[test]
    fn detects_yaml_toml_ini_and_rejects_prose() {
        let yaml = "apiVersion: v1\nkind: Service\nmetadata:\n  name: api\n  namespace: prod\n";
        assert_eq!(detect_config_flavor(yaml), ConfigFlavor::Yaml);

        let toml_src = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n";
        assert_eq!(detect_config_flavor(toml_src), ConfigFlavor::Toml);

        let ini = "[database]\nhost = localhost\nport = 5432\n";
        assert_eq!(detect_config_flavor(ini), ConfigFlavor::Ini);
        let ini_only = "[database]\nhost: localhost\nuser = admin\n";
        assert_eq!(detect_config_flavor(ini_only), ConfigFlavor::Ini);

        let prose = "First sentence here. Second sentence follows. Third sentence ends.";
        assert_eq!(detect_config_flavor(prose), ConfigFlavor::Unknown);
    }

    #[test]
    fn elides_comments_with_ccr_store_and_recovers_original() {
        let yaml = "\
# Main service deployment manifest
# Managed by platform team

apiVersion: apps/v1
kind: Deployment
metadata:
  name: api-server
  labels:
    app: api-server
    tier: backend
# End of manifest
";
        let store = InMemoryCcrStore::new();
        let res = ConfigCompressor::default().compress(yaml, "", Some(&store));
        assert!(res.was_modified);
        let hash = res.ccr_hash.expect("ccr hash populated");
        assert_eq!(store.get(&hash).as_deref(), Some(yaml));
    }
}
