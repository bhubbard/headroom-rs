//! Integration tests for `headroom-mcp` server protocol, CCR fallback, FTS5 memory,
//! and isolated agent configuration registrars.

use std::collections::BTreeMap;
use std::sync::Arc;

use headroom_core::ccr::InMemoryCcrStore;
use headroom_mcp::{
    check_status, install_mcp, uninstall_mcp, AgentTarget, JsonRpcRequest, McpServer,
    McpServerConfig, MemoryStore, OverwritePolicy, RegisterStatus, ServerSpec,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn call_req(id: i64, name: &str, arguments: Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(id)),
        method: "tools/call".to_string(),
        params: Some(json!({ "name": name, "arguments": arguments })),
    }
}

#[tokio::test]
async fn mcp_compress_retrieve_and_memory_lifecycle() {
    let ccr = Arc::new(InMemoryCcrStore::new());
    let mem = MemoryStore::in_memory().unwrap();
    let cfg = McpServerConfig {
        proxy_url: "http://127.0.0.1:1".to_string(),
        ccr_db_path: None,
        memory_db_path: None,
        stateless: true,
    };
    let mut server = McpServer::with_stores(cfg, ccr.clone(), mem).unwrap();

    // 1. initialize
    let init = server
        .handle_request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            method: "initialize".to_string(),
            params: Some(json!({})),
        })
        .await
        .expect("initialize response");
    assert_eq!(init.result.unwrap()["serverInfo"]["name"], "headroom");

    // Notifications must produce no response
    assert!(server
        .handle_request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: "notifications/initialized".to_string(),
            params: None,
        })
        .await
        .is_none());

    // 2. tools/list
    let tools = server
        .handle_request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(2)),
            method: "tools/list".to_string(),
            params: None,
        })
        .await
        .unwrap();
    let tools_result = tools.result.unwrap();
    let names: Vec<&str> = tools_result["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"headroom_compress"));
    assert!(names.contains(&"headroom_retrieve"));
    assert!(names.contains(&"headroom_stats"));
    assert!(names.contains(&"memory_add"));

    // 3. Compress large JSON array and recover full payload via headroom_retrieve
    let rows: Vec<Value> = (0..100)
        .map(|i| json!({"id": i, "service": "orders", "status": "ok", "region": "us-east-1"}))
        .collect();
    let original_json = serde_json::to_string(&rows).unwrap();
    let comp_resp = server
        .handle_request(call_req(
            3,
            "headroom_compress",
            json!({ "content": original_json }),
        ))
        .await
        .unwrap();
    let comp_text = comp_resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let hash_pos = comp_text.find("ccr_hash=").expect("ccr_hash in summary");
    let hash_slice = &comp_text[hash_pos + "ccr_hash=".len()..];
    let hash = hash_slice.trim_end_matches([')', ']']);
    assert_eq!(hash.len(), 24);

    let ret_resp = server
        .handle_request(call_req(4, "ccr_retrieve", json!({ "hash": hash })))
        .await
        .unwrap();
    assert_eq!(
        ret_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap(),
        original_json
    );

    // 4. Memory add -> search -> delete
    let mem_add = server
        .handle_request(call_req(
            5,
            "memory_add",
            json!({
                "content": "Always validate FTS5 tokens in headroom-mcp",
                "tags": ["rust", "sqlite"]
            }),
        ))
        .await
        .unwrap();
    let added: Value = serde_json::from_str(
        mem_add.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let mem_id = added["id"].as_str().unwrap().to_string();

    let search = server
        .handle_request(call_req(6, "memory_search", json!({ "query": "FTS5" })))
        .await
        .unwrap();
    let search_json: Value = serde_json::from_str(
        search.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(search_json["count"], 1);

    let del = server
        .handle_request(call_req(7, "memory_delete", json!({ "id": mem_id })))
        .await
        .unwrap();
    let del_json: Value =
        serde_json::from_str(del.result.unwrap()["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(del_json["deleted"], true);
}

#[tokio::test]
async fn mcp_retrieve_falls_back_to_proxy_http_endpoint() {
    let proxy = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/retrieve/0123456789abcdef01234567"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "hash": "0123456789abcdef01234567",
            "content": "full upstream payload line 1\nerror line 2",
            "total_bytes": 40
        })))
        .mount(&proxy)
        .await;

    let ccr = Arc::new(InMemoryCcrStore::new());
    let mem = MemoryStore::in_memory().unwrap();
    let cfg = McpServerConfig {
        proxy_url: proxy.uri(),
        ccr_db_path: None,
        memory_db_path: None,
        stateless: true,
    };
    let mut server = McpServer::with_stores(cfg, ccr, mem).unwrap();
    let resp = server
        .handle_request(call_req(
            1,
            "headroom_retrieve",
            json!({
                "hash": "0123456789abcdef01234567",
                "query": "error"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.result.unwrap()["content"][0]["text"].as_str().unwrap(),
        "error line 2"
    );
}

#[test]
fn agent_registrars_are_idempotent_non_destructive_and_uninstallable() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();

    // Seed pre-existing servers in JSON and TOML configs
    std::fs::create_dir_all(home.join(".gemini/antigravity")).unwrap();
    std::fs::write(
        home.join(".gemini/antigravity/mcp_config.json"),
        r#"{"mcpServers": {"other": {"command": "echo"}}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::write(
        home.join(".codex/config.toml"),
        "model = \"o3\"\n[mcp_servers.other]\ncommand = \"echo\"\n",
    )
    .unwrap();

    let spec = ServerSpec::headroom("/usr/local/bin/headroom", "http://127.0.0.1:8787");
    let first = install_mcp(
        home,
        &AgentTarget::ALL,
        &spec,
        OverwritePolicy::PreserveExisting,
    );
    assert!(first.iter().all(|r| r.status == RegisterStatus::Registered));

    // Idempotent second run
    let second = install_mcp(
        home,
        &AgentTarget::ALL,
        &spec,
        OverwritePolicy::PreserveExisting,
    );
    assert!(second.iter().all(|r| r.status == RegisterStatus::Already));

    // Preserve foreign servers
    let ag_json: Value = serde_json::from_str(
        &std::fs::read_to_string(home.join(".gemini/antigravity/mcp_config.json")).unwrap(),
    )
    .unwrap();
    assert!(ag_json["mcpServers"]["other"].is_object());
    assert!(ag_json["mcpServers"]["headroom"].is_object());

    // Mismatch without --force refuses overwrite; --force updates
    let modified = ServerSpec {
        name: "headroom".to_string(),
        command: "/opt/bin/headroom".to_string(),
        args: vec!["mcp".to_string(), "serve".to_string()],
        env: BTreeMap::new(),
    };
    let mismatch = install_mcp(
        home,
        &AgentTarget::ALL,
        &modified,
        OverwritePolicy::PreserveExisting,
    );
    assert!(mismatch
        .iter()
        .all(|r| r.status == RegisterStatus::Mismatch));

    let forced = install_mcp(home, &AgentTarget::ALL, &modified, OverwritePolicy::Force);
    assert!(forced.iter().all(|r| r.status == RegisterStatus::Updated));

    // Status reflects registered=true
    let statuses = check_status(home, &AgentTarget::ALL, "headroom");
    assert!(statuses.iter().all(|s| s.registered));

    // Uninstall removes headroom but preserves foreign servers
    let removed = uninstall_mcp(home, &AgentTarget::ALL, "headroom");
    assert!(removed.iter().all(|r| r.status == RegisterStatus::Removed));

    let codex_after = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
    assert!(codex_after.contains("[mcp_servers.other]"));
    assert!(!codex_after.contains("[mcp_servers.headroom]"));

    // Corrupt JSON must never be overwritten
    std::fs::write(home.join(".cursor/mcp.json"), "{ invalid json").unwrap();
    let corrupt_res = install_mcp(home, &[AgentTarget::Cursor], &spec, OverwritePolicy::Force);
    assert_eq!(corrupt_res[0].status, RegisterStatus::Failed);
    assert_eq!(
        std::fs::read_to_string(home.join(".cursor/mcp.json")).unwrap(),
        "{ invalid json"
    );
}
