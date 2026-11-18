//! Structural (recursive) JSON span scanner and splicer for embedded JSON payloads.
//!
//! Port of `headroom/transforms/recursive_json.py`. Locates balanced JSON object/array
//! byte spans inside larger text payloads (e.g. `gh api` or `curl` wrappers), routes
//! record-array spans through a caller-supplied dispatcher, and splices strictly smaller
//! replacements back into place while leaving surrounding bytes exact.

use std::collections::HashMap;

use serde_json::Value;

const SCAN_BUDGET_PER_CHAR: usize = 4;
const SCAN_BUDGET_FLOOR: usize = 4096;

fn matching_open(close: u8) -> u8 {
    if close == b'}' {
        b'{'
    } else {
        b'['
    }
}

fn scan_from(
    bytes: &[u8],
    start: usize,
    known: &mut HashMap<usize, Option<usize>>,
) -> (Option<usize>, usize) {
    let mut stack: Vec<usize> = Vec::new();
    let mut in_str = false;
    let mut esc = false;
    for (j, &ch) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if esc {
                esc = false;
            } else if ch == b'\\' {
                esc = true;
            } else if ch == b'"' {
                in_str = false;
            }
            continue;
        }
        if ch == b'"' {
            in_str = true;
        } else if ch == b'{' || ch == b'[' {
            stack.push(j);
        } else if ch == b'}' || ch == b']' {
            let Some(&top) = stack.last() else {
                for p in stack {
                    known.insert(p, None);
                }
                return (None, j - start + 1);
            };
            if bytes[top] != matching_open(ch) {
                for p in stack {
                    known.insert(p, None);
                }
                return (None, j - start + 1);
            }
            stack.pop();
            known.insert(top, Some(j + 1));
            if stack.is_empty() {
                return (Some(j + 1), j - start + 1);
            }
        }
    }
    for p in stack {
        known.insert(p, None);
    }
    (None, bytes.len() - start)
}

fn scan_spans(text: &str, include_nested: bool) -> (Vec<(usize, usize)>, bool) {
    let bytes = text.as_bytes();
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut known: HashMap<usize, Option<usize>> = HashMap::new();
    let budget = SCAN_BUDGET_PER_CHAR
        .saturating_mul(bytes.len())
        .saturating_add(SCAN_BUDGET_FLOOR);
    let mut spent = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let ch = bytes[i];
        if ch == b'{' || ch == b'[' {
            let end = if let Some(&cached) = known.get(&i) {
                cached
            } else if spent >= budget {
                return (out, false);
            } else {
                let (e, walked) = scan_from(bytes, i, &mut known);
                spent = spent.saturating_add(walked);
                e
            };
            if let Some(end_idx) = end {
                out.push((i, end_idx));
                i = end_idx;
                continue;
            }
        }
        i += 1;
    }
    if include_nested {
        out.clear();
        for start in 0..bytes.len() {
            if let Some(Some(end_idx)) = known.get(&start) {
                out.push((start, *end_idx));
            }
        }
    }
    (out, true)
}

/// Return byte spans `(start, end)` of top-level balanced JSON objects or arrays in `text`.
#[must_use]
pub fn json_document_spans(text: &str) -> Vec<(usize, usize)> {
    scan_json_documents(text).0
}

/// Scan `text` for valid JSON object/array byte spans, returning `(spans, complete)`.
///
/// `complete` is `false` when the linear scan or cumulative parse budget is exhausted.
#[must_use]
pub fn scan_json_documents(text: &str) -> (Vec<(usize, usize)>, bool) {
    let (spans, complete) = scan_spans(text, true);
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut covered_until = 0usize;
    let mut parsed_bytes = 0usize;
    let parse_budget = SCAN_BUDGET_PER_CHAR
        .saturating_mul(text.len())
        .saturating_add(SCAN_BUDGET_FLOOR);
    for (a, b) in spans {
        if a < covered_until {
            continue;
        }
        let span_len = b.saturating_sub(a);
        if parsed_bytes.saturating_add(span_len) > parse_budget {
            return (out, false);
        }
        parsed_bytes += span_len;
        let Some(slice) = text.get(a..b) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<Value>(slice) else {
            continue;
        };
        if parsed.is_object() || parsed.is_array() {
            out.push((a, b));
            covered_until = b;
        }
    }
    (out, complete)
}

/// Return `true` when `span` parses as JSON containing an array of objects (`len >= 2`, `>= 80%` dicts).
#[must_use]
pub fn carries_record_array(span: &str) -> bool {
    let Ok(val) = serde_json::from_str::<Value>(span) else {
        return false;
    };
    fn walk(v: &Value) -> bool {
        match v {
            Value::Array(items) => {
                if items.len() >= 2 {
                    let dicts = items.iter().filter(|e| e.is_object()).count();
                    if (dicts as f64) >= 0.8 * (items.len() as f64) {
                        return true;
                    }
                }
                items.iter().any(walk)
            }
            Value::Object(map) => map.values().any(walk),
            _ => false,
        }
    }
    walk(&val)
}

/// Splice smaller dispatched JSON replacements into `content`, preserving all surrounding bytes.
///
/// Returns `Some((rewritten, all_reversible))` only when at least one span shrank and total tokens decreased.
pub fn route_embedded_json<D, T>(
    content: &str,
    mut dispatch: D,
    count_tokens: T,
) -> Option<(String, bool)>
where
    D: FnMut(&str) -> Option<(String, bool)>,
    T: Fn(&str) -> usize,
{
    let (spans, _) = scan_spans(content, false);
    if spans.is_empty() {
        return None;
    }
    let trimmed = content.trim();
    if spans.len() == 1 {
        let (a, b) = spans[0];
        if content.get(a..b) == Some(trimmed) {
            return None;
        }
    }

    let mut repls: Vec<(usize, usize, String)> = Vec::new();
    let mut reversible = true;
    for (a, b) in spans {
        let Some(chunk) = content.get(a..b) else {
            continue;
        };
        if chunk.contains("<<ccr:") || !carries_record_array(chunk) {
            continue;
        }
        let Some((out, span_rev)) = dispatch(chunk) else {
            continue;
        };
        if out == chunk {
            continue;
        }
        if count_tokens(&out) < count_tokens(chunk) {
            repls.push((a, b, out));
            reversible = reversible && span_rev;
        }
    }

    if repls.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(content.len());
    let mut last = 0usize;
    for (a, b, repl) in repls {
        out.push_str(&content[last..a]);
        out.push_str(&repl);
        last = b;
    }
    out.push_str(&content[last..]);

    if count_tokens(&out) < count_tokens(content) {
        Some((out, reversible))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_embedded_json_and_preserves_utf8_wrapper() {
        let payload = "Exit code: 0\nλ result:\n[{\"id\": 1, \"v\": \"ok\"}, {\"id\": 2, \"v\": \"ok\"}]\nDone.\n";
        let spans = json_document_spans(payload);
        assert_eq!(spans.len(), 1);
        let (a, b) = spans[0];
        assert!(carries_record_array(&payload[a..b]));
    }

    #[test]
    fn route_embedded_json_splices_only_target_span() {
        let wrapped =
            "Header line\n[{\"a\":1,\"b\":2},{\"a\":3,\"b\":4},{\"a\":5,\"b\":6}]\nFooter line";
        let res = route_embedded_json(
            wrapped,
            |chunk| Some((format!("[{}]", &chunk[1..10]), true)),
            |s| s.len().div_ceil(4),
        );
        let (rewritten, rev) = res.expect("spliced");
        assert!(rev);
        assert!(rewritten.starts_with("Header line\n"));
        assert!(rewritten.ends_with("\nFooter line"));
    }

    #[test]
    fn skips_ccr_marked_chunks() {
        let wrapped =
            "Prefix [{\"a\":1,\"note\":\"<<ccr:0123456789abcdef01234567>>\"},{\"a\":2}] Suffix";
        let res = route_embedded_json(wrapped, |_| Some(("[]".into(), true)), |s| s.len());
        assert!(res.is_none());
    }
}
