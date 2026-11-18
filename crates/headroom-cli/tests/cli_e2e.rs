//! End-to-end tests for the `headroom` unified Rust CLI.

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::Arc;

use headroom_cli::doctor::{validate_base_url_safety, CheckLevel};
use headroom_cli::inspect::unified_diff;
use headroom_cli::savings::render_bar;
use headroom_proxy::observability::ledger::{provider, Ledger, RequestEvent, Usage};
use serde_json::{json, Value};
use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_headroom")
}

#[test]
fn doctor_url_validator_rejects_spoofing() {
    assert_eq!(
        validate_base_url_safety("http://127.0.0.1:8787"),
        Ok(CheckLevel::Pass)
    );
    assert_eq!(
        validate_base_url_safety("http://localhost:8787/v1"),
        Ok(CheckLevel::Pass)
    );
    assert_eq!(
        validate_base_url_safety("http://[::1]:8787"),
        Ok(CheckLevel::Pass)
    );
    assert_eq!(
        validate_base_url_safety("https://api.anthropic.com"),
        Ok(CheckLevel::Pass)
    );

    assert!(validate_base_url_safety("http://127.0.0.1@evil.com").is_err());
    assert!(validate_base_url_safety("http://localhost.evil.com").is_err());
    assert!(validate_base_url_safety("https://user:pass@127.0.0.1").is_err());
    assert!(validate_base_url_safety("ftp://127.0.0.1").is_err());
}

#[test]
fn compress_subcommand_handles_messages_and_raw_logs() {
    let dir = TempDir::new().unwrap();
    let msg_path = dir.path().join("messages.json");
    let rows: Vec<Value> = (0..80)
        .map(|i| {
            json!({
                "id": i,
                "service": "billing",
                "status": if i == 42 { "error" } else { "ok" },
                "region": "us-west-2",
                "detail": format!("event-{i}")
            })
        })
        .collect();
    let payload = json!([
        {"role": "user", "content": "check billing failures"},
        {"role": "tool", "content": serde_json::to_string(&rows).unwrap()}
    ]);
    std::fs::write(&msg_path, serde_json::to_string(&payload).unwrap()).unwrap();

    let out = Command::new(bin())
        .args(["compress", msg_path.to_str().unwrap(), "--json"])
        .output()
        .expect("run headroom compress");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let parsed: Value = serde_json::from_slice(&out.stdout).expect("valid json stdout");
    let before = parsed["tokens_before"].as_u64().unwrap();
    let after = parsed["tokens_after"].as_u64().unwrap();
    assert!(after < before, "expected {after} < {before}");
    assert_eq!(parsed["tokens_saved"].as_u64().unwrap(), before - after);

    // Raw text log stdin with --stats
    let mut log_input = String::new();
    for i in 0..80 {
        log_input.push_str(&format!(
            "2026-11-18T10:00:{:02}Z INFO worker polling queue batch_id=100\n",
            i % 60
        ));
    }
    let mut child = Command::new(bin())
        .args(["compress", "--stats"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn compress stdin");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(log_input.as_bytes())
        .unwrap();
    let stdin_out = child.wait_with_output().unwrap();
    assert!(stdin_out.status.success());
    let stderr = String::from_utf8_lossy(&stdin_out.stderr);
    assert!(stderr.contains("headroom compress:"), "{stderr}");
}

#[test]
fn doctor_subcommand_reports_exit_codes_for_spoofed_env_and_clean_env() {
    let dir = TempDir::new().unwrap();
    let ws = dir.path().join("ws");

    // Clean environment with an unreachable port exits 1 (warnings, no failures)
    let ok_out = Command::new(bin())
        .args(["doctor", "--port", "1", "--json"])
        .env("HOME", dir.path())
        .env("HEADROOM_WORKSPACE_DIR", &ws)
        .env_remove("ANTHROPIC_BASE_URL")
        .env_remove("OPENAI_BASE_URL")
        .output()
        .unwrap();
    assert_eq!(ok_out.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&ok_out.stdout).unwrap();
    assert_eq!(report["ok"], Value::Bool(true));

    // Spoofed userinfo attack returns exit code 2
    let bad_userinfo = Command::new(bin())
        .args(["doctor", "--port", "1", "--json"])
        .env("HOME", dir.path())
        .env("HEADROOM_WORKSPACE_DIR", &ws)
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1@evil.com")
        .env_remove("OPENAI_BASE_URL")
        .output()
        .unwrap();
    assert_eq!(bad_userinfo.status.code(), Some(2));
    let bad_report: Value = serde_json::from_slice(&bad_userinfo.stdout).unwrap();
    assert_eq!(bad_report["ok"], Value::Bool(false));

    // Spoofed domain suffix returns exit code 2
    let bad_suffix = Command::new(bin())
        .args(["doctor", "--port", "1"])
        .env("HOME", dir.path())
        .env("HEADROOM_WORKSPACE_DIR", &ws)
        .env_remove("ANTHROPIC_BASE_URL")
        .env("OPENAI_BASE_URL", "http://localhost.evil.com/v1")
        .output()
        .unwrap();
    assert_eq!(bad_suffix.status.code(), Some(2));
}

#[tokio::test]
async fn savings_subcommand_reads_ledger_and_renders_unicode_bars() {
    let dir = TempDir::new().unwrap();
    let stats_path = dir.path().join("native_stats.json");
    let ledger = Arc::new(Ledger::load(stats_path.clone()));

    let mut ev1 = RequestEvent::new(provider::OPENAI_CHAT, "gpt-4o", "req-1");
    ev1.tokens_before = 1000;
    ev1.tokens_after = 300;
    ev1.usage = Usage {
        input_tokens: 300,
        output_tokens: 50,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    };
    ledger.record(ev1);

    let mut ev2 = RequestEvent::new(provider::ANTHROPIC, "claude-sonnet-4-5-20250929", "req-2");
    ev2.tokens_before = 2000;
    ev2.tokens_after = 500;
    ev2.usage = Usage {
        input_tokens: 500,
        output_tokens: 100,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    };
    ledger.record(ev2);
    ledger.flush().await;

    assert_eq!(render_bar(50.0, 10), "█████░░░░░");

    let text_out = Command::new(bin())
        .args([
            "savings",
            "--stats-path",
            stats_path.to_str().unwrap(),
            "--by-model",
        ])
        .output()
        .unwrap();
    assert!(text_out.status.success());
    let stdout = String::from_utf8_lossy(&text_out.stdout);
    assert!(stdout.contains('█'), "{stdout}");
    assert!(stdout.contains("gpt-4o"), "{stdout}");
    assert!(stdout.contains("claude-sonnet-4-5"), "{stdout}");

    let json_out = Command::new(bin())
        .args([
            "savings",
            "--stats-path",
            stats_path.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(json_out.status.success());
    let parsed: Value = serde_json::from_slice(&json_out.stdout).unwrap();
    assert_eq!(parsed["lifetime"]["requests"], 2);
    assert_eq!(parsed["lifetime"]["tokens_saved"], 2200);
}

#[test]
fn inspect_subcommand_renders_unified_diff() {
    let diff = unified_diff("a\nb\nc\n", "a\nx\nc\n", 1);
    assert!(diff.contains("--- before"));
    assert!(diff.contains("+++ after"));
    assert!(diff.contains("-b\n"));
    assert!(diff.contains("+x\n"));

    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("build.log");
    let mut log_content = String::new();
    for _ in 0..50 {
        log_content.push_str("[info] compiling repeated target artifact chunk\n");
    }
    log_content.push_str("[error] link failed on target binary\n");
    std::fs::write(&log_path, &log_content).unwrap();

    let out = Command::new(bin())
        .args(["inspect", "--input", log_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--- before"), "{stdout}");
    assert!(stdout.contains("+++ after"), "{stdout}");
    assert!(stdout.contains("@@ "), "{stdout}");
}

#[test]
fn mcp_install_status_and_uninstall_roundtrip() {
    let home = TempDir::new().unwrap();
    let ws = home.path().join(".headroom");

    let install = Command::new(bin())
        .args(["mcp", "install", "--target", "all", "--json"])
        .env("HOME", home.path())
        .env("HEADROOM_WORKSPACE_DIR", &ws)
        .output()
        .unwrap();
    assert!(install.status.success());
    let install_rows: Vec<Value> = serde_json::from_slice(&install.stdout).unwrap();
    assert_eq!(install_rows.len(), 4);
    assert!(install_rows
        .iter()
        .all(|r| r["status"].as_str() == Some("registered")));

    let status_on = Command::new(bin())
        .args(["mcp", "status", "--target", "all", "--json"])
        .env("HOME", home.path())
        .env("HEADROOM_WORKSPACE_DIR", &ws)
        .output()
        .unwrap();
    assert!(status_on.status.success());
    let status_rows: Vec<Value> = serde_json::from_slice(&status_on.stdout).unwrap();
    assert!(status_rows
        .iter()
        .all(|r| r["registered"].as_bool() == Some(true)));

    let uninstall = Command::new(bin())
        .args(["mcp", "uninstall", "--target", "all", "--json"])
        .env("HOME", home.path())
        .env("HEADROOM_WORKSPACE_DIR", &ws)
        .output()
        .unwrap();
    assert!(uninstall.status.success());
    let uninstall_rows: Vec<Value> = serde_json::from_slice(&uninstall.stdout).unwrap();
    assert!(uninstall_rows
        .iter()
        .all(|r| r["status"].as_str() == Some("removed")));

    let status_off = Command::new(bin())
        .args(["mcp", "status", "--target", "all", "--json"])
        .env("HOME", home.path())
        .env("HEADROOM_WORKSPACE_DIR", &ws)
        .output()
        .unwrap();
    assert!(status_off.status.success());
    let status_off_rows: Vec<Value> = serde_json::from_slice(&status_off.stdout).unwrap();
    assert!(status_off_rows
        .iter()
        .all(|r| r["registered"].as_bool() == Some(false)));
}

#[test]
fn wrap_and_unwrap_inject_env_and_cleanup_sessions() {
    let dir = TempDir::new().unwrap();
    let ws = dir.path().join("ws");

    let out = Command::new(bin())
        .args([
            "wrap",
            "--port",
            "0",
            "--upstream",
            "http://127.0.0.1:9",
            "/usr/bin/env",
        ])
        .env("HOME", dir.path())
        .env("HEADROOM_WORKSPACE_DIR", &ws)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:"),
        "{stdout}"
    );
    assert!(
        stdout.contains("OPENAI_BASE_URL=http://127.0.0.1:"),
        "{stdout}"
    );
    assert!(
        stdout.contains("HEADROOM_PROXY_URL=http://127.0.0.1:"),
        "{stdout}"
    );

    let wrap_dir = ws.join("wrap");
    let remaining: Vec<_> = std::fs::read_dir(&wrap_dir)
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert!(
        remaining.is_empty(),
        "expected RAII wrap guard to remove session file, found {remaining:?}"
    );

    let stale = wrap_dir.join("999999.json");
    std::fs::write(&stale, "{}").unwrap();
    let unwrap_out = Command::new(bin())
        .args(["unwrap", "--all"])
        .env("HOME", dir.path())
        .env("HEADROOM_WORKSPACE_DIR", &ws)
        .output()
        .unwrap();
    assert!(unwrap_out.status.success());
    assert!(!stale.exists());
}
