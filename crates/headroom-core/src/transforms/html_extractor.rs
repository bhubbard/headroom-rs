//! Pure-Rust HTML main-content extractor for web scraping tool outputs.
//!
//! Port of `headroom/transforms/html_extractor.py`. Strips scripts, styles,
//! navigation, footers, asides, and HTML comments while converting headings,
//! paragraphs, lists, links, code blocks, and tables into clean Markdown or
//! plain text.

/// Output serialization format for [`HtmlExtractor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HtmlOutputFormat {
    /// Emit lightweight Markdown headings, lists, links, and tables.
    #[default]
    Markdown,
    /// Emit clean plain text without Markdown punctuation.
    PlainText,
}

/// Whether hyperlink URLs are kept in extracted output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkPolicy {
    /// Emit `[text](url)` in Markdown mode (or `text` in PlainText mode).
    #[default]
    Include,
    /// Emit only the anchor text without URLs.
    Strip,
}

/// Whether HTML `<table>` elements are extracted into output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TablePolicy {
    /// Convert tables into Markdown pipe tables or tab/space rows.
    #[default]
    Include,
    /// Skip table elements completely.
    Strip,
}

/// Configuration for [`HtmlExtractor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlExtractorConfig {
    /// Target output format (Markdown or PlainText).
    pub output_format: HtmlOutputFormat,
    /// Policy for `<a href="...">` links.
    pub links: LinkPolicy,
    /// Policy for `<table>` elements.
    pub tables: TablePolicy,
    /// Whether to collect `<title>` and `<meta>` tags.
    pub extract_metadata: bool,
}

impl Default for HtmlExtractorConfig {
    fn default() -> Self {
        Self {
            output_format: HtmlOutputFormat::Markdown,
            links: LinkPolicy::Include,
            tables: TablePolicy::Include,
            extract_metadata: true,
        }
    }
}

/// Extracted HTML document metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HtmlMetadata {
    pub title: Option<String>,
    pub author: Option<String>,
    pub date: Option<String>,
    pub sitename: Option<String>,
    pub description: Option<String>,
}

/// Output produced by [`HtmlExtractor::extract`].
#[derive(Debug, Clone, PartialEq)]
pub struct HtmlExtractionResult {
    pub extracted: String,
    pub original_length: usize,
    pub extracted_length: usize,
    pub compression_ratio: f64,
    pub metadata: HtmlMetadata,
}

impl HtmlExtractionResult {
    /// Percentage of bytes removed relative to the original HTML.
    #[must_use]
    pub fn reduction_percent(&self) -> f64 {
        if self.original_length == 0 {
            return 0.0;
        }
        (1.0 - self.compression_ratio) * 100.0
    }
}

/// Pure-Rust HTML content extractor.
#[derive(Debug, Clone, Default)]
pub struct HtmlExtractor {
    config: HtmlExtractorConfig,
}

impl HtmlExtractor {
    /// Create a new HTML extractor with `config`.
    #[must_use]
    pub fn new(config: HtmlExtractorConfig) -> Self {
        Self { config }
    }

    /// Extract clean readable content and metadata from `html`.
    #[must_use]
    pub fn extract(&self, html: &str, base_url: Option<&str>) -> HtmlExtractionResult {
        let original_length = html.len();
        if html.trim().is_empty() {
            return HtmlExtractionResult {
                extracted: String::new(),
                original_length,
                extracted_length: 0,
                compression_ratio: 0.0,
                metadata: HtmlMetadata::default(),
            };
        }

        let mut parser = ParserState::new(&self.config, base_url);
        parser.run(html);
        let extracted = normalize_whitespace(&parser.out);
        let extracted_length = extracted.len();
        let compression_ratio = extracted_length as f64 / original_length.max(1) as f64;

        HtmlExtractionResult {
            extracted,
            original_length,
            extracted_length,
            compression_ratio,
            metadata: parser.metadata,
        }
    }
}

/// Heuristic check matching Python's `is_html_content`.
#[must_use]
pub fn is_html_content(content: &str) -> bool {
    if content.trim().is_empty() {
        return false;
    }
    let sample_end = floor_char_boundary(content, 2000);
    let lower = content[..sample_end].trim_start().to_ascii_lowercase();
    if lower.starts_with("<!doctype html") || lower.starts_with("<html") {
        return true;
    }
    const INDICATORS: [&str; 8] = [
        "<head",
        "<body",
        "<div",
        "<script",
        "<style",
        "<meta",
        "<link",
        "<!doctype",
    ];
    INDICATORS.iter().filter(|tag| lower.contains(*tag)).count() >= 2
}

fn floor_char_boundary(s: &str, max_bytes: usize) -> usize {
    if max_bytes >= s.len() {
        return s.len();
    }
    let mut idx = max_bytes;
    while !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

struct ParserState<'a> {
    config: &'a HtmlExtractorConfig,
    base_url: Option<&'a str>,
    out: String,
    metadata: HtmlMetadata,
    skip_stack: Vec<String>,
    in_title: bool,
    title_buf: String,
    in_pre: bool,
    in_link: bool,
    link_href: Option<String>,
    link_text: String,
    in_table: bool,
    table_rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    in_cell: bool,
    cell_buf: String,
}

impl<'a> ParserState<'a> {
    fn new(config: &'a HtmlExtractorConfig, base_url: Option<&'a str>) -> Self {
        Self {
            config,
            base_url,
            out: String::new(),
            metadata: HtmlMetadata::default(),
            skip_stack: Vec::new(),
            in_title: false,
            title_buf: String::new(),
            in_pre: false,
            in_link: false,
            link_href: None,
            link_text: String::new(),
            in_table: false,
            table_rows: Vec::new(),
            current_row: Vec::new(),
            in_cell: false,
            cell_buf: String::new(),
        }
    }

    fn run(&mut self, html: &str) {
        let mut cursor = 0usize;
        let bytes = html.as_bytes();
        while cursor < bytes.len() {
            if bytes[cursor] == b'<' {
                if html[cursor..].starts_with("<!--") {
                    if let Some(end) = html[cursor + 4..].find("-->") {
                        cursor += 4 + end + 3;
                        continue;
                    }
                    break;
                }
                if let Some(tag_end) = find_tag_close(html, cursor + 1) {
                    let raw_tag = &html[cursor + 1..tag_end];
                    self.handle_tag(raw_tag);
                    cursor = tag_end + 1;
                    continue;
                }
            }
            let next_lt = html[cursor..]
                .find('<')
                .map_or(html.len(), |rel| cursor + rel);
            let text_slice = &html[cursor..next_lt];
            self.handle_text(text_slice);
            cursor = next_lt.max(cursor + 1);
        }
    }

    fn handle_text(&mut self, raw: &str) {
        let decoded = decode_entities(raw);
        if self.in_title {
            self.title_buf.push_str(&decoded);
            return;
        }
        if !self.skip_stack.is_empty() {
            return;
        }
        if self.in_cell {
            self.cell_buf.push_str(&decoded);
            return;
        }
        if self.in_link {
            self.link_text.push_str(&decoded);
            return;
        }
        if self.in_pre {
            self.out.push_str(&decoded);
        } else {
            let collapsed = collapse_inline_spaces(&decoded);
            if !collapsed.is_empty()
                && (collapsed != " " || (!self.out.ends_with([' ', '\n']) && !self.out.is_empty()))
            {
                self.out.push_str(&collapsed);
            }
        }
    }

    fn handle_tag(&mut self, raw_tag: &str) {
        let trimmed = raw_tag.trim();
        if trimmed.is_empty() || trimmed.starts_with('!') || trimmed.starts_with('?') {
            return;
        }
        let is_close = trimmed.starts_with('/');
        let self_closing = trimmed.ends_with('/');
        let inner = trimmed.trim_start_matches('/').trim_end_matches('/').trim();
        let mut parts = inner.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("").to_ascii_lowercase();
        let attrs = parts.next().unwrap_or("");
        if name.is_empty() {
            return;
        }

        if is_close {
            self.handle_close_tag(&name);
            return;
        }

        if name == "meta" {
            if self.config.extract_metadata {
                self.capture_meta(attrs);
            }
            return;
        }
        if name == "title" && self.skip_stack.is_empty() && !self_closing {
            self.in_title = true;
            self.title_buf.clear();
            return;
        }
        if is_ignored_container(&name) {
            if !self_closing {
                self.skip_stack.push(name);
            }
            return;
        }
        if !self.skip_stack.is_empty() {
            return;
        }

        match name.as_str() {
            "br" | "hr" => self.push_newline(),
            "p" | "div" | "section" | "article" | "main" | "blockquote" | "tr" => {
                if name == "tr" && self.in_table {
                    self.current_row.clear();
                } else {
                    self.ensure_blank_line();
                }
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.ensure_blank_line();
                if self.config.output_format == HtmlOutputFormat::Markdown {
                    let level = name[1..].parse::<usize>().unwrap_or(1).clamp(1, 6);
                    for _ in 0..level {
                        self.out.push('#');
                    }
                    self.out.push(' ');
                }
            }
            "li" => {
                self.push_newline();
                self.out.push_str("- ");
            }
            "pre" => {
                self.ensure_blank_line();
                if self.config.output_format == HtmlOutputFormat::Markdown {
                    self.out.push_str("```\n");
                }
                self.in_pre = true;
            }
            "code" if !self.in_pre && self.config.output_format == HtmlOutputFormat::Markdown => {
                self.out.push('`');
            }
            "a" if !self_closing => {
                self.in_link = true;
                self.link_href = extract_attr(attrs, "href");
                self.link_text.clear();
            }
            "table" if !self_closing => {
                if self.config.tables == TablePolicy::Strip {
                    self.skip_stack.push(name);
                } else {
                    self.in_table = true;
                    self.table_rows.clear();
                }
            }
            "th" | "td" if self.in_table && !self_closing => {
                self.in_cell = true;
                self.cell_buf.clear();
            }
            _ => {}
        }
    }

    fn handle_close_tag(&mut self, name: &str) {
        if name == "title" && self.in_title {
            self.in_title = false;
            if self.config.extract_metadata && self.metadata.title.is_none() {
                let t = collapse_inline_spaces(self.title_buf.trim());
                if !t.is_empty() {
                    self.metadata.title = Some(t);
                }
            }
            self.title_buf.clear();
            return;
        }
        if let Some(pos) = self.skip_stack.iter().rposition(|t| t == name) {
            self.skip_stack.truncate(pos);
            return;
        }
        if !self.skip_stack.is_empty() {
            return;
        }

        match name {
            "p" | "div" | "section" | "article" | "main" | "blockquote" | "h1" | "h2" | "h3"
            | "h4" | "h5" | "h6" | "ul" | "ol" => {
                self.ensure_blank_line();
            }
            "li" => self.push_newline(),
            "pre" => {
                if !self.out.ends_with('\n') {
                    self.out.push('\n');
                }
                if self.config.output_format == HtmlOutputFormat::Markdown {
                    self.out.push_str("```\n");
                }
                self.in_pre = false;
            }
            "code" if !self.in_pre && self.config.output_format == HtmlOutputFormat::Markdown => {
                self.out.push('`');
            }
            "a" if self.in_link => {
                let text = collapse_inline_spaces(self.link_text.trim());
                let href = self.link_href.take();
                self.in_link = false;
                self.link_text.clear();
                if text.is_empty() {
                    return;
                }
                if self.config.links == LinkPolicy::Include
                    && self.config.output_format == HtmlOutputFormat::Markdown
                {
                    if let Some(raw_href) = href.filter(|h| !h.trim().is_empty()) {
                        let resolved = resolve_href(self.base_url, raw_href.trim());
                        self.out.push_str(&format!("[{text}]({resolved})"));
                        return;
                    }
                }
                self.out.push_str(&text);
            }
            "th" | "td" if self.in_table && self.in_cell => {
                let cell = collapse_inline_spaces(self.cell_buf.trim()).replace('|', "\\|");
                self.current_row.push(cell);
                self.in_cell = false;
                self.cell_buf.clear();
            }
            "tr" if self.in_table => {
                if !self.current_row.is_empty() {
                    self.table_rows.push(std::mem::take(&mut self.current_row));
                }
            }
            "table" if self.in_table => {
                self.in_table = false;
                self.flush_table();
            }
            _ => {}
        }
    }

    fn flush_table(&mut self) {
        if self.table_rows.is_empty() {
            return;
        }
        self.ensure_blank_line();
        let width = self.table_rows.iter().map(Vec::len).max().unwrap_or(0);
        if width == 0 {
            self.table_rows.clear();
            return;
        }
        for (idx, row) in self.table_rows.iter().enumerate() {
            let mut cells = row.clone();
            cells.resize(width, String::new());
            if self.config.output_format == HtmlOutputFormat::Markdown {
                self.out.push_str("| ");
                self.out.push_str(&cells.join(" | "));
                self.out.push_str(" |\n");
                if idx == 0 {
                    let sep: Vec<&str> = vec!["---"; width];
                    self.out.push_str("| ");
                    self.out.push_str(&sep.join(" | "));
                    self.out.push_str(" |\n");
                }
            } else {
                self.out.push_str(&cells.join("\t"));
                self.out.push('\n');
            }
        }
        self.table_rows.clear();
        self.ensure_blank_line();
    }

    fn capture_meta(&mut self, attrs: &str) {
        let content = extract_attr(attrs, "content").map(|s| s.trim().to_string());
        let Some(content) = content.filter(|s| !s.is_empty()) else {
            return;
        };
        let key = extract_attr(attrs, "name")
            .or_else(|| extract_attr(attrs, "property"))
            .unwrap_or_default()
            .to_ascii_lowercase();
        match key.as_str() {
            "author" => self.metadata.author.get_or_insert(content),
            "description" | "og:description" => self.metadata.description.get_or_insert(content),
            "date" | "article:published_time" | "pubdate" => {
                self.metadata.date.get_or_insert(content)
            }
            "og:site_name" => self.metadata.sitename.get_or_insert(content),
            "og:title" => self.metadata.title.get_or_insert(content),
            _ => return,
        };
    }

    fn push_newline(&mut self) {
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
    }

    fn ensure_blank_line(&mut self) {
        if self.out.is_empty() {
            return;
        }
        if !self.out.ends_with('\n') {
            self.out.push_str("\n\n");
        } else if !self.out.ends_with("\n\n") {
            self.out.push('\n');
        }
    }
}

fn is_ignored_container(tag: &str) -> bool {
    matches!(
        tag,
        "script" | "style" | "noscript" | "svg" | "template" | "nav" | "footer" | "aside"
    )
}

fn find_tag_close(html: &str, start: usize) -> Option<usize> {
    let mut quote: Option<u8> = None;
    for (rel, b) in html.as_bytes()[start..].iter().copied().enumerate() {
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'"' || b == b'\'' => quote = Some(b),
            None if b == b'>' => return Some(start + rel),
            None => {}
        }
    }
    None
}

fn extract_attr(attrs: &str, target: &str) -> Option<String> {
    let bytes = attrs.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let name_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' {
            i += 1;
        }
        let name = attrs[name_start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let val = if bytes[i] == b'"' || bytes[i] == b'\'' {
            let quote = bytes[i];
            i += 1;
            let val_start = i;
            while i < bytes.len() && bytes[i] != quote {
                i += 1;
            }
            let slice = &attrs[val_start..i];
            if i < bytes.len() {
                i += 1;
            }
            slice
        } else {
            let val_start = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            &attrs[val_start..i]
        };
        if name == target {
            return Some(decode_entities(val));
        }
    }
    None
}

fn resolve_href(base_url: Option<&str>, href: &str) -> String {
    if href.starts_with("http://")
        || href.starts_with("https://")
        || href.starts_with("mailto:")
        || href.starts_with('#')
    {
        return href.to_string();
    }
    let Some(base) = base_url else {
        return href.to_string();
    };
    if href.starts_with('/') {
        if let Some(scheme_pos) = base.find("://") {
            let host_start = scheme_pos + 3;
            let origin_end = base[host_start..]
                .find('/')
                .map_or(base.len(), |p| host_start + p);
            return format!("{}{href}", &base[..origin_end]);
        }
    }
    let trimmed_base = base.trim_end_matches('/');
    format!("{trimmed_base}/{href}")
}

fn decode_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        if let Some(semi) = after[..after.len().min(12)].find(';') {
            let entity = &after[1..semi];
            if let Some(ch) = decode_one_entity(entity) {
                out.push_str(ch.as_str());
                rest = &after[semi + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &after[1..];
    }
    out.push_str(rest);
    out
}

fn decode_one_entity(entity: &str) -> Option<String> {
    match entity {
        "amp" => Some("&".to_string()),
        "lt" => Some("<".to_string()),
        "gt" => Some(">".to_string()),
        "quot" => Some("\"".to_string()),
        "apos" | "#39" => Some("'".to_string()),
        "nbsp" => Some(" ".to_string()),
        "mdash" => Some("—".to_string()),
        "ndash" => Some("–".to_string()),
        _ if entity.starts_with("#x") || entity.starts_with("#X") => {
            u32::from_str_radix(&entity[2..], 16)
                .ok()
                .and_then(char::from_u32)
                .map(|c| c.to_string())
        }
        _ if entity.starts_with('#') => entity[1..]
            .parse::<u32>()
            .ok()
            .and_then(char::from_u32)
            .map(|c| c.to_string()),
        _ => None,
    }
}

fn collapse_inline_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    out
}

fn normalize_whitespace(raw: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut blank_run = 0usize;
    for line in raw.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blank_run += 1;
            if blank_run <= 1 && !lines.is_empty() {
                lines.push(String::new());
            }
        } else {
            blank_run = 0;
            lines.push(trimmed.to_string());
        }
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_article_and_drops_boilerplate() {
        let html = r#"<!DOCTYPE html>
<html>
<head>
  <title>Rust Port Notes</title>
  <meta name="author" content="Headroom Team">
  <style>.nav { color: red; }</style>
  <script>console.log("tracking");</script>
</head>
<body>
  <nav><a href="/home">Home</a><a href="/pricing">Pricing</a></nav>
  <main>
    <h1>Rust Port Notes</h1>
    <p>Pure Rust extraction removes boilerplate with zero Python.</p>
    <p>See <a href="/docs/guide">the guide</a> for details.</p>
  </main>
  <footer>Copyright 2026</footer>
</body>
</html>"#;
        assert!(is_html_content(html));
        let extractor = HtmlExtractor::new(HtmlExtractorConfig::default());
        let res = extractor.extract(html, Some("https://example.com/blog/post"));
        assert_eq!(res.metadata.title.as_deref(), Some("Rust Port Notes"));
        assert_eq!(res.metadata.author.as_deref(), Some("Headroom Team"));
        assert!(res.extracted.contains("# Rust Port Notes"));
        assert!(res
            .extracted
            .contains("[the guide](https://example.com/docs/guide)"));
        assert!(!res.extracted.contains("console.log"));
        assert!(!res.extracted.contains("Pricing"));
        assert!(!res.extracted.contains("Copyright 2026"));
        assert!(res.extracted_length < res.original_length);
    }

    #[test]
    fn converts_tables_to_markdown() {
        let html =
            "<table><tr><th>ID</th><th>Name</th></tr><tr><td>1</td><td>Alice</td></tr></table>";
        let res = HtmlExtractor::new(HtmlExtractorConfig::default()).extract(html, None);
        assert!(res.extracted.contains("| ID | Name |"));
        assert!(res.extracted.contains("| --- | --- |"));
        assert!(res.extracted.contains("| 1 | Alice |"));
    }
}
