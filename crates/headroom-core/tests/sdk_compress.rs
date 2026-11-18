//! Integration tests for the high-level `headroom_core::sdk` API (`compress`, `compress_text`).

use headroom_core::ccr::{CcrStore, InMemoryCcrStore};
use headroom_core::{
    compress, compress_text, compress_with_ccr, CompressConfig, SdkMode, SystemMessagePolicy,
    UserMessagePolicy,
};
use serde_json::{json, Value};

#[test]
fn compress_text_routes_all_major_formats() {
    let store = InMemoryCcrStore::new();
    let cfg = CompressConfig::default();

    // 1. JSON array of objects
    let rows: Vec<Value> = (0..120)
        .map(|i| json!({"id": i, "service": "payments", "status": "ok", "region": "us-west-2"}))
        .collect();
    let json_raw = serde_json::to_string(&rows).unwrap();
    let json_res = compress_text(&json_raw, &cfg, Some(&store)).unwrap();
    assert!(json_res.was_modified);
    assert!(json_res.tokens_saved > 0);
    let hash = json_res.ccr_hash.unwrap();
    assert_eq!(store.get(&hash).as_deref(), Some(json_raw.as_str()));

    // 2. HTML page
    let html = format!(
        "<!DOCTYPE html><html><head><title>Doc</title></head><body>\
         <nav>Sidebar</nav><main><h1>Guide</h1>{}</main><footer>Foot</footer></body></html>",
        "<p>Detailed architectural explanation with repeated context across sections.</p>\n"
            .repeat(12)
    );
    let html_res = compress_text(&html, &cfg, Some(&store)).unwrap();
    assert!(html_res.was_modified);
    assert_eq!(html_res.strategy, "html_extractor");
    assert!(html_res.compressed.contains("# Guide"));

    // 3. CSV tabular
    let mut csv_raw = String::from("id,env,zone,healthy\n");
    for i in 0..80 {
        csv_raw.push_str(&format!("{i},prod,us-central1,true\n"));
    }
    let csv_res = compress_text(&csv_raw, &cfg, Some(&store)).unwrap();
    assert!(csv_res.was_modified);
    assert!(csv_res.tokens_saved > 0);
}

#[test]
fn agent_lossless_mode_never_emits_ccr_markers() {
    let store = InMemoryCcrStore::new();
    let mut builder = CompressConfig::builder();
    builder
        .mode(SdkMode::AgentLossless)
        .min_tokens_to_compress(10);
    let cfg = builder.build().unwrap();

    let mut csv_raw = String::from("id,env,zone,healthy\n");
    for i in 0..80 {
        csv_raw.push_str(&format!("{i},prod,us-central1,true\n"));
    }
    let res = compress_text(&csv_raw, &cfg, Some(&store)).unwrap();
    assert!(!res.compressed.contains("<<ccr:"));
    assert!(res.ccr_hash.is_none());
}

#[test]
fn compress_handles_anthropic_and_openai_messages() {
    let store = InMemoryCcrStore::new();
    let payload: Vec<Value> = (0..100)
        .map(|i| json!({"id": i, "kind": "event", "ok": true, "host": "prod-1"}))
        .collect();
    let payload_str = serde_json::to_string(&payload).unwrap();

    let anthropic_messages = vec![
        json!({"role": "system", "content": payload_str.clone()}),
        json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "Inspect the latest tool result."},
                {"type": "tool_result", "tool_use_id": "call_1", "content": payload_str.clone()}
            ]
        }),
    ];

    let cfg = CompressConfig {
        user_messages: UserMessagePolicy::Skip,
        system_messages: SystemMessagePolicy::Preserve,
        ..CompressConfig::default()
    };
    let out = compress_with_ccr(&anthropic_messages, &cfg, Some(&store)).unwrap();
    assert!(out.tokens_saved > 0);
    assert_eq!(out.messages[0]["content"], payload_str);
    assert_eq!(
        out.messages[1]["content"][0]["text"],
        "Inspect the latest tool result."
    );
    assert_ne!(out.messages[1]["content"][1]["content"], payload_str);

    // Pure `compress` wrapper without CCR store also works
    let openai_messages = vec![
        json!({"role": "user", "content": "check logs"}),
        json!({"role": "tool", "tool_call_id": "c1", "content": payload_str}),
    ];
    let openai_out = compress(&openai_messages, &CompressConfig::default()).unwrap();
    assert!(openai_out.tokens_saved > 0);
}
