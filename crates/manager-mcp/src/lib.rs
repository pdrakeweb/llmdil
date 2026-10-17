//! MCP server registry (spec §8).
//!
//! GitHub and Google Drive MCP servers, each in mode local | remote |
//! disabled. Local servers are supervised through `manager-supervisor`
//! (spawn args from the [`ServerSpec`]); remote servers are URL +
//! credential-presence configuration only. Credentials are never read —
//! only presence is checked, via a caller-supplied predicate, and the UI
//! shows env *keys* only.
//!
//! [`editor_mcp_config`] generates the `mcpServers` JSON object consumed by
//! Continue (`config.json`) and Cline (`cline_mcp_settings.json`).

use manager_supervisor::{ProcessSpec, Supervisor, SupervisorError};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Where an MCP server runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpMode {
    Local,
    Remote,
    Disabled,
}

/// How the app talks to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
    Http,
}

/// One MCP server's static description.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSpec {
    /// Stable id, e.g. "github".
    pub id: String,
    /// Display name, e.g. "GitHub".
    pub name: String,
    pub transport: McpTransport,
    pub mode: McpMode,
    /// Command for stdio transport (e.g. "npx"); None for http.
    pub command: Option<String>,
    /// Args for stdio transport.
    pub args: Vec<String>,
    /// Env var KEYS the server needs. Values are never stored here.
    pub env_keys: Vec<String>,
    /// Base URL for http transport.
    pub url: Option<String>,
    /// Windows Credential Manager target holding the credential (name only).
    pub cred_ref: Option<String>,
    /// Package reference for install documentation.
    pub package: String,
}

/// The two curated servers from spec §8.
pub fn default_servers() -> Vec<ServerSpec> {
    vec![
        ServerSpec {
            id: "github".to_string(),
            name: "GitHub".to_string(),
            transport: McpTransport::Stdio,
            mode: McpMode::Local,
            command: Some("npx".to_string()),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-github".to_string(),
            ],
            env_keys: vec!["GITHUB_PERSONAL_ACCESS_TOKEN".to_string()],
            url: None,
            cred_ref: Some("llm-manager/mcp/github".to_string()),
            package: "@modelcontextprotocol/server-github".to_string(),
        },
        ServerSpec {
            id: "gdrive".to_string(),
            name: "Google Drive".to_string(),
            transport: McpTransport::Stdio,
            mode: McpMode::Local,
            command: Some("npx".to_string()),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-gdrive".to_string(),
            ],
            env_keys: vec!["GDRIVE_CREDENTIALS_PATH".to_string()],
            url: None,
            cred_ref: Some("llm-manager/mcp/gdrive".to_string()),
            package: "@modelcontextprotocol/server-gdrive".to_string(),
        },
    ]
}

/// Live status of one server for the Editor & MCP page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerStatus {
    pub id: String,
    pub name: String,
    pub mode: McpMode,
    pub transport: McpTransport,
    /// True when a supervised process for this server is currently live.
    pub running: bool,
    /// Env keys the server needs (values never exposed).
    pub env_keys: Vec<String>,
}

/// Status of every known server. `servers` is the registry (usually
/// [`default_servers`] with modes applied); running state comes from the
/// supervisor's live map.
pub fn list_status(supervisor: &Supervisor, servers: &[ServerSpec]) -> Vec<McpServerStatus> {
    let live_ids: std::collections::HashSet<String> = supervisor
        .status()
        .into_iter()
        .filter(|s| matches!(s.state, manager_supervisor::ProcessState::Running))
        .map(|s| s.id)
        .collect();
    servers
        .iter()
        .map(|s| McpServerStatus {
            id: s.id.clone(),
            name: s.name.clone(),
            mode: s.mode,
            transport: s.transport,
            running: live_ids.contains(&process_id(&s.id)),
            env_keys: s.env_keys.clone(),
        })
        .collect()
}

/// Supervisor process id for a server.
pub fn process_id(server_id: &str) -> String {
    format!("mcp-{server_id}")
}

/// Build the [`ProcessSpec`] for a local stdio server. Returns `None` for
/// remote/disabled servers (nothing to supervise) and for local servers
/// without a command.
///
/// Env *values* are injected by the caller at spawn time (resolved from
/// Credential Manager per invocation); this spec carries keys only.
pub fn process_spec_for(server: &ServerSpec) -> Option<ProcessSpec> {
    if server.mode != McpMode::Local || server.transport != McpTransport::Stdio {
        return None;
    }
    let command = server.command.as_ref()?;
    Some(ProcessSpec {
        id: process_id(&server.id),
        program: PathBuf::from(command),
        args: server.args.clone(),
        env: Vec::new(),
        workdir: None,
        port: None,
    })
}

/// Register and spawn a local server. Idempotent-ish: an already-running
/// server is left alone (`Ok`).
pub async fn spawn_local(supervisor: &mut Supervisor, server: &ServerSpec) -> Result<(), McpError> {
    let spec = process_spec_for(server).ok_or_else(|| {
        McpError::InvalidSpec(format!(
            "server {:?} is not a local stdio server; nothing to spawn",
            server.id
        ))
    })?;
    let id = spec.id.clone();
    let already_running = supervisor
        .status()
        .iter()
        .any(|s| s.id == id && matches!(s.state, manager_supervisor::ProcessState::Running));
    if already_running {
        return Ok(());
    }
    supervisor.register_spec(spec);
    supervisor
        .spawn_backend(&id)
        .await
        .map_err(|e| McpError::Supervisor(e.to_string()))
}

/// Stop a local server's supervised process. Not-running is `Ok`.
pub async fn stop_local(supervisor: &mut Supervisor, server: &ServerSpec) -> Result<(), McpError> {
    let id = process_id(&server.id);
    let running = supervisor
        .status()
        .iter()
        .any(|s| s.id == id && matches!(s.state, manager_supervisor::ProcessState::Running));
    if !running {
        return Ok(());
    }
    supervisor
        .stop(&id, true)
        .await
        .map_err(|e| McpError::Supervisor(e.to_string()))
}

/// Map each required env key to credential presence.
///
/// `exists` answers "does Credential Manager target `t` exist" for the
/// server's `cred_ref`-derived target; the credential *value* is never
/// read. When the server has no `cred_ref`, every key reports `false`
/// (presence unknown) rather than guessing.
pub fn credential_presence<F>(server: &ServerSpec, exists: F) -> Vec<(String, bool)>
where
    F: Fn(&str) -> bool,
{
    server
        .env_keys
        .iter()
        .map(|key| {
            let present = server
                .cred_ref
                .as_ref()
                .map(|target| exists(target))
                .unwrap_or(false);
            (key.clone(), present)
        })
        .collect()
}

/// Generate the `mcpServers` JSON object for editor configs.
///
/// Shape matches what Continue (`config.json`) and Cline
/// (`cline_mcp_settings.json`) consume:
/// `{ "mcpServers": { "<id>": { "command", "args", "env": {KEY: ""},
/// "disabled": bool } } }`. Env values are empty strings — the app injects
/// real values at spawn from Credential Manager. Remote (http) servers are
/// emitted as `{ "url": ... }`.
pub fn editor_mcp_config(servers: &[ServerSpec]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for s in servers {
        let entry = match s.transport {
            McpTransport::Http => {
                let mut e = serde_json::Map::new();
                if let Some(url) = &s.url {
                    e.insert("url".to_string(), serde_json::Value::from(url.as_str()));
                }
                e.insert(
                    "disabled".to_string(),
                    serde_json::Value::from(s.mode == McpMode::Disabled),
                );
                serde_json::Value::Object(e)
            }
            McpTransport::Stdio => {
                let mut e = serde_json::Map::new();
                if let Some(cmd) = &s.command {
                    e.insert("command".to_string(), serde_json::Value::from(cmd.as_str()));
                }
                e.insert("args".to_string(), serde_json::Value::from(s.args.clone()));
                let env: serde_json::Map<String, serde_json::Value> = s
                    .env_keys
                    .iter()
                    .map(|k| (k.clone(), serde_json::Value::from("")))
                    .collect();
                e.insert("env".to_string(), serde_json::Value::Object(env));
                e.insert(
                    "disabled".to_string(),
                    serde_json::Value::from(s.mode == McpMode::Disabled),
                );
                serde_json::Value::Object(e)
            }
        };
        map.insert(s.id.clone(), entry);
    }
    serde_json::Value::Object(
        [("mcpServers".to_string(), serde_json::Value::Object(map))]
            .into_iter()
            .collect(),
    )
}

/// MCP errors.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("supervisor error: {0}")]
    Supervisor(String),
    #[error("invalid server spec: {0}")]
    InvalidSpec(String),
}

impl From<SupervisorError> for McpError {
    fn from(e: SupervisorError) -> Self {
        McpError::Supervisor(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_servers_are_github_and_gdrive() {
        let servers = default_servers();
        assert_eq!(servers.len(), 2);
        let ids: Vec<&str> = servers.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"github"));
        assert!(ids.contains(&"gdrive"));
        for s in &servers {
            assert_eq!(s.transport, McpTransport::Stdio);
            assert_eq!(s.mode, McpMode::Local);
            assert!(!s.env_keys.is_empty());
            // Secrets never stored: only names/refs.
            assert!(s
                .cred_ref
                .as_ref()
                .map(|r| !r.contains('='))
                .unwrap_or(true));
        }
    }

    #[test]
    fn editor_mcp_config_shape() {
        let v = editor_mcp_config(&default_servers());
        let servers = v["mcpServers"].as_object().unwrap();
        assert_eq!(servers.len(), 2);
        let gh = &servers["github"];
        assert_eq!(gh["command"], serde_json::Value::from("npx"));
        assert!(gh["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == &serde_json::Value::from("@modelcontextprotocol/server-github")));
        // Env keys present, values empty.
        assert_eq!(
            gh["env"]["GITHUB_PERSONAL_ACCESS_TOKEN"],
            serde_json::Value::from("")
        );
        assert_eq!(gh["disabled"], serde_json::Value::from(false));
    }

    #[test]
    fn editor_mcp_config_marks_disabled_servers() {
        let mut servers = default_servers();
        servers[0].mode = McpMode::Disabled;
        let v = editor_mcp_config(&servers);
        assert_eq!(
            v["mcpServers"]["github"]["disabled"],
            serde_json::Value::from(true)
        );
    }

    #[test]
    fn editor_mcp_config_remote_http_uses_url() {
        let server = ServerSpec {
            id: "remote".to_string(),
            name: "Remote".to_string(),
            transport: McpTransport::Http,
            mode: McpMode::Remote,
            command: None,
            args: vec![],
            env_keys: vec![],
            url: Some("https://mcp.example.com/sse".to_string()),
            cred_ref: None,
            package: String::new(),
        };
        let v = editor_mcp_config(std::slice::from_ref(&server));
        assert_eq!(
            v["mcpServers"]["remote"]["url"],
            serde_json::Value::from("https://mcp.example.com/sse")
        );
        assert!(v["mcpServers"]["remote"].get("command").is_none());
    }

    #[test]
    fn process_spec_for_only_local_stdio() {
        let servers = default_servers();
        let spec = process_spec_for(&servers[0]).expect("local stdio has a spec");
        assert_eq!(spec.id, "mcp-github");
        assert_eq!(spec.program, PathBuf::from("npx"));
        assert!(spec.env.is_empty(), "no secret values in the spec");

        let mut remote = servers[0].clone();
        remote.mode = McpMode::Remote;
        assert!(process_spec_for(&remote).is_none());
        let mut disabled = servers[0].clone();
        disabled.mode = McpMode::Disabled;
        assert!(process_spec_for(&disabled).is_none());
    }

    #[test]
    fn credential_presence_never_reads_values() {
        use std::cell::RefCell;
        let servers = default_servers();
        let saw_targets = RefCell::new(Vec::new());
        let presence = credential_presence(&servers[0], |target| {
            saw_targets.borrow_mut().push(target.to_string());
            target == "llm-manager/mcp/github"
        });
        assert_eq!(
            presence,
            vec![("GITHUB_PERSONAL_ACCESS_TOKEN".to_string(), true)]
        );
        // Only the target *name* was queried.
        assert_eq!(
            saw_targets.borrow().as_slice(),
            &["llm-manager/mcp/github".to_string()]
        );

        let mut no_ref = servers[0].clone();
        no_ref.cred_ref = None;
        let presence = credential_presence(&no_ref, |_| true);
        assert_eq!(
            presence,
            vec![("GITHUB_PERSONAL_ACCESS_TOKEN".to_string(), false)]
        );
    }

    #[tokio::test]
    async fn spawn_local_rejects_non_local() {
        let mut sup = Supervisor::new(manager_supervisor::BackoffPolicy {
            base: std::time::Duration::from_millis(10),
            max: std::time::Duration::from_secs(1),
            max_retries: 0,
        });
        let mut servers = default_servers();
        servers[0].mode = McpMode::Disabled;
        let err = spawn_local(&mut sup, &servers[0]).await.unwrap_err();
        assert!(matches!(err, McpError::InvalidSpec(_)));

        // stop_local on a never-spawned server is Ok.
        stop_local(&mut sup, &servers[0]).await.unwrap();
    }
}
