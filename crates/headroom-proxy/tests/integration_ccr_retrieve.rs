//! Integration tests for optional in-proxy CCR storage (`/v1/retrieve`)
//! and live transformation feed (`/transformations/feed`).

mod common;

use headroom_proxy::config::CompressionMode;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn extract_ccr_hash(text: &str) -> Option<String> {
    let start = text.find("<<ccr:")?;
    let rest = &text[start + "<<ccr:".len()..];
    let end = rest.find(">>")?;
    let candidate = &rest[..end];
    if candidate.len() == 24 && candidate.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(candidate.to_string())
    } else {
        None
    }
}

#[tokio::test]
async fn ccr_retrieve_and_transformations_feed_roundtrip() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_ccr_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5-20250929",
            "content": [{"type": "text", "text": "done"}],
            "usage": {"input_tokens": 50, "output_tokens": 5}
        })))
        .mount(&upstream)
        .await;

    let proxy = common::start_proxy_with(&upstream.uri(), |cfg| {
        cfg.compression = true;
        cfg.compression_mode = CompressionMode::LiveZone;
        cfg.ccr = true;
        cfg.log_messages = true;
    })
    .await;

    // Construct a nested heterogeneous 120-element tool_result array so
    // SmartCrusher falls through to lossy CCR-recoverable sampling.
    let items: Vec<Value> = (0..120)
        .map(|i| {
            let extra = "x".repeat(40);
            if i % 3 == 0 {
                json!({
                    "id": i,
                    "detail": { "tag": format!("entry-{i}"), "note": extra },
                    "severity": if i == 50 { "error" } else { "info" },
                    "optional_flag": true
                })
            } else {
                json!({
                    "id": i,
                    "detail": { "tag": format!("entry-{i}"), "extra": { "idx": i } },
                    "severity": "info",
                    "payload": extra
                })
            }
        })
        .collect();
    let original_tool_payload = serde_json::to_string(&items).expect("serialize items");

    let req_body = json!({
        "model": "claude-sonnet-4-5-20250929",
        "max_tokens": 256,
        "messages": [
            { "role": "user", "content": "Inspect latest deployment events" },
            {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_01",
                    "name": "list_events",
                    "input": {}
                }]
            },
            {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_01",
                    "content": original_tool_payload
                }]
            }
        ]
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("x-api-key", "sk-ant-test-payg-key")
        .json(&req_body)
        .send()
        .await
        .expect("POST /v1/messages");
    assert_eq!(resp.status(), 200);

    let recorded = upstream.received_requests().await.expect("requests");
    assert_eq!(recorded.len(), 1);
    let forwarded_body = String::from_utf8_lossy(&recorded[0].body);
    let hash = extract_ccr_hash(&forwarded_body).unwrap_or_else(|| {
        panic!("expected <<ccr:HASH>> marker in forwarded body: {forwarded_body}")
    });

    // GET /v1/retrieve/{hash} recovers original tool_result content
    let get_res = client
        .get(format!("{}/v1/retrieve/{hash}", proxy.url()))
        .send()
        .await
        .expect("GET /v1/retrieve/{hash}");
    assert_eq!(get_res.status(), 200);
    let get_json: Value = get_res.json().await.expect("json");
    assert_eq!(get_json["hash"], hash);
    let retrieved_str = get_json["content"].as_str().expect("content str");
    assert!(retrieved_str.contains("entry-42"));
    assert!(retrieved_str.contains("entry-99"));

    // POST /v1/retrieve with query filter
    let post_res = client
        .post(format!("{}/v1/retrieve", proxy.url()))
        .json(&json!({
            "hash": hash,
            "query": "entry-42",
            "max_results": 5
        }))
        .send()
        .await
        .expect("POST /v1/retrieve");
    assert_eq!(post_res.status(), 200);
    let post_json: Value = post_res.json().await.expect("json");
    let filtered = post_json["content"].as_str().expect("filtered str");
    assert!(filtered.contains("entry-42"));
    assert!(!filtered.contains("entry-10"));

    // Missing hash returns 404
    let missing_res = client
        .get(format!(
            "{}/v1/retrieve/000000000000000000000000",
            proxy.url()
        ))
        .send()
        .await
        .expect("missing retrieve");
    assert_eq!(missing_res.status(), 404);

    // DNS-rebinding Host header blocked with 404
    let evil_retrieve = client
        .get(format!("{}/v1/retrieve/{hash}", proxy.url()))
        .header("Host", "evil.example.com")
        .send()
        .await
        .expect("evil host retrieve");
    assert_eq!(evil_retrieve.status(), 404);

    // GET /transformations/feed records the compression event
    let feed_res = client
        .get(format!("{}/transformations/feed", proxy.url()))
        .send()
        .await
        .expect("GET /transformations/feed");
    assert_eq!(feed_res.status(), 200);
    let feed_json: Value = feed_res.json().await.expect("feed json");
    let items = feed_json["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["endpoint"], "/v1/messages");
    let before = items[0]["tokens_before"].as_u64().unwrap();
    let after = items[0]["tokens_after"].as_u64().unwrap();
    assert!(after < before, "expected {after} < {before}");

    // DNS-rebinding blocked on /transformations/feed
    let evil_feed = client
        .get(format!("{}/transformations/feed", proxy.url()))
        .header("Host", "evil.example.com")
        .send()
        .await
        .expect("evil host feed");
    assert_eq!(evil_feed.status(), 404);

    proxy.shutdown().await;
}
