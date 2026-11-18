//! Non-destructive MCP config registrars for Antigravity, Claude Code, Codex, and Cursor.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

const CODEX_MARKER_START: &str = "# --- Headroom MCP server ---";
const CODEX_MARKER_END: &str = "# --- end Headroom MCP server ---";

/// Supported coding-agent MCP configuration targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentTarget {
    Antigravity,
    Claude,
    Codex,
    Cursor,
}

impl AgentTarget {
    /// All supported agent targets in canonical display order.
    pub const ALL: [AgentTarget; 4] = [
        AgentTarget::Antigravity,
        AgentTarget::Claude,
        AgentTarget::Codex,
        AgentTarget::Cursor,
    ];

    /// Lowercase CLI/flag name (`"antigravity"`, `"claude"`, `"codex"`, `"cursor"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AgentTarget::Antigravity => "antigravity",
            AgentTarget::Claude => "claude",
            AgentTarget::Codex => "codex",
            AgentTarget::Cursor => "cursor",
        }
    }

    /// Human-readable product name.
    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            AgentTarget::Antigravity => "Antigravity IDE",
            AgentTarget::Claude => "Claude Code",
            AgentTarget::Codex => "OpenAI Codex CLI",
            AgentTarget::Cursor => "Cursor",
        }
    }

    /// Resolve the target config path relative to `home_dir`.
    #[must_use]
    pub fn config_path(self, home_dir: &Path) -> PathBuf {
        match self {
            AgentTarget::Antigravity => home_dir.join(".gemini/antigravity/mcp_config.json"),
            AgentTarget::Claude => home_dir.join(".claude.json"),
            AgentTarget::Codex => home_dir.join(".codex/config.toml"),
            AgentTarget::Cursor => home_dir.join(".cursor/mcp.json"),
        }
    }

    /// Whether the target tool appears to be installed under `home_dir`.
    #[must_use]
    pub fn is_detected(self, home_dir: &Path) -> bool {
        let cfg = self.config_path(home_dir);
        if cfg.exists() {
            return true;
        }
        match self {
            AgentTarget::Antigravity => home_dir.join(".gemini/antigravity").is_dir(),
            AgentTarget::Claude => {
                home_dir.join(".claude").is_dir() || home_dir.join(".claude.json").exists()
            }
            AgentTarget::Codex => home_dir.join(".codex").is_dir(),
            AgentTarget::Cursor => home_dir.join(".cursor").is_dir(),
        }
    }
}

/// Error returned when parsing an unknown [`AgentTarget`] name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown agent target {0:?} (expected antigravity, claude, codex, or cursor)")]
pub struct ParseAgentTargetError(String);

impl std::str::FromStr for AgentTarget {
    type Err = ParseAgentTargetError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let norm = s.trim().to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|a| a.as_str() == norm)
            .ok_or_else(|| ParseAgentTargetError(s.to_string()))
    }
}

/// Controls whether a mismatched existing entry is overwritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverwritePolicy {
    /// Leave differing existing entries untouched and report `Mismatch`.
    #[default]
    PreserveExisting,
    /// Overwrite differing existing entries.
    Force,
}

/// MCP server command specification to write into agent configurations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSpec {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl ServerSpec {
    /// Standard Headroom MCP server specification.
    #[must_use]
    pub fn headroom(command: impl Into<String>, proxy_url: impl Into<String>) -> Self {
        let mut env = BTreeMap::new();
        env.insert("HEADROOM_PROXY_URL".to_string(), proxy_url.into());
        Self {
            name: "headroom".to_string(),
            command: command.into(),
            args: vec!["mcp".to_string(), "serve".to_string()],
            env,
        }
    }
}

/// Outcome of a single target registration or removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterStatus {
    Registered,
    Updated,
    Already,
    Mismatch,
    Removed,
    NotPresent,
    Failed,
}

/// Per-agent operation result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterResult {
    pub target: AgentTarget,
    pub status: RegisterStatus,
    pub config_path: PathBuf,
    pub detail: String,
}

/// Current installation status of an MCP server on one agent target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStatus {
    pub target: AgentTarget,
    pub detected: bool,
    pub registered: bool,
    pub config_path: PathBuf,
    pub existing: Option<ServerSpec>,
}

/// Install `spec` into every requested target under `home_dir`.
#[must_use]
pub fn install_mcp(
    home_dir: &Path,
    targets: &[AgentTarget],
    spec: &ServerSpec,
    policy: OverwritePolicy,
) -> Vec<RegisterResult> {
    targets
        .iter()
        .map(|&target| match target {
            AgentTarget::Codex => register_codex(home_dir, spec, policy),
            _ => register_json_target(home_dir, target, spec, policy),
        })
        .collect()
}

/// Remove `server_name` from every requested target under `home_dir`.
#[must_use]
pub fn uninstall_mcp(
    home_dir: &Path,
    targets: &[AgentTarget],
    server_name: &str,
) -> Vec<RegisterResult> {
    targets
        .iter()
        .map(|&target| match target {
            AgentTarget::Codex => unregister_codex(home_dir, server_name),
            _ => unregister_json_target(home_dir, target, server_name),
        })
        .collect()
}

/// Inspect MCP server registration status across `targets`.
#[must_use]
pub fn check_status(
    home_dir: &Path,
    targets: &[AgentTarget],
    server_name: &str,
) -> Vec<AgentStatus> {
    targets
        .iter()
        .map(|&target| {
            let config_path = target.config_path(home_dir);
            let detected = target.is_detected(home_dir);
            let existing = match target {
                AgentTarget::Codex => read_codex_entry(&config_path, server_name),
                _ => read_json_entry(&config_path, server_name),
            };
            AgentStatus {
                target,
                detected,
                registered: existing.is_some(),
                config_path,
                existing,
            }
        })
        .collect()
}

fn spec_to_json(spec: &ServerSpec) -> Value {
    let mut obj = Map::new();
    obj.insert("command".to_string(), Value::String(spec.command.clone()));
    obj.insert(
        "args".to_string(),
        Value::Array(spec.args.iter().cloned().map(Value::String).collect()),
    );
    if !spec.env.is_empty() {
        let mut env_obj = Map::new();
        for (k, v) in &spec.env {
            env_obj.insert(k.clone(), Value::String(v.clone()));
        }
        obj.insert("env".to_string(), Value::Object(env_obj));
    }
    Value::Object(obj)
}

fn json_to_spec(name: &str, entry: &Map<String, Value>) -> Option<ServerSpec> {
    let command = entry.get("command")?.as_str()?.to_string();
    let args = entry
        .get("args")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let mut env = BTreeMap::new();
    if let Some(env_obj) = entry.get("env").and_then(Value::as_object) {
        for (k, v) in env_obj {
            if let Some(s) = v.as_str() {
                env.insert(k.clone(), s.to_string());
            }
        }
    }
    Some(ServerSpec {
        name: name.to_string(),
        command,
        args,
        env,
    })
}

fn read_json_entry(path: &Path, server_name: &str) -> Option<ServerSpec> {
    let raw = std::fs::read_to_string(path).ok()?;
    let val: Value = serde_json::from_str(&raw).ok()?;
    let entry = val.get("mcpServers")?.get(server_name)?.as_object()?;
    json_to_spec(server_name, entry)
}

fn register_json_target(
    home_dir: &Path,
    target: AgentTarget,
    spec: &ServerSpec,
    policy: OverwritePolicy,
) -> RegisterResult {
    let path = target.config_path(home_dir);
    let mut root: Map<String, Value> = if path.exists() {
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return RegisterResult {
                target,
                status: RegisterStatus::Failed,
                config_path: path.clone(),
                detail: format!("could not read {}", path.display()),
            };
        };
        let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&raw) else {
            return RegisterResult {
                target,
                status: RegisterStatus::Failed,
                config_path: path.clone(),
                detail: format!(
                    "{} is not valid JSON object; refusing to overwrite",
                    path.display()
                ),
            };
        };
        obj
    } else {
        Map::new()
    };

    let existing = root
        .get("mcpServers")
        .and_then(Value::as_object)
        .and_then(|s| s.get(&spec.name))
        .and_then(Value::as_object)
        .and_then(|e| json_to_spec(&spec.name, e));

    if let Some(ref cur) = existing {
        if cur == spec {
            return RegisterResult {
                target,
                status: RegisterStatus::Already,
                config_path: path,
                detail: "already matches current configuration".to_string(),
            };
        }
        if policy == OverwritePolicy::PreserveExisting {
            return RegisterResult {
                target,
                status: RegisterStatus::Mismatch,
                config_path: path,
                detail: format!(
                    "existing `{}` configuration differs (re-run with --force to update)",
                    spec.name
                ),
            };
        }
    }

    if !root.get("mcpServers").is_some_and(Value::is_object) {
        root.insert("mcpServers".to_string(), Value::Object(Map::new()));
    }
    if let Some(Value::Object(servers)) = root.get_mut("mcpServers") {
        servers.insert(spec.name.clone(), spec_to_json(spec));
    }

    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return RegisterResult {
                target,
                status: RegisterStatus::Failed,
                config_path: path,
                detail: format!("failed creating parent dir: {e}"),
            };
        }
    }
    let serialized = match serde_json::to_string_pretty(&Value::Object(root)) {
        Ok(s) => format!("{s}\n"),
        Err(e) => {
            return RegisterResult {
                target,
                status: RegisterStatus::Failed,
                config_path: path,
                detail: format!("failed serializing JSON: {e}"),
            }
        }
    };
    if let Err(e) = std::fs::write(&path, serialized) {
        let detail = format!("failed writing {}: {e}", path.display());
        return RegisterResult {
            target,
            status: RegisterStatus::Failed,
            config_path: path,
            detail,
        };
    }

    let status = if existing.is_some() {
        RegisterStatus::Updated
    } else {
        RegisterStatus::Registered
    };
    RegisterResult {
        target,
        status,
        config_path: path.clone(),
        detail: format!("configured {} in {}", spec.name, path.display()),
    }
}

fn unregister_json_target(
    home_dir: &Path,
    target: AgentTarget,
    server_name: &str,
) -> RegisterResult {
    let path = target.config_path(home_dir);
    if !path.exists() {
        return RegisterResult {
            target,
            status: RegisterStatus::NotPresent,
            config_path: path,
            detail: "config file not present".to_string(),
        };
    }
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return RegisterResult {
            target,
            status: RegisterStatus::Failed,
            config_path: path.clone(),
            detail: format!("could not read {}", path.display()),
        };
    };
    let Ok(Value::Object(mut root)) = serde_json::from_str::<Value>(&raw) else {
        return RegisterResult {
            target,
            status: RegisterStatus::Failed,
            config_path: path.clone(),
            detail: format!("{} is not valid JSON; refusing to modify", path.display()),
        };
    };
    let removed = root
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .is_some_and(|servers| servers.remove(server_name).is_some());

    if !removed {
        return RegisterResult {
            target,
            status: RegisterStatus::NotPresent,
            config_path: path,
            detail: format!("{} not configured", server_name),
        };
    }

    let serialized = serde_json::to_string_pretty(&Value::Object(root)).unwrap_or_default() + "\n";
    if let Err(e) = std::fs::write(&path, serialized) {
        return RegisterResult {
            target,
            status: RegisterStatus::Failed,
            config_path: path,
            detail: format!("failed writing config: {e}"),
        };
    }

    if target == AgentTarget::Antigravity {
        let cache_dir = home_dir.join(".gemini/antigravity/mcp").join(server_name);
        if cache_dir.exists() {
            let _ = std::fs::remove_dir_all(cache_dir);
        }
    }

    RegisterResult {
        target,
        status: RegisterStatus::Removed,
        config_path: path.clone(),
        detail: format!("removed {server_name} from {}", path.display()),
    }
}

fn codex_markers(server_name: &str) -> (String, String) {
    if server_name == "headroom" {
        (CODEX_MARKER_START.to_string(), CODEX_MARKER_END.to_string())
    } else {
        (
            format!("# --- Headroom MCP server: {server_name} ---"),
            format!("# --- end Headroom MCP server: {server_name} ---"),
        )
    }
}

fn render_codex_block(spec: &ServerSpec) -> String {
    let (start, end) = codex_markers(&spec.name);
    let args_toml = spec
        .args
        .iter()
        .map(|a| format!("\"{}\"", escape_toml_str(a)))
        .collect::<Vec<_>>()
        .join(", ");
    let mut lines = vec![
        start,
        format!("[mcp_servers.{}]", format_toml_key(&spec.name)),
        format!("command = \"{}\"", escape_toml_str(&spec.command)),
        format!("args = [{args_toml}]"),
    ];
    if !spec.env.is_empty() {
        lines.push(format!("[mcp_servers.{}.env]", format_toml_key(&spec.name)));
        for (k, v) in &spec.env {
            lines.push(format!(
                "{} = \"{}\"",
                format_toml_key(k),
                escape_toml_str(v)
            ));
        }
    }
    lines.push(end);
    lines.join("\n") + "\n"
}

fn escape_toml_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn format_toml_key(key: &str) -> String {
    if key
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && !key.is_empty()
    {
        key.to_string()
    } else {
        format!("\"{}\"", escape_toml_str(key))
    }
}

fn read_codex_entry(path: &Path, server_name: &str) -> Option<ServerSpec> {
    let raw = std::fs::read_to_string(path).ok()?;
    let parsed: toml::Value = toml::from_str(&raw).ok()?;
    let entry = parsed
        .get("mcp_servers")?
        .as_table()?
        .get(server_name)?
        .as_table()?;
    let command = entry.get("command")?.as_str()?.to_string();
    let args = entry
        .get("args")
        .and_then(toml::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let mut env = BTreeMap::new();
    if let Some(env_tbl) = entry.get("env").and_then(toml::Value::as_table) {
        for (k, v) in env_tbl {
            if let Some(s) = v.as_str() {
                env.insert(k.clone(), s.to_string());
            }
        }
    }
    Some(ServerSpec {
        name: server_name.to_string(),
        command,
        args,
        env,
    })
}

fn strip_marker_span(raw: &str, start_marker: &str, end_marker: &str) -> (String, bool) {
    let lines: Vec<&str> = raw.split_inclusive('\n').collect();
    let Some(start_idx) = lines
        .iter()
        .position(|ln| ln.trim_end_matches(['\r', '\n']) == start_marker)
    else {
        return (raw.to_string(), false);
    };
    let Some(rel_end) = lines[start_idx + 1..]
        .iter()
        .position(|ln| ln.trim_end_matches(['\r', '\n']) == end_marker)
    else {
        return (raw.to_string(), false);
    };
    let end_idx = start_idx + 1 + rel_end;
    let mut out = String::with_capacity(raw.len());
    for (idx, ln) in lines.iter().enumerate() {
        if idx < start_idx || idx > end_idx {
            out.push_str(ln);
        }
    }
    (out, true)
}

fn register_codex(home_dir: &Path, spec: &ServerSpec, policy: OverwritePolicy) -> RegisterResult {
    let target = AgentTarget::Codex;
    let path = target.config_path(home_dir);
    let existing_content = if path.exists() {
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return RegisterResult {
                target,
                status: RegisterStatus::Failed,
                config_path: path.clone(),
                detail: format!("could not read {}", path.display()),
            };
        };
        if toml::from_str::<toml::Value>(&raw).is_err() {
            return RegisterResult {
                target,
                status: RegisterStatus::Failed,
                config_path: path.clone(),
                detail: format!(
                    "{} is not valid TOML; refusing to overwrite",
                    path.display()
                ),
            };
        }
        raw
    } else {
        String::new()
    };

    let existing = read_codex_entry(&path, &spec.name);
    if let Some(ref cur) = existing {
        if cur == spec {
            return RegisterResult {
                target,
                status: RegisterStatus::Already,
                config_path: path,
                detail: "already matches current configuration".to_string(),
            };
        }
        if policy == OverwritePolicy::PreserveExisting {
            return RegisterResult {
                target,
                status: RegisterStatus::Mismatch,
                config_path: path,
                detail: format!(
                    "existing `{}` configuration differs (re-run with --force to update)",
                    spec.name
                ),
            };
        }
    }

    let (start_marker, end_marker) = codex_markers(&spec.name);
    let (stripped, _) = strip_marker_span(&existing_content, &start_marker, &end_marker);
    let block = render_codex_block(spec);

    let mut updated = stripped.trim_end_matches('\n').to_string();
    if !updated.is_empty() {
        updated.push_str("\n\n");
    }
    updated.push_str(&block);

    if toml::from_str::<toml::Value>(&updated).is_err() {
        return RegisterResult {
            target,
            status: RegisterStatus::Failed,
            config_path: path,
            detail: "updated TOML failed validation; aborting write".to_string(),
        };
    }

    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return RegisterResult {
                target,
                status: RegisterStatus::Failed,
                config_path: path,
                detail: format!("failed creating parent dir: {e}"),
            };
        }
    }
    if let Err(e) = std::fs::write(&path, updated) {
        let detail = format!("failed writing {}: {e}", path.display());
        return RegisterResult {
            target,
            status: RegisterStatus::Failed,
            config_path: path,
            detail,
        };
    }

    let status = if existing.is_some() {
        RegisterStatus::Updated
    } else {
        RegisterStatus::Registered
    };
    RegisterResult {
        target,
        status,
        config_path: path.clone(),
        detail: format!("configured {} in {}", spec.name, path.display()),
    }
}

fn unregister_codex(home_dir: &Path, server_name: &str) -> RegisterResult {
    let target = AgentTarget::Codex;
    let path = target.config_path(home_dir);
    if !path.exists() {
        return RegisterResult {
            target,
            status: RegisterStatus::NotPresent,
            config_path: path,
            detail: "config file not present".to_string(),
        };
    }
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return RegisterResult {
            target,
            status: RegisterStatus::Failed,
            config_path: path.clone(),
            detail: format!("could not read {}", path.display()),
        };
    };
    if toml::from_str::<toml::Value>(&raw).is_err() {
        return RegisterResult {
            target,
            status: RegisterStatus::Failed,
            config_path: path.clone(),
            detail: format!("{} is not valid TOML; refusing to modify", path.display()),
        };
    }
    let (start_marker, end_marker) = codex_markers(server_name);
    let (updated, removed) = strip_marker_span(&raw, &start_marker, &end_marker);
    if !removed {
        return RegisterResult {
            target,
            status: RegisterStatus::NotPresent,
            config_path: path,
            detail: format!("{server_name} marker block not found"),
        };
    }
    let cleaned = if updated.trim().is_empty() {
        String::new()
    } else {
        format!("{}\n", updated.trim_end())
    };
    if let Err(e) = std::fs::write(&path, cleaned) {
        let detail = format!("failed writing {}: {e}", path.display());
        return RegisterResult {
            target,
            status: RegisterStatus::Failed,
            config_path: path,
            detail,
        };
    }
    RegisterResult {
        target,
        status: RegisterStatus::Removed,
        config_path: path.clone(),
        detail: format!("removed {server_name} from {}", path.display()),
    }
}

/// Format a summary line for CLI display without exposing JSON noise.
#[must_use]
pub fn format_register_summary(result: &RegisterResult) -> String {
    let tag = match result.status {
        RegisterStatus::Registered => "installed",
        RegisterStatus::Updated => "updated",
        RegisterStatus::Already => "already up-to-date",
        RegisterStatus::Mismatch => "mismatch",
        RegisterStatus::Removed => "removed",
        RegisterStatus::NotPresent => "not present",
        RegisterStatus::Failed => "failed",
    };
    format!(
        "  [{}] {}: {} ({})",
        tag,
        result.target.display_name(),
        result.detail,
        result.config_path.display()
    )
}

/// Produce a JSON value representation of status entries for `--json` CLI output.
#[must_use]
pub fn status_as_json(statuses: &[AgentStatus]) -> Value {
    let items: Vec<Value> = statuses
        .iter()
        .map(|s| {
            json!({
                "agent": s.target.as_str(),
                "display_name": s.target.display_name(),
                "detected": s.detected,
                "registered": s.registered,
                "config_path": s.config_path.display().to_string(),
            })
        })
        .collect();
    Value::Array(items)
}
