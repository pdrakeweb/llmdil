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

/// Derive the `package` string for a runtime spec from stored args:
/// the first arg that does not start with `-` — for the curated
/// servers that is exactly the package reference (`npx -y <pkg>`,
/// `uvx <pkg>`). `McpServerConfig` has no package field (design §3.1),
/// so this derivation is the documented choice; when no such arg
/// exists (e.g. `python -m …`, where `-m`'s operand also qualifies —
/// it is the first non-flag arg — or a bare command with no args)
/// the result is that first non-flag arg or, failing that, empty.
/// `package` is documentation-only (install docs), never executed.
fn derive_package(args: &[String]) -> String {
    args.iter()
        .find(|a| !a.starts_with('-'))
        .cloned()
        .unwrap_or_default()
}

/// Build the runtime view ([`ServerSpec`]) of a persisted registry
/// entry (`manager_config::McpServerConfig`, MCP design §3.1/§12
/// Phase 1: "`ServerSpec` becomes the runtime view of
/// `McpServerConfig`").
///
/// Exposure → mode mapping:
/// - `Local` → [`McpMode::Local`]: app-supervised, spawnable.
/// - `Disabled` → [`McpMode::Disabled`].
/// - `Gateway` → [`McpMode::Remote`]: this is *not* a claim that the
///   server is a remote HTTP server. `Remote` is the existing mode
///   meaning "not app-supervised: configuration only, nothing to
///   spawn" — and that safety property is exactly what a Gateway
///   server needs on the legacy local paths: the gateway (LiteLLM),
///   not this app, hosts it, so `process_spec_for` must return
///   `None` and `spawn_local` must refuse it. Mapping `Gateway` to
///   `Local` would let the legacy spawn path launch a second,
///   unsupervised-by-the-gateway copy; mapping it to `Remote`
///   preserves the invariant that only `Local` servers are ever
///   spawned by the app. The persisted `exposure` remains the source
///   of truth wherever the distinction matters (e.g. `mcp_list`
///   reports it separately from this derived mode).
///
/// `display_name` → `name` (falling back to the id when the stored
/// display name is empty, so the UI never shows a blank name);
/// `package` is derived from `args` (see [`derive_package`]).
pub fn server_spec_from_config(cfg: &manager_config::McpServerConfig) -> ServerSpec {
    let transport = match cfg.transport {
        manager_config::McpTransport::Stdio => McpTransport::Stdio,
        manager_config::McpTransport::Http => McpTransport::Http,
    };
    let mode = match cfg.exposure {
        manager_config::McpExposure::Local => McpMode::Local,
        manager_config::McpExposure::Disabled => McpMode::Disabled,
        // See the mapping rationale in this function's doc comment.
        manager_config::McpExposure::Gateway => McpMode::Remote,
    };
    ServerSpec {
        id: cfg.id.clone(),
        name: if cfg.display_name.trim().is_empty() {
            cfg.id.clone()
        } else {
            cfg.display_name.clone()
        },
        transport,
        mode,
        command: cfg.command.clone(),
        args: cfg.args.clone(),
        env_keys: cfg.env_keys.clone(),
        url: cfg.url.clone(),
        cred_ref: cfg.cred_ref.clone(),
        package: derive_package(&cfg.args),
    }
}

/// Runtime views of every entry in a persisted registry.
pub fn server_specs_from_config(cfg: &manager_config::McpConfig) -> Vec<ServerSpec> {
    cfg.servers.iter().map(server_spec_from_config).collect()
}

impl From<&manager_config::McpServerConfig> for ServerSpec {
    fn from(cfg: &manager_config::McpServerConfig) -> Self {
        server_spec_from_config(cfg)
    }
}

/// Credential Manager targets accepted for a server's credential
/// presence (MCP design §10.1 dual-target acceptance): the server's
/// own `cred_ref` first, then — for the two legacy servers only — the
/// other spelling of its target (unified
/// `local-llm-service-manager/<id>-mcp` vs legacy
/// `llm-manager/mcp/<id>`), so a credential stored under either
/// prefix keeps working during the transition. The returned order is
/// preference order: the first target that exists is the one
/// reported as found.
pub fn accepted_cred_targets(server: &ServerSpec) -> Vec<String> {
    // No cred_ref named: no targets are accepted (presence is
    // unknown, as in `credential_presence`) — the known legacy
    // targets are not guessed at for a server that names none.
    let Some(primary) = &server.cred_ref else {
        return Vec::new();
    };
    let mut out: Vec<String> = vec![primary.clone()];
    let alternate = match server.id.as_str() {
        "github" => Some([
            manager_config::GITHUB_MCP_CRED_REF,
            manager_config::LEGACY_GITHUB_MCP_CRED_REF,
        ]),
        "gdrive" => Some([
            manager_config::GDRIVE_MCP_CRED_REF,
            manager_config::LEGACY_GDRIVE_MCP_CRED_REF,
        ]),
        _ => None,
    };
    if let Some(pair) = alternate {
        for target in pair {
            if !out.iter().any(|t| t == target) {
                out.push(target.to_string());
            }
        }
    }
    out
}

/// The first accepted credential target that exists for `server`
/// (see [`accepted_cred_targets`]), or `None` when none does / the
/// server names no credential. Presence only — values never read.
pub fn credential_target_found<F>(server: &ServerSpec, exists: F) -> Option<String>
where
    F: Fn(&str) -> bool,
{
    accepted_cred_targets(server)
        .into_iter()
        .find(|t| exists(t))
}

/// The two curated servers from spec §8.
///
/// LEGACY SEED SOURCE ONLY: this is no longer the app's source of
/// truth. The persisted registry (`AppConfig.mcp`, seeded by
/// `manager_config::seeded_mcp_servers`) is; this function is kept
/// for back-compat (existing callers/tests) and as the historical
/// record the seed was derived from. Note its divergences from the
/// seed, unified by design §10.1: it uses the legacy credential
/// targets (`llm-manager/mcp/…`) and the legacy Drive env key
/// (`GDRIVE_CREDENTIALS_PATH`).
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
        low_priority: false,
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

    fn cfg_server(exposure: manager_config::McpExposure) -> manager_config::McpServerConfig {
        manager_config::McpServerConfig {
            id: "github".to_string(),
            display_name: "GitHub".to_string(),
            transport: manager_config::McpTransport::Stdio,
            exposure,
            command: Some("npx".to_string()),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-github".to_string(),
            ],
            url: None,
            env_keys: vec!["GITHUB_PERSONAL_ACCESS_TOKEN".to_string()],
            cred_ref: Some(manager_config::GITHUB_MCP_CRED_REF.to_string()),
            signup_url: None,
            credential_hint: None,
            allowed_tools: None,
            metered: false,
            rpm_limit: None,
        }
    }

    #[test]
    fn conversion_maps_exposure_to_mode_safely() {
        use manager_config::McpExposure;
        let local = server_spec_from_config(&cfg_server(McpExposure::Local));
        assert_eq!(local.mode, McpMode::Local);
        assert!(process_spec_for(&local).is_some());
        let disabled = server_spec_from_config(&cfg_server(McpExposure::Disabled));
        assert_eq!(disabled.mode, McpMode::Disabled);
        assert!(process_spec_for(&disabled).is_none());
        // Gateway -> Remote: the legacy local paths must never spawn
        // a gateway-hosted server.
        let gateway = server_spec_from_config(&cfg_server(McpExposure::Gateway));
        assert_eq!(gateway.mode, McpMode::Remote);
        assert!(process_spec_for(&gateway).is_none());
        // Fields carry over; package is derived from args.
        assert_eq!(gateway.name, "GitHub");
        assert_eq!(gateway.package, "@modelcontextprotocol/server-github");
        assert_eq!(gateway.transport, McpTransport::Stdio);
        assert_eq!(
            gateway.cred_ref.as_deref(),
            Some(manager_config::GITHUB_MCP_CRED_REF)
        );
    }

    #[test]
    fn conversion_of_seeded_registry_matches_legacy_shape() {
        let specs = server_specs_from_config(&manager_config::McpConfig::seeded());
        // Phase 3: the seed carries the legacy pair plus the curated
        // pair. The legacy entries keep their Local shape exactly;
        // the curated entries map by exposure (Gateway -> Remote,
        // Disabled -> Disabled) and are never app-spawned.
        assert_eq!(specs.len(), 4);
        let legacy: Vec<&ServerSpec> = specs
            .iter()
            .filter(|s| s.id == "github" || s.id == "gdrive")
            .collect();
        assert_eq!(legacy.len(), 2);
        assert!(legacy.iter().all(|s| s.mode == McpMode::Local));
        let brave = specs.iter().find(|s| s.id == "brave_search").unwrap();
        assert_eq!(brave.mode, McpMode::Remote);
        assert_eq!(brave.package, "@brave/brave-search-mcp-server@2.1.4");
        let fetch = specs.iter().find(|s| s.id == "web_fetch").unwrap();
        assert_eq!(fetch.mode, McpMode::Disabled);
        let gd = specs.iter().find(|s| s.id == "gdrive").unwrap();
        // Canonicalised env key in the registry view.
        assert_eq!(gd.env_keys, vec!["GOOGLE_APPLICATION_CREDENTIALS"]);
        assert_eq!(gd.package, "@modelcontextprotocol/server-gdrive");
        // Empty display name falls back to the id.
        let mut blank = cfg_server(manager_config::McpExposure::Local);
        blank.display_name = String::new();
        assert_eq!(server_spec_from_config(&blank).name, "github");
        // No non-flag arg -> empty package.
        let mut no_pkg = cfg_server(manager_config::McpExposure::Local);
        no_pkg.args = vec!["-y".to_string()];
        assert_eq!(server_spec_from_config(&no_pkg).package, "");
    }

    #[test]
    fn accepted_cred_targets_dual_for_legacy_only() {
        let specs = server_specs_from_config(&manager_config::McpConfig::seeded());
        let gh = specs.iter().find(|s| s.id == "github").unwrap();
        assert_eq!(
            accepted_cred_targets(gh),
            vec![
                manager_config::GITHUB_MCP_CRED_REF.to_string(),
                manager_config::LEGACY_GITHUB_MCP_CRED_REF.to_string(),
            ]
        );
        // Found under the legacy target only -> reported as that target.
        let found =
            credential_target_found(gh, |t| t == manager_config::LEGACY_GITHUB_MCP_CRED_REF);
        assert_eq!(
            found.as_deref(),
            Some(manager_config::LEGACY_GITHUB_MCP_CRED_REF)
        );
        assert_eq!(credential_target_found(gh, |_| false), None);
        // A non-legacy server gets only its own target.
        let mut custom = gh.clone();
        custom.id = "custom".to_string();
        custom.cred_ref = Some("some/target".to_string());
        assert_eq!(accepted_cred_targets(&custom), vec!["some/target"]);
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
