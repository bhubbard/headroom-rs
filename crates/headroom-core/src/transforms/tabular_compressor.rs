//! Tabular-text compressor: bridges CSV, TSV, Markdown, and fixed-width tables to SmartCrusher.
//!
//! Port of `headroom/transforms/tabular_ingest.py`. Parses tabular text into JSON
//! records and routes them through [`SmartCrusher`], which performs lossless
//! `csv-schema` compaction first and falls back to CCR-recoverable sampling when
//! lossless does not meet its threshold.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::content_detector::{detect_content_type, ContentType};
use super::smart_crusher::{SmartCrusher, SmartCrusherConfig};

const MAX_CSV_FIELD_BYTES: usize = 128 * 1024;
static MD_SEP_CELL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^:?-{2,}:?$").expect("valid markdown separator regex"));

/// Tabular format identified by [`parse_tabular`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TabularFormat {
    Csv,
    Tsv,
    Markdown,
    FixedWidth,
}

impl TabularFormat {
    /// Canonical lowercase name for telemetry and display.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TabularFormat::Csv => "csv",
            TabularFormat::Tsv => "tsv",
            TabularFormat::Markdown => "markdown",
            TabularFormat::FixedWidth => "fixed_width",
        }
    }
}

/// Configuration for [`TabularCompressor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabularCompressorConfig {
    /// SmartCrusher lossless compaction format (`"csv-schema"`, `"json"`, `"markdown-kv"`).
    pub compaction_format: String,
    /// Minimum character savings required to adopt SmartCrusher's output.
    pub min_savings_chars: usize,
}

impl Default for TabularCompressorConfig {
    fn default() -> Self {
        Self {
            compaction_format: "csv-schema".to_string(),
            min_savings_chars: 1,
        }
    }
}

/// Outcome of [`TabularCompressor::compress`].
#[derive(Debug, Clone, PartialEq)]
pub struct TabularCompressionResult {
    pub compressed: String,
    pub original: String,
    pub was_modified: bool,
    pub format: Option<TabularFormat>,
    pub rows: usize,
    pub columns: usize,
    pub strategy: String,
}

impl TabularCompressionResult {
    /// Byte length ratio (`compressed / original`).
    #[must_use]
    pub fn compression_ratio(&self) -> f64 {
        if self.original.is_empty() {
            return 0.0;
        }
        self.compressed.len() as f64 / self.original.len() as f64
    }
}

/// Parse delimited text (`csv` / `tsv`) into `(headers, rows)`.
#[must_use]
pub fn parse_csv(content: &str, delimiter: u8) -> (Vec<String>, Vec<Vec<String>>) {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(false)
        .flexible(true)
        .from_reader(content.as_bytes());

    let mut parsed: Vec<Vec<String>> = Vec::new();
    for record in reader.records() {
        let Ok(rec) = record else {
            return (Vec::new(), Vec::new());
        };
        let row: Vec<String> = rec.iter().map(str::to_string).collect();
        if row.iter().any(|c| c.len() > MAX_CSV_FIELD_BYTES) {
            return (Vec::new(), Vec::new());
        }
        if !row.is_empty() && !(row.len() == 1 && row[0].trim().is_empty()) {
            parsed.push(row);
        }
    }

    let Some(first_non_blank) = parsed
        .iter()
        .position(|r| r.iter().any(|c| !c.trim().is_empty()))
    else {
        return (Vec::new(), Vec::new());
    };
    let last_non_blank = parsed
        .iter()
        .rposition(|r| r.iter().any(|c| !c.trim().is_empty()))
        .unwrap_or(first_non_blank);

    let trimmed: Vec<Vec<String>> = parsed[first_non_blank..=last_non_blank]
        .iter()
        .map(|row| {
            if row.iter().any(|c| !c.trim().is_empty()) {
                row.clone()
            } else {
                vec![String::new(); row.len()]
            }
        })
        .collect();

    if trimmed.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let headers: Vec<String> = trimmed[0].iter().map(|h| h.trim().to_string()).collect();
    (headers, trimmed[1..].to_vec())
}

/// Parse a Markdown pipe table into `(headers, rows)`.
#[must_use]
pub fn parse_markdown_table(content: &str) -> (Vec<String>, Vec<Vec<String>>) {
    fn split_row(row: &str) -> Vec<String> {
        row.trim()
            .trim_matches('|')
            .split('|')
            .map(|c| c.trim().to_string())
            .collect()
    }

    fn is_separator(row: &str) -> bool {
        let cells: Vec<String> = split_row(row)
            .into_iter()
            .filter(|c| !c.is_empty())
            .collect();
        cells.len() >= 2 && cells.iter().all(|c| MD_SEP_CELL.is_match(c))
    }

    let lines: Vec<&str> = content
        .split('\n')
        .filter(|ln| !ln.trim().is_empty() && ln.contains('|'))
        .collect();
    if lines.len() < 2 {
        return (Vec::new(), Vec::new());
    }
    let headers = split_row(lines[0]);
    let mut body_start = 1usize;
    if body_start < lines.len() && is_separator(lines[body_start]) {
        body_start += 1;
    }
    let rows = lines[body_start..].iter().map(|ln| split_row(ln)).collect();
    (headers, rows)
}

/// Parse space-aligned fixed-width columns by empty gutter runs (`>= 2` spaces).
#[must_use]
pub fn parse_fixed_width(content: &str) -> (Vec<String>, Vec<Vec<String>>) {
    let lines: Vec<&str> = content
        .split('\n')
        .map(str::trim_end)
        .filter(|ln| !ln.trim().is_empty())
        .collect();
    if lines.len() < 2 || lines.iter().any(|ln| ln.contains('\t')) {
        return (Vec::new(), Vec::new());
    }
    // Use character-grid columns only when all lines are ASCII-column aligned or
    // compute max char count safely.
    let width = lines.iter().map(|ln| ln.chars().count()).max().unwrap_or(0);
    if width == 0 {
        return (Vec::new(), Vec::new());
    }
    let mut occupied = vec![false; width];
    for ln in &lines {
        for (idx, ch) in ln.chars().enumerate() {
            if ch != ' ' {
                occupied[idx] = true;
            }
        }
    }

    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    let mut blank_run = 0usize;
    for (i, filled) in occupied.iter().copied().chain([false, false]).enumerate() {
        if filled {
            if start.is_none() {
                start = Some(i);
            }
            blank_run = 0;
            continue;
        }
        blank_run += 1;
        if let Some(s) = start {
            if blank_run == 2 {
                spans.push((s, i - 1));
                start = None;
            }
        }
    }
    if let Some(s) = start {
        spans.push((s, width));
    }

    let extract_cells = |ln: &str| -> Vec<String> {
        let chars: Vec<char> = ln.chars().collect();
        spans
            .iter()
            .map(|&(a, b)| {
                let lo = a.min(chars.len());
                let hi = b.min(chars.len());
                chars[lo..hi].iter().collect::<String>().trim().to_string()
            })
            .collect()
    };

    let headers = extract_cells(lines[0]);
    if headers.iter().any(|h| h.is_empty() || h.contains("  ")) {
        return (Vec::new(), Vec::new());
    }
    let rows = lines[1..].iter().map(|ln| extract_cells(ln)).collect();
    (headers, rows)
}

/// Parse tabular text if valid and rectangular, rejecting ragged tables and `git_status`.
#[must_use]
pub fn parse_tabular(content: &str) -> Option<(Vec<String>, Vec<Vec<String>>, TabularFormat)> {
    let detection = detect_content_type(content);
    if detection.content_type != ContentType::Tabular {
        return None;
    }
    let raw_fmt = detection
        .metadata
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("csv");
    if raw_fmt == "git_status" {
        return None;
    }

    let (headers, rows, fmt) = match raw_fmt {
        "markdown" => {
            let (h, r) = parse_markdown_table(content);
            (h, r, TabularFormat::Markdown)
        }
        "fixed_width" => {
            let (h, r) = parse_fixed_width(content);
            (h, r, TabularFormat::FixedWidth)
        }
        _ => {
            let delim = detection
                .metadata
                .get("delimiter")
                .and_then(Value::as_str)
                .and_then(|s| s.bytes().next())
                .unwrap_or(b',');
            let (h, r) = parse_csv(content, delim);
            let fmt = if delim == b'\t' {
                TabularFormat::Tsv
            } else {
                TabularFormat::Csv
            };
            (h, r, fmt)
        }
    };

    if headers.is_empty() || rows.is_empty() {
        return None;
    }
    let width = headers.len();
    if rows.iter().any(|row| row.len() != width) {
        return None;
    }
    let unique: HashSet<&str> = headers.iter().map(String::as_str).collect();
    if unique.len() != width {
        return None;
    }
    if fmt == TabularFormat::FixedWidth && width < 2 {
        return None;
    }
    Some((headers, rows, fmt))
}

/// Convert parsed headers and rows into JSON objects preserving header order.
#[must_use]
pub fn to_records(headers: &[String], rows: &[Vec<String>]) -> Vec<Value> {
    if headers.is_empty() {
        return Vec::new();
    }
    let width = headers.len();
    rows.iter()
        .map(|row| {
            let mut obj = Map::with_capacity(width);
            for (idx, key) in headers.iter().enumerate() {
                let val = row.get(idx).cloned().unwrap_or_default();
                obj.insert(key.clone(), Value::String(val));
            }
            Value::Object(obj)
        })
        .collect()
}

/// Tabular-text compressor routing parsed tables through [`SmartCrusher`].
#[derive(Debug, Clone, Default)]
pub struct TabularCompressor {
    config: TabularCompressorConfig,
}

impl TabularCompressor {
    /// Construct a new [`TabularCompressor`] with `config`.
    #[must_use]
    pub fn new(config: TabularCompressorConfig) -> Self {
        Self { config }
    }

    /// Compress tabular `content` using `context` for relevance scoring.
    #[must_use]
    pub fn compress(&self, content: &str, context: &str) -> TabularCompressionResult {
        let Some((headers, rows, fmt)) = parse_tabular(content) else {
            return TabularCompressionResult {
                compressed: content.to_string(),
                original: content.to_string(),
                was_modified: false,
                format: None,
                rows: 0,
                columns: 0,
                strategy: "tabular".to_string(),
            };
        };

        let records = to_records(&headers, &rows);
        let Ok(json_str) = serde_json::to_string(&records) else {
            return TabularCompressionResult {
                compressed: content.to_string(),
                original: content.to_string(),
                was_modified: false,
                format: Some(fmt),
                rows: rows.len(),
                columns: headers.len(),
                strategy: "tabular".to_string(),
            };
        };

        let crusher = SmartCrusher::with_compaction_format(
            SmartCrusherConfig::default(),
            &self.config.compaction_format,
        )
        .unwrap_or_else(|| SmartCrusher::new(SmartCrusherConfig::default()));

        let mut result = crusher.crush(&json_str, context, 1.0);
        let mut savings = content.len().saturating_sub(result.compressed.len());
        if !result.was_modified || savings < self.config.min_savings_chars {
            // Lossless table re-rendering (e.g. csv-schema on already-compact CSV)
            // may not shrink below the original raw table; fall back to CCR
            // lossy sampling over the parsed records.
            let lossy_crusher = SmartCrusher::new(SmartCrusherConfig {
                lossless_min_savings_ratio: 1.0,
                ..SmartCrusherConfig::default()
            });
            let lossy_result = lossy_crusher.crush(&json_str, context, 1.0);
            let lossy_savings = content.len().saturating_sub(lossy_result.compressed.len());
            if lossy_result.was_modified && lossy_savings >= self.config.min_savings_chars {
                result = lossy_result;
                savings = lossy_savings;
            }
        }
        if !result.was_modified || savings < self.config.min_savings_chars {
            return TabularCompressionResult {
                compressed: content.to_string(),
                original: content.to_string(),
                was_modified: false,
                format: Some(fmt),
                rows: rows.len(),
                columns: headers.len(),
                strategy: "tabular".to_string(),
            };
        }

        TabularCompressionResult {
            compressed: result.compressed,
            original: content.to_string(),
            was_modified: true,
            format: Some(fmt),
            rows: rows.len(),
            columns: headers.len(),
            strategy: if result.strategy.is_empty() {
                "tabular".to_string()
            } else {
                result.strategy
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compresses_large_csv_via_smart_crusher() {
        let mut csv_data = String::from("id,service,status,region\n");
        for i in 0..80 {
            csv_data.push_str(&format!("{i},api-gateway,ok,us-east-1\n"));
        }
        let res = TabularCompressor::default().compress(&csv_data, "");
        assert_eq!(res.format, Some(TabularFormat::Csv));
        assert!(res.was_modified);
        assert!(res.compressed.len() < csv_data.len());
    }

    #[test]
    fn rejects_ragged_csv() {
        let ragged = "a,b,c\n1,2,3\n4,5\n6,7,8\n";
        assert!(parse_tabular(ragged).is_none());
    }

    #[test]
    fn parses_markdown_and_fixed_width() {
        let md = "| id | name |\n| --- | --- |\n| 1 | a |\n| 2 | b |\n| 3 | c |\n";
        let (_, rows, fmt) = parse_tabular(md).expect("markdown table");
        assert_eq!(fmt, TabularFormat::Markdown);
        assert_eq!(rows.len(), 3);

        let fw = "NAME       STATUS    RESTARTS\nweb-0      Running   0\nweb-1      Running   0\nweb-2      Running   0\n";
        let (headers, fw_rows, fw_fmt) = parse_tabular(fw).expect("fixed width");
        assert_eq!(fw_fmt, TabularFormat::FixedWidth);
        assert_eq!(headers, vec!["NAME", "STATUS", "RESTARTS"]);
        assert_eq!(fw_rows.len(), 3);
    }
}
