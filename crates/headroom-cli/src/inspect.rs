//! `headroom inspect` — pure-Rust unified diffs over proxy feed or local file compression.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use headroom_proxy::TransformationRecord;
use serde_json::json;

use crate::compress::{compress_payload, CliCompressMode};

/// Arguments for `headroom inspect`.
#[derive(Debug, Args)]
pub struct InspectArgs {
    /// Proxy port to fetch `/transformations/feed` from.
    #[arg(long, default_value_t = 8787)]
    pub port: u16,

    /// Compress and diff a local file instead of querying the proxy feed.
    #[arg(long)]
    pub input: Option<PathBuf>,

    /// Maximum number of recent transformations to print from `/transformations/feed`.
    #[arg(long, default_value_t = 5)]
    pub limit: usize,

    /// Model name used when `--input` is given.
    #[arg(long, default_value = "gpt-4o")]
    pub model: String,

    /// Emit raw JSON instead of unified diff text.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffOp<'a> {
    Equal(&'a str),
    Remove(&'a str),
    Add(&'a str),
}

fn split_lines(s: &str) -> Vec<&str> {
    if s.is_empty() {
        Vec::new()
    } else {
        s.lines().collect()
    }
}

/// Render a pure-Rust LCS line-based unified diff (`--- before` / `+++ after`).
#[must_use]
pub fn unified_diff(before: &str, after: &str, context_lines: usize) -> String {
    if before == after {
        return String::from("(no changes)\n");
    }
    let a = split_lines(before);
    let b = split_lines(after);
    let n = a.len();
    let m = b.len();

    let ops: Vec<DiffOp<'_>> = if n.saturating_mul(m) > 4_000_000 {
        let mut fallback = Vec::with_capacity(n + m);
        for line in &a {
            fallback.push(DiffOp::Remove(line));
        }
        for line in &b {
            fallback.push(DiffOp::Add(line));
        }
        fallback
    } else {
        let cols = m + 1;
        let mut dp = vec![0u32; (n + 1) * cols];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                dp[i * cols + j] = if a[i] == b[j] {
                    dp[(i + 1) * cols + (j + 1)] + 1
                } else {
                    dp[(i + 1) * cols + j].max(dp[i * cols + (j + 1)])
                };
            }
        }
        let mut out = Vec::new();
        let mut i = 0usize;
        let mut j = 0usize;
        while i < n && j < m {
            if a[i] == b[j] {
                out.push(DiffOp::Equal(a[i]));
                i += 1;
                j += 1;
            } else if dp[(i + 1) * cols + j] >= dp[i * cols + (j + 1)] {
                out.push(DiffOp::Remove(a[i]));
                i += 1;
            } else {
                out.push(DiffOp::Add(b[j]));
                j += 1;
            }
        }
        while i < n {
            out.push(DiffOp::Remove(a[i]));
            i += 1;
        }
        while j < m {
            out.push(DiffOp::Add(b[j]));
            j += 1;
        }
        out
    };

    let change_indices: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter_map(|(idx, op)| (!matches!(op, DiffOp::Equal(_))).then_some(idx))
        .collect();
    if change_indices.is_empty() {
        return String::from("(no changes)\n");
    }

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for idx in change_indices {
        let start = idx.saturating_sub(context_lines);
        let end = (idx + context_lines).min(ops.len() - 1);
        if let Some(last) = ranges.last_mut() {
            if start <= last.1 + 1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        ranges.push((start, end));
    }

    let mut out = String::from("--- before\n+++ after\n");
    for (start, end) in ranges {
        let before_start = ops[..start]
            .iter()
            .filter(|op| !matches!(op, DiffOp::Add(_)))
            .count()
            + 1;
        let after_start = ops[..start]
            .iter()
            .filter(|op| !matches!(op, DiffOp::Remove(_)))
            .count()
            + 1;
        let before_count = ops[start..=end]
            .iter()
            .filter(|op| !matches!(op, DiffOp::Add(_)))
            .count();
        let after_count = ops[start..=end]
            .iter()
            .filter(|op| !matches!(op, DiffOp::Remove(_)))
            .count();
        out.push_str(&format!(
            "@@ -{before_start},{before_count} +{after_start},{after_count} @@\n"
        ));
        for op in &ops[start..=end] {
            match op {
                DiffOp::Equal(line) => {
                    out.push(' ');
                    out.push_str(line);
                    out.push('\n');
                }
                DiffOp::Remove(line) => {
                    out.push('-');
                    out.push_str(line);
                    out.push('\n');
                }
                DiffOp::Add(line) => {
                    out.push('+');
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// Execute `headroom inspect`.
pub async fn run(args: InspectArgs) -> Result<()> {
    if let Some(ref path) = args.input {
        let before = std::fs::read_to_string(path)
            .with_context(|| format!("failed reading {}", path.display()))?;
        let res = compress_payload(&before, &args.model, CliCompressMode::Auto, None, None)?;
        let diff = unified_diff(&before, &res.compressed, 3);
        if args.json {
            let obj = json!({
                "file": path.display().to_string(),
                "tokens_before": res.tokens_before,
                "tokens_after": res.tokens_after,
                "tokens_saved": res.tokens_saved,
                "strategy": res.strategy,
                "diff": diff,
            });
            println!("{}", serde_json::to_string_pretty(&obj)?);
        } else {
            println!(
                "=== {} ({} -> {} tokens, {} saved, strategy={}) ===",
                path.display(),
                res.tokens_before,
                res.tokens_after,
                res.tokens_saved,
                res.strategy
            );
            print!("{diff}");
        }
        return Ok(());
    }

    let url = format!("http://127.0.0.1:{}/transformations/feed", args.port);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;
    let resp = client.get(&url).send().await.with_context(|| {
        format!("failed connecting to {url} (start proxy with `--log-messages`)")
    })?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "{url} returned HTTP {} (ensure proxy started with `--log-messages`)",
            resp.status()
        );
    }

    let body: serde_json::Value = resp.json().await?;
    let items: Vec<TransformationRecord> =
        serde_json::from_value(body["items"].clone()).unwrap_or_default();

    if args.json {
        let slice_start = items.len().saturating_sub(args.limit);
        println!("{}", serde_json::to_string_pretty(&items[slice_start..])?);
        return Ok(());
    }

    if items.is_empty() {
        println!("No transformations recorded yet.");
        return Ok(());
    }

    for rec in items.iter().rev().take(args.limit) {
        let saved = rec.tokens_before.saturating_sub(rec.tokens_after);
        println!(
            "=== [{}] {} {} -> {} tokens ({saved} saved, strategies={}) ===",
            rec.timestamp,
            rec.endpoint,
            rec.tokens_before,
            rec.tokens_after,
            rec.strategies.join(",")
        );
        print!("{}", unified_diff(&rec.before_body, &rec.after_body, 3));
    }

    Ok(())
}
