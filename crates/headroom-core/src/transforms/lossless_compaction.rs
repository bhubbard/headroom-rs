//! Format-native, reversible lossless compaction for text, logs, diffs, search, and configs.
//!
//! Direct port of `headroom/transforms/lossless_compaction.py`. Every helper keeps
//! output looking like its native content type without emitting CCR retrieval markers.
//! Reversible folds self-verify against their exact inverses before being adopted.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

/// Content category dispatched by [`compact_lossless`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LosslessKind {
    /// Build / application log lines (ANSI strip + syslog run collapse).
    Log,
    /// `grep` / `ripgrep` output (`file:line:content` -> heading or dir-heading).
    Search,
    /// Pure file-path listings (`find` / `ls -1` / `rg -l`).
    Paths,
    /// Unified diff bookkeeping (`index <sha>..<sha>` stripping).
    Diff,
    /// Plain prose / text run collapse.
    Text,
    /// Structured config (run collapse + repeated stanza folding).
    Config,
}

const LOSSLESS_COMPACTION_ENV: &str = "HEADROOM_LOSSLESS_COMPACTION";
const FOLD_MIN_BLOCK: usize = 3;
const FOLD_MAX_BLOCK: usize = 64;
const FOLD_MAX_CANDIDATES: usize = 8;
const FOLD_MAX_LINES: usize = 20_000;

static ANSI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*m").expect("valid ANSI regex"));
static RUN_MARKER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\.\.\. \(repeated (\d+) times\)$").expect("valid run marker"));
static BLOCK_MARKER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\.\.\. \(repeats (\d+) lines from (\d+) lines back\)$")
        .expect("valid block marker")
});
static GREP_ROW_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([^\n:]+):(\d+):(.*)$").expect("valid grep row"));
static HEADING_ROW_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d+):(.*)$").expect("valid heading row"));
static DIR_DATA_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([^/\\\n:]+):(\d+):(.*)$").expect("valid dir row"));
static TIMESTAMP_ROW_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(?:\[?(?:\d{4}-\d{2}-\d{2}[ T]\d{1,2}:\d{2}|\d{4}/\d{2}/\d{2}[ T]\d{1,2}:\d{2}|\d{2}/\d{2}/\d{2,4}[ T]\d{1,2}:\d{2}|[A-Z][a-z]{2}\s+\d{1,2}\s+\d{1,2}:\d{2}|[IWEF]\d{4}\s+\d{1,2}:\d{2}|[A-Za-z0-9_.-]+ [^\s:]+ [^\s:]+ \[\d{2}/[A-Z][a-z]{2}/\d{4}:\d{2}:\d{2}|\d{1,2}:\d{2}:\d{2}(?:[.,]\d+)?(?:\s|\]|$))|[IWEFDU], \[\d{4}-\d{2}-\d{2}T\d{2}:\d{2})",
    )
    .expect("valid timestamp guard")
});
static DIFF_INDEX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^index [0-9a-fA-F]+\.\.[0-9a-fA-F]+( [0-7]+)?$").expect("diff"));
static PATH_ROW_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^((?:\.{0,2}/)?(?:[^/\s:<>;]+/)+)([^/\s:<>;]+)$").expect("valid path row")
});

/// Remove ANSI CSI/SGR color and style escape sequences.
#[must_use]
pub fn strip_ansi(text: &str) -> String {
    ANSI_RE.replace_all(text, "").into_owned()
}

fn split_keep_trailing(text: &str) -> (Vec<&str>, bool) {
    if text.is_empty() {
        return (Vec::new(), false);
    }
    let had_trailing = text.ends_with('\n');
    let body = if had_trailing {
        &text[..text.len() - 1]
    } else {
        text
    };
    (body.split('\n').collect(), had_trailing)
}

fn join_lines(lines: &[String], had_trailing: bool) -> String {
    let mut out = lines.join("\n");
    if had_trailing {
        out.push('\n');
    }
    out
}

/// Collapse consecutive runs of `>= 2` identical lines (`... (repeated N times)`).
#[must_use]
pub fn collapse_runs(text: &str) -> String {
    let (lines, had_trailing) = split_keep_trailing(text);
    if lines.is_empty() {
        return text.to_string();
    }
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let mut j = i;
        while j + 1 < lines.len() && lines[j + 1] == lines[i] {
            j += 1;
        }
        let run_len = j - i + 1;
        out.push(lines[i].to_string());
        if run_len >= 2 {
            out.push(format!("... (repeated {run_len} times)"));
        }
        i = j + 1;
    }
    join_lines(&out, had_trailing)
}

/// Return `true` if `text` contains a run-collapse marker line.
#[must_use]
pub fn is_run_collapsed(text: &str) -> bool {
    text.split('\n').any(|ln| RUN_MARKER_RE.is_match(ln))
}

#[derive(Debug, Clone, Copy)]
struct OutputBudget {
    max_bytes: Option<usize>,
    used: usize,
    slack: usize,
}

impl OutputBudget {
    fn new(max_bytes: Option<usize>, had_trailing: bool) -> Self {
        Self {
            max_bytes,
            used: 0,
            slack: usize::from(!had_trailing),
        }
    }

    fn take(&mut self, bytes: usize) -> Result<(), ()> {
        let Some(limit) = self.max_bytes else {
            return Ok(());
        };
        self.used = self.used.saturating_add(bytes);
        if self.used > limit.saturating_add(self.slack) {
            return Err(());
        }
        Ok(())
    }
}

/// Exact inverse of [`collapse_runs`], returning `None` if expansion exceeds `max_bytes`.
#[must_use]
pub fn expand_runs(text: &str, max_bytes: Option<usize>) -> Option<String> {
    let (lines, had_trailing) = split_keep_trailing(text);
    if lines.is_empty() {
        return Some(text.to_string());
    }
    let mut budget = OutputBudget::new(max_bytes, had_trailing);
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if i + 1 < lines.len() {
            if let Some(cap) = RUN_MARKER_RE.captures(lines[i + 1]) {
                let count: usize = cap[1].parse().ok()?;
                budget
                    .take(count.saturating_mul(line.len().saturating_add(1)))
                    .ok()?;
                for _ in 0..count {
                    out.push(line.to_string());
                }
                i += 2;
                continue;
            }
        }
        budget.take(line.len().saturating_add(1)).ok()?;
        out.push(line.to_string());
        i += 1;
    }
    Some(join_lines(&out, had_trailing))
}

fn remember_pos<'a>(positions: &mut HashMap<&'a str, Vec<usize>>, line: &'a str, index: usize) {
    let bucket = positions.entry(line).or_default();
    bucket.push(index);
    if bucket.len() > FOLD_MAX_CANDIDATES {
        bucket.remove(0);
    }
}

/// Fold multi-line blocks (`>= 3` lines) that repeat earlier content into back-references.
#[must_use]
pub fn fold_repeated_blocks(text: &str) -> String {
    let (lines, had_trailing) = split_keep_trailing(text);
    let n = lines.len();
    if !(FOLD_MIN_BLOCK * 2..=FOLD_MAX_LINES).contains(&n) {
        return text.to_string();
    }
    let mut positions: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut out: Vec<String> = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        let mut best_len = 0usize;
        let mut best_dist = 0usize;
        if let Some(cands) = positions.get(lines[i]) {
            for &q in cands.iter().rev() {
                let max_len = FOLD_MAX_BLOCK.min(n - i).min(i - q);
                let mut length = 0usize;
                while length < max_len && lines[q + length] == lines[i + length] {
                    length += 1;
                }
                if length > best_len {
                    best_len = length;
                    best_dist = i - q;
                }
            }
        }
        if best_len >= FOLD_MIN_BLOCK {
            let marker = format!("... (repeats {best_len} lines from {best_dist} lines back)");
            let block_bytes: usize = lines[i..i + best_len].iter().map(|l| l.len() + 1).sum();
            if block_bytes > marker.len() + 1 {
                out.push(marker);
                for (offset, &line) in lines[i..i + best_len].iter().enumerate() {
                    remember_pos(&mut positions, line, i + offset);
                }
                i += best_len;
                continue;
            }
        }
        remember_pos(&mut positions, lines[i], i);
        out.push(lines[i].to_string());
        i += 1;
    }
    join_lines(&out, had_trailing)
}

/// Exact inverse of [`fold_repeated_blocks`], bounded by `max_bytes` when provided.
#[must_use]
pub fn unfold_repeated_blocks(text: &str, max_bytes: Option<usize>) -> Option<String> {
    let (lines, had_trailing) = split_keep_trailing(text);
    if lines.is_empty() {
        return Some(text.to_string());
    }
    let mut budget = OutputBudget::new(max_bytes, had_trailing);
    let mut out: Vec<String> = Vec::new();
    for line in lines {
        if let Some(cap) = BLOCK_MARKER_RE.captures(line) {
            let length: usize = cap[1].parse().ok()?;
            let dist: usize = cap[2].parse().ok()?;
            if dist <= out.len() && length <= dist {
                let start = out.len() - dist;
                let copy_bytes: usize =
                    out[start..start + length].iter().map(|l| l.len() + 1).sum();
                budget.take(copy_bytes).ok()?;
                let slice: Vec<String> = out[start..start + length].to_vec();
                out.extend(slice);
                continue;
            }
        }
        budget.take(line.len().saturating_add(1)).ok()?;
        out.push(line.to_string());
    }
    Some(join_lines(&out, had_trailing))
}

/// Convert `path:line:content` grep rows into `ripgrep --heading` format.
#[must_use]
pub fn search_heading(text: &str) -> String {
    let (lines, had_trailing) = split_keep_trailing(text);
    if lines.is_empty() {
        return text.to_string();
    }
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut current_path: Option<&str> = None;
    for line in lines {
        let matched = if TIMESTAMP_ROW_RE.is_match(line) {
            None
        } else {
            GREP_ROW_RE.captures(line)
        };
        if let Some(cap) = matched {
            let path = cap.get(1).map_or("", |m| m.as_str());
            let line_no = cap.get(2).map_or("", |m| m.as_str());
            let content = cap.get(3).map_or("", |m| m.as_str());
            if current_path != Some(path) {
                out.push(path.to_string());
                current_path = Some(path);
            }
            out.push(format!("{line_no}:{content}"));
        } else {
            out.push(line.to_string());
            current_path = None;
        }
    }
    join_lines(&out, had_trailing)
}

/// Exact inverse of [`search_heading`].
#[must_use]
pub fn search_unheading(text: &str) -> String {
    let (lines, had_trailing) = split_keep_trailing(text);
    if lines.is_empty() {
        return text.to_string();
    }
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut current_path: Option<&str> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let data = HEADING_ROW_RE.captures(line);
        if let (Some(path), Some(cap)) = (current_path, &data) {
            let line_no = cap.get(1).map_or("", |m| m.as_str());
            let content = cap.get(2).map_or("", |m| m.as_str());
            out.push(format!("{path}:{line_no}:{content}"));
            i += 1;
            continue;
        }
        if data.is_none() && i + 1 < lines.len() && HEADING_ROW_RE.is_match(lines[i + 1]) {
            current_path = Some(line);
            i += 1;
            continue;
        }
        current_path = None;
        out.push(line.to_string());
        i += 1;
    }
    join_lines(&out, had_trailing)
}

/// Fold `dir/file:line:content` grep rows by shared parent directory.
#[must_use]
pub fn search_dir_heading(text: &str) -> String {
    let (lines, had_trailing) = split_keep_trailing(text);
    if lines.is_empty() {
        return text.to_string();
    }
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut current_dir: Option<&str> = None;
    for line in lines {
        let matched = if TIMESTAMP_ROW_RE.is_match(line) {
            None
        } else {
            GREP_ROW_RE.captures(line)
        };
        if let Some(cap) = matched {
            let path = cap.get(1).map_or("", |m| m.as_str());
            if let Some(slash) = path.rfind(['/', '\\']) {
                let (dir_part, base) = path.split_at(slash + 1);
                let line_no = cap.get(2).map_or("", |m| m.as_str());
                let content = cap.get(3).map_or("", |m| m.as_str());
                if current_dir != Some(dir_part) {
                    out.push(dir_part.to_string());
                    current_dir = Some(dir_part);
                }
                out.push(format!("{base}:{line_no}:{content}"));
                continue;
            }
        }
        out.push(line.to_string());
        current_dir = None;
    }
    join_lines(&out, had_trailing)
}

/// Exact inverse of [`search_dir_heading`].
#[must_use]
pub fn search_dir_unheading(text: &str) -> String {
    let (lines, had_trailing) = split_keep_trailing(text);
    if lines.is_empty() {
        return text.to_string();
    }
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut current_dir: Option<&str> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(dir) = current_dir {
            if DIR_DATA_RE.is_match(line) {
                out.push(format!("{dir}{line}"));
                i += 1;
                continue;
            }
        }
        if (line.ends_with('/') || line.ends_with('\\'))
            && i + 1 < lines.len()
            && DIR_DATA_RE.is_match(lines[i + 1])
        {
            current_dir = Some(line);
            i += 1;
            continue;
        }
        current_dir = None;
        out.push(line.to_string());
        i += 1;
    }
    join_lines(&out, had_trailing)
}

/// Drop `index <sha>..<sha> [<mode>]` bookkeeping lines from unified diffs.
#[must_use]
pub fn diff_strip_index(text: &str) -> String {
    let (lines, had_trailing) = split_keep_trailing(text);
    if lines.is_empty() {
        return text.to_string();
    }
    let out: Vec<String> = lines
        .into_iter()
        .filter(|ln| !DIFF_INDEX_RE.is_match(ln))
        .map(str::to_string)
        .collect();
    join_lines(&out, had_trailing)
}

fn valid_dir_segments(dir: &str) -> bool {
    let trimmed = dir
        .strip_prefix("./")
        .or_else(|| dir.strip_prefix("../"))
        .or_else(|| dir.strip_prefix('/'))
        .unwrap_or(dir);
    trimmed
        .trim_end_matches('/')
        .split('/')
        .all(|seg| seg.chars().any(|c| c.is_alphanumeric() || c == '_'))
}

/// Fold pure file-path listings (`find` / `ls -1` / `rg -l`) by directory heading.
#[must_use]
pub fn path_heading(text: &str) -> String {
    let (lines, had_trailing) = split_keep_trailing(text);
    let mut parsed: Vec<Option<(&str, &str)>> = Vec::with_capacity(lines.len());
    let mut path_count = 0usize;
    for &ln in &lines {
        if ln.is_empty() {
            parsed.push(None);
            continue;
        }
        let Some(cap) = PATH_ROW_RE.captures(ln) else {
            return text.to_string();
        };
        let dir = cap.get(1).map_or("", |m| m.as_str());
        let base = cap.get(2).map_or("", |m| m.as_str());
        if !valid_dir_segments(dir) {
            return text.to_string();
        }
        path_count += 1;
        parsed.push(Some((dir, base)));
    }
    if path_count < 2 {
        return text.to_string();
    }
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut current: Option<&str> = None;
    for (line, entry) in lines.iter().zip(parsed) {
        if let Some((dir, base)) = entry {
            if current != Some(dir) {
                out.push(dir.to_string());
                current = Some(dir);
            }
            out.push(base.to_string());
        } else {
            out.push((*line).to_string());
            current = None;
        }
    }
    join_lines(&out, had_trailing)
}

/// Exact inverse of [`path_heading`].
#[must_use]
pub fn path_unheading(text: &str) -> String {
    let (lines, had_trailing) = split_keep_trailing(text);
    if lines.is_empty() {
        return text.to_string();
    }
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut current: Option<&str> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let is_base = !line.is_empty() && !line.contains('/');
        if let Some(dir) = current {
            if is_base {
                out.push(format!("{dir}{line}"));
                i += 1;
                continue;
            }
        }
        if line.ends_with('/')
            && i + 1 < lines.len()
            && !lines[i + 1].is_empty()
            && !lines[i + 1].contains('/')
        {
            current = Some(line);
            i += 1;
            continue;
        }
        current = None;
        out.push(line.to_string());
        i += 1;
    }
    join_lines(&out, had_trailing)
}

fn lossless_compaction_enabled() -> bool {
    match std::env::var(LOSSLESS_COMPACTION_ENV) {
        Ok(raw) => !matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => true,
    }
}

/// Run format-native lossless compaction for `kind`, verifying exact round-trips.
///
/// Honors `HEADROOM_LOSSLESS_COMPACTION=0` to disable folds at runtime. If
/// verification fails or the output is not strictly shorter in bytes, `content`
/// is returned unchanged.
#[must_use]
pub fn compact_lossless(content: &str, kind: LosslessKind) -> String {
    if content.is_empty() || !lossless_compaction_enabled() {
        return content.to_string();
    }
    match kind {
        LosslessKind::Log => {
            let baseline = strip_ansi(content);
            let candidate = collapse_runs(&baseline);
            if expand_runs(&candidate, Some(baseline.len())).as_deref() != Some(&baseline) {
                return content.to_string();
            }
            if candidate.len() < content.len() {
                candidate
            } else {
                content.to_string()
            }
        }
        LosslessKind::Search => {
            let mut best = content.to_string();
            let file_cand = search_heading(content);
            if file_cand.len() < best.len() && search_unheading(&file_cand) == content {
                best = file_cand;
            }
            let dir_cand = search_dir_heading(content);
            if dir_cand.len() < best.len() && search_dir_unheading(&dir_cand) == content {
                best = dir_cand;
            }
            best
        }
        LosslessKind::Paths => {
            let candidate = path_heading(content);
            if candidate.len() < content.len() && path_unheading(&candidate) == content {
                candidate
            } else {
                content.to_string()
            }
        }
        LosslessKind::Diff => {
            let candidate = diff_strip_index(content);
            if candidate.len() < content.len() {
                candidate
            } else {
                content.to_string()
            }
        }
        LosslessKind::Text => {
            let candidate = collapse_runs(content);
            if candidate.len() < content.len()
                && expand_runs(&candidate, Some(content.len())).as_deref() == Some(content)
            {
                candidate
            } else {
                content.to_string()
            }
        }
        LosslessKind::Config => {
            let collapsed = collapse_runs(content);
            let candidate = fold_repeated_blocks(&collapsed);
            if candidate.len() >= content.len() {
                return content.to_string();
            }
            let Some(unfolded) = unfold_repeated_blocks(&candidate, Some(collapsed.len())) else {
                return content.to_string();
            };
            if expand_runs(&unfolded, Some(content.len())).as_deref() == Some(content) {
                candidate
            } else {
                content.to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syslog_runs_roundtrip() {
        let input = "INFO ok\nINFO ok\nINFO ok\nINFO ok\nWARN done\n";
        let compressed = compact_lossless(input, LosslessKind::Log);
        assert!(compressed.contains("... (repeated 4 times)"));
        assert_eq!(expand_runs(&compressed, None).as_deref(), Some(input));
    }

    #[test]
    fn search_heading_never_folds_timestamps() {
        let logs = "2026-09-02 14:30:00 [FATAL] db down\n2026-09-02 14:30:01 [FATAL] retry\n";
        assert_eq!(search_heading(logs), logs);
        assert_eq!(compact_lossless(logs, LosslessKind::Search), logs);
    }

    #[test]
    fn search_dir_and_file_headings_roundtrip() {
        let grep = "src/api/auth.rs:10:fn login() {}\nsrc/api/auth.rs:24:fn logout() {}\nsrc/api/auth.rs:40:fn refresh() {}\n";
        let folded = compact_lossless(grep, LosslessKind::Search);
        assert!(folded.len() < grep.len());
        assert_eq!(search_unheading(&folded), grep);
    }

    #[test]
    fn config_repeated_blocks_roundtrip() {
        let yaml = "\
containers:
  - name: web-1
    image: nginx:1.27
    port: 8080
    protocol: TCP
    restart: Always
  - name: web-2
    image: nginx:1.27
    port: 8080
    protocol: TCP
    restart: Always
";
        let folded = compact_lossless(yaml, LosslessKind::Config);
        assert!(folded.len() < yaml.len(), "expected block fold: {folded}");
        assert!(folded.contains("repeats"));
        let unfolded = unfold_repeated_blocks(&folded, Some(yaml.len())).unwrap();
        assert_eq!(unfolded, yaml);
    }

    #[test]
    fn diff_index_strip_works() {
        let diff = "diff --git a/a.rs b/a.rs\nindex 1234567..89abcdef 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n";
        let stripped = compact_lossless(diff, LosslessKind::Diff);
        assert!(!stripped.contains("index 1234567"));
        assert!(stripped.contains("+new"));
    }

    #[test]
    fn path_heading_roundtrips_cleanly() {
        let paths = "src/transforms/a.rs\nsrc/transforms/b.rs\nsrc/transforms/c.rs\n";
        let folded = compact_lossless(paths, LosslessKind::Paths);
        assert!(folded.len() < paths.len());
        assert_eq!(path_unheading(&folded), paths);
    }
}
