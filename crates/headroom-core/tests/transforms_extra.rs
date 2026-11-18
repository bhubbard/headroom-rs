//! Integration tests for ported transforms (`lossless_compaction`, `html_extractor`,
//! `tabular_compressor`, `config_compressor`, `recursive_json`) and live-zone Tabular/Html arms.

use headroom_core::ccr::{CcrStore, InMemoryCcrStore};
use headroom_core::transforms::{
    carries_record_array, compact_lossless, compress_anthropic_live_zone, detect_config_flavor,
    expand_runs, json_document_spans, path_unheading, route_embedded_json, search_dir_unheading,
    search_unheading, unfold_repeated_blocks, AuthMode, BlockAction, ConfigCompressor,
    ConfigFlavor, HtmlExtractor, HtmlExtractorConfig, LiveZoneOutcome, LosslessKind,
    TabularCompressor, TabularFormat,
};
use serde_json::json;

#[test]
fn lossless_compaction_all_kinds_roundtrip() {
    let log = "\x1b[32mINFO ready\x1b[0m\n".repeat(12);
    let compacted_log = compact_lossless(&log, LosslessKind::Log);
    assert!(compacted_log.len() < log.len());
    assert!(!compacted_log.contains("\x1b[32m"));
    assert_eq!(
        expand_runs(&compacted_log, Some(log.len())).as_deref(),
        Some("INFO ready\n".repeat(12).as_str())
    );

    let grep_file = "crates/headroom-core/src/lib.rs:10:pub mod a;\n\
                     crates/headroom-core/src/lib.rs:11:pub mod b;\n\
                     crates/headroom-core/src/lib.rs:12:pub mod c;\n\
                     crates/headroom-core/src/lib.rs:13:pub mod d;\n";
    let headed = compact_lossless(grep_file, LosslessKind::Search);
    assert!(headed.len() < grep_file.len());
    assert_eq!(search_unheading(&headed), grep_file);

    let grep_dir = "crates/headroom-core/src/transforms/a.rs:1:fn a() {}\n\
                    crates/headroom-core/src/transforms/b.rs:1:fn b() {}\n\
                    crates/headroom-core/src/transforms/c.rs:1:fn c() {}\n\
                    crates/headroom-core/src/transforms/d.rs:1:fn d() {}\n";
    let dir_headed = compact_lossless(grep_dir, LosslessKind::Search);
    assert!(dir_headed.len() < grep_dir.len());
    assert_eq!(search_dir_unheading(&dir_headed), grep_dir);

    let paths = "crates/headroom-core/src/transforms/a.rs\n\
                 crates/headroom-core/src/transforms/b.rs\n\
                 crates/headroom-core/src/transforms/c.rs\n";
    let path_folded = compact_lossless(paths, LosslessKind::Paths);
    assert!(path_folded.len() < paths.len());
    assert_eq!(path_unheading(&path_folded), paths);

    let yaml = "\
services:
  - name: alpha
    image: ghcr.io/org/svc:1.0
    replicas: 3
    port: 8080
    protocol: TCP
  - name: beta
    image: ghcr.io/org/svc:1.0
    replicas: 3
    port: 8080
    protocol: TCP
";
    let folded_cfg = compact_lossless(yaml, LosslessKind::Config);
    assert!(folded_cfg.len() < yaml.len());
    assert_eq!(
        unfold_repeated_blocks(&folded_cfg, Some(yaml.len())).as_deref(),
        Some(yaml)
    );
}

#[test]
fn html_extractor_and_live_zone_html_arm() {
    let html = format!(
        "<!DOCTYPE html><html><head><title>API Docs</title><style>body{{}}</style></head>\
         <body><nav>Home | Docs | Login</nav><main><h1>Endpoints</h1>{}<p>End.</p></main>\
         <footer>Copyright 2026</footer></body></html>",
        "<p>Request rate limits apply per organization token and reset every sixty seconds.</p>\n"
            .repeat(12)
    );
    assert!(html.len() > 512);
    let extracted = HtmlExtractor::new(HtmlExtractorConfig::default()).extract(&html, None);
    assert!(extracted.extracted_length < extracted.original_length);
    assert!(extracted.extracted.contains("# Endpoints"));
    assert!(!extracted.extracted.contains("Copyright 2026"));

    let body = serde_json::to_vec(&json!({
        "model": "claude-3-5-sonnet-20241022",
        "messages": [{
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "t1", "content": html}]
        }]
    }))
    .unwrap();
    let out = compress_anthropic_live_zone(&body, 0, AuthMode::Payg, "claude-3-5-sonnet-20241022")
        .unwrap();
    match out {
        LiveZoneOutcome::Modified { manifest, .. } => {
            assert!(manifest.block_outcomes.iter().any(|b| matches!(
                b.action,
                BlockAction::Compressed {
                    strategy: "html_extractor",
                    ..
                }
            )));
        }
        other => panic!("expected HtmlExtractor Modified outcome, got {other:?}"),
    }
}

#[test]
fn tabular_compressor_and_live_zone_tabular_arm() {
    let mut csv_payload = String::from("id,region,service,status\n");
    for i in 0..80 {
        csv_payload.push_str(&format!("{i},us-west-2,edge-proxy,healthy\n"));
    }
    assert!(csv_payload.len() > 512);
    let res = TabularCompressor::default().compress(&csv_payload, "");
    assert_eq!(res.format, Some(TabularFormat::Csv));
    assert!(res.was_modified);

    let body = serde_json::to_vec(&json!({
        "model": "claude-3-5-sonnet-20241022",
        "messages": [{
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "t1", "content": csv_payload}]
        }]
    }))
    .unwrap();
    let out = compress_anthropic_live_zone(&body, 0, AuthMode::Payg, "claude-3-5-sonnet-20241022")
        .unwrap();
    match out {
        LiveZoneOutcome::Modified { manifest, .. } => {
            assert!(manifest.block_outcomes.iter().any(|b| matches!(
                b.action,
                BlockAction::Compressed {
                    strategy: "tabular",
                    ..
                }
            )));
        }
        other => panic!("expected Tabular Modified outcome, got {other:?}"),
    }
}

#[test]
fn config_compressor_toml_array_and_yaml_ccr() {
    let mut toml_cfg = String::new();
    for i in 0..40 {
        toml_cfg.push_str(&format!(
            "[[workers]]\nname = \"worker-{i}\"\nregion = \"us-east-1\"\nreplicas = 2\nenabled = true\n\n"
        ));
    }
    assert_eq!(detect_config_flavor(&toml_cfg), ConfigFlavor::Toml);
    let store = InMemoryCcrStore::new();
    let res = ConfigCompressor::default().compress(&toml_cfg, "", Some(&store));
    assert!(res.was_modified);
    let hash = res.ccr_hash.expect("hash present");
    assert_eq!(store.get(&hash).as_deref(), Some(toml_cfg.as_str()));
}

#[test]
fn recursive_json_handles_wrapped_payloads() {
    let wrapped = "Status: 200 OK\nBody: [{\"id\":1,\"name\":\"alpha\"},{\"id\":2,\"name\":\"beta\"},{\"id\":3,\"name\":\"gamma\"}]\nTrace: ok\n";
    let spans = json_document_spans(wrapped);
    assert_eq!(spans.len(), 1);
    assert!(carries_record_array(&wrapped[spans[0].0..spans[0].1]));

    let rewritten = route_embedded_json(
        wrapped,
        |_| Some((r#"[{"id":1}]"#.to_string(), true)),
        |s| s.len().div_ceil(4),
    );
    let (out, rev) = rewritten.expect("spliced");
    assert!(rev);
    assert!(out.starts_with("Status: 200 OK\nBody: [{\"id\":1}]"));
    assert!(out.ends_with("\nTrace: ok\n"));
}
