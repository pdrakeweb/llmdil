//! Gateway spawn fingerprint, applied stamp, lifecycle logs, and config drift checks.

use super::*;

/// The gateway inputs that travel to the spawned process via argv
/// and environment rather than through `config.yaml` — invisible to
/// a YAML-only comparison. The start-gateway step records the
/// fingerprint it actually spawned with (see
/// [`gateway_spawn_state_json`]); the drift check compares that
/// record against the fingerprint of the current configuration.
/// Secret values are never stored: credentials contribute only
/// their SHA-256 hashes, enough to detect a rotation.
/// (Serialized via [`gateway_spawn_state_json`].)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewaySpawnFingerprint {
    pub port: u16,
    pub bind_host: String,
    /// The app-managed PostgreSQL port when the gateway was spawned
    /// with a DATABASE_URL; `None` when it ran database-free.
    pub postgres_port: Option<u16>,
    /// SHA-256 of the LiteLLM master key the gateway was spawned
    /// with (empty when no key was in play).
    pub master_key_hash: String,
    /// SHA-256 of the OpenRouter API key the gateway was spawned
    /// with (empty when the tier was off or unarmed).
    pub openrouter_key_hash: String,
    /// Whether the gateway was spawned with
    /// `LITELLM_ENABLE_MCP_STDIO=true` (MCP design §4.2): true
    /// exactly when the armed set for that start contained a stdio
    /// server. Serde-defaulted (false) so spawn-state records
    /// written before the MCP feature still parse — and still
    /// compare current for configurations with no Gateway servers,
    /// whose armed set is empty (see `mcp_servers_hash`).
    #[serde(default)]
    pub mcp_stdio_enabled: bool,
    /// Hash of the ARMED registered MCP server set the gateway was
    /// spawned with ([`mcp_armed_servers_hash`]); empty for an
    /// empty armed set. Serde-defaulted ("") like
    /// `mcp_stdio_enabled`, for the same pre-feature compatibility.
    #[serde(default)]
    pub mcp_servers_hash: String,
    /// Hash of the SearXNG search-provider spawn inputs (MCP design
    /// §6.3, Phase 3): the `SEARXNG_API_BASE` env value the gateway
    /// was spawned with. Empty exactly when SearXNG contributed
    /// nothing to the spawn (disabled, or enabled with an empty
    /// base) — which is also what a pre-Phase-3 record holds, via
    /// the serde default, so searxng-less configurations still
    /// compare current against old records. Toggling SearXNG or
    /// changing its base changes this hash and registers as drift.
    #[serde(default)]
    pub searxng_hash: String,
}

/// The fingerprint contribution of the SearXNG settings (see
/// [`GatewaySpawnFingerprint::searxng_hash`]): the empty string when
/// SearXNG is not in play for a spawn (disabled, or no base
/// configured — generation emits no `search_tools:` block and the
/// spawn env carries no `SEARXNG_API_BASE` in either case), else a
/// SHA-256 over the enabled flag and the base URL. The base is
/// config, not a secret, but it is hashed like the other spawn
/// inputs for uniformity: the fingerprint detects change, it does
/// not need to carry the value.
pub fn searxng_fingerprint_hash(searxng: &manager_config::SearxngSettings) -> String {
    use sha2::Digest as _;
    let api_base = searxng.api_base.trim();
    if !searxng.enabled || api_base.is_empty() {
        return String::new();
    }
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"searxng:v1:enabled:");
    hasher.update(api_base.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn secret_fingerprint(secret: Option<&str>) -> String {
    use sha2::Digest as _;
    match secret {
        Some(s) if !s.is_empty() => format!("{:x}", sha2::Sha256::digest(s.as_bytes())),
        _ => String::new(),
    }
}

/// The spawn fingerprint of the current configuration: argv/env
/// inputs from `config`, credential hashes from the values the
/// caller resolved (the wizard reads Credential Manager; the app
/// layer passes what it read). `postgres_installed` is the ground
/// truth the step itself uses to decide whether DATABASE_URL is in
/// play. `mcp_armed` is the armed Gateway MCP server set for the
/// spawn being fingerprinted (from [`armed_gateway_mcp_servers`] —
/// the one arming rule): it contributes the stdio-flag state and
/// the registered-set hash, so toggling or editing a server, or a
/// credential becoming unreadable, registers as drift.
pub fn gateway_spawn_fingerprint(
    config: &AppConfig,
    postgres_installed: bool,
    master_key: Option<&str>,
    openrouter_key: Option<&str>,
    mcp_armed: &[&McpServerConfig],
) -> GatewaySpawnFingerprint {
    GatewaySpawnFingerprint {
        port: config.gateway.port,
        bind_host: config.gateway.bind_host.clone(),
        postgres_port: postgres_installed.then_some(config.postgres.port),
        master_key_hash: secret_fingerprint(master_key),
        openrouter_key_hash: secret_fingerprint(openrouter_key),
        mcp_stdio_enabled: mcp_armed_set_has_stdio(mcp_armed),
        mcp_servers_hash: mcp_armed_servers_hash(mcp_armed),
        searxng_hash: searxng_fingerprint_hash(&config.mcp.searxng),
    }
}

/// What a successful gateway spawn records, at
/// `<data_dir>/gateway/spawn-state.json`: the spawn fingerprint and
/// the hash of the generated YAML it spawned with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewaySpawnState {
    pub fingerprint: GatewaySpawnFingerprint,
    pub yaml_sha256: String,
    /// The LiteLLM version the start-gateway step probed in the
    /// gateway venv before this spawn (MCP design §4.3), or `None`
    /// when it could not be determined. Serde-defaulted so spawn
    /// records written before the field existed still parse.
    /// Recorded for visibility (the MCP view's `LiteLLM: <version>`
    /// line); it is not part of the drift comparison — a LiteLLM
    /// upgrade is an `updates` concern, not a config change.
    #[serde(default)]
    pub litellm_version: Option<String>,
}

/// What the last successful gateway start was spawned with, or `None`
/// when no (parseable) record exists.
pub fn recorded_gateway_spawn(data_dir: &Path) -> Option<GatewaySpawnState> {
    let text = std::fs::read_to_string(gateway_spawn_state_path(data_dir)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The `config.yaml` the last successful gateway start was spawned
/// with; `None` when there is no record or the file has been rewritten
/// since (a failed start).
pub fn applied_gateway_yaml(data_dir: &Path) -> Option<String> {
    let spawn = recorded_gateway_spawn(data_dir)?;
    let yaml = std::fs::read_to_string(data_dir.join("gateway").join("config.yaml")).ok()?;
    (yaml_fingerprint(&yaml) == spawn.yaml_sha256).then_some(yaml)
}

/// Whether saved changes the gateway reads from its command line —
/// port and bind address — wait for a gateway restart: `(port, bind)`.
/// Both false when no spawn is recorded.
pub fn gateway_listen_pending(config: &AppConfig, data_dir: &Path) -> (bool, bool) {
    match recorded_gateway_spawn(data_dir) {
        Some(s) => (
            s.fingerprint.port != config.gateway.port,
            s.fingerprint.bind_host != config.gateway.bind_host,
        ),
        None => (false, false),
    }
}

/// Path of the spawn-state record: `<data_dir>/gateway/spawn-state.json`.
pub fn gateway_spawn_state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("gateway").join("spawn-state.json")
}

/// The gateway's own lifecycle marker log:
/// `<data_dir>/logs/gateway.lifecycle.log` — the same file the
/// service host writes in service mode (`gateway_definition`'s
/// `lifecycle_log`), so both modes' markers land in one scope.
pub fn gateway_lifecycle_log(data_dir: &Path) -> PathBuf {
    data_dir.join("logs").join("gateway.lifecycle.log")
}

/// The app-managed PostgreSQL server's lifecycle marker log:
/// `<data_dir>/logs/postgres.lifecycle.log` (next to `postgres.log`).
pub fn postgres_lifecycle_log(data_dir: &Path) -> PathBuf {
    data_dir.join("logs").join("postgres.lifecycle.log")
}

/// Write one lifecycle marker for `service` (`"gateway"`,
/// `"postgres"`) into `scope_log` through the ops layer (the test
/// double records it). Best-effort, never fails.
pub(crate) fn mark(
    ops: &dyn SystemOps,
    scope_log: &Path,
    service: &str,
    event: &str,
    detail: Option<&str>,
) {
    let line = manager_supervisor::lifecycle::marker_line(
        chrono::Utc::now(),
        service,
        event,
        detail,
        None,
    );
    ops.append_marker(scope_log, &line);
}

/// Path of the gateway "applied" stamp:
/// `<data_dir>/gateway/applied.json`. Written only after a
/// start-gateway run fully succeeds; the activation plan resolves
/// gateway-scope entries against its `at` instead of the YAML mtime
/// (finding CR-1: the YAML is written before the stop, the
/// PostgreSQL start, the spawn and the health waits, so a failed
/// restart used to mark the plan applied).
pub fn gateway_applied_stamp_path(data_dir: &Path) -> PathBuf {
    data_dir.join("gateway").join("applied.json")
}

/// The applied stamp's on-disk form.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GatewayAppliedStamp {
    /// When the YAML this successful start runs with was written.
    at: chrono::DateTime<chrono::Utc>,
    /// The gateway pid that started successfully.
    pid: u32,
}

/// Serialize an applied stamp (see [`gateway_applied_stamp_path`]) for
/// the caller to persist — the start-gateway step writes it through
/// its ops layer so step tests stay hermetic.
pub fn gateway_applied_stamp_json(
    yaml_written_at: chrono::DateTime<chrono::Utc>,
    pid: u32,
) -> String {
    serde_json::to_string_pretty(&GatewayAppliedStamp {
        at: yaml_written_at,
        pid,
    })
    .unwrap_or_default()
}

/// Write the applied stamp atomically (temp file + rename), for
/// callers outside a wizard step.
pub fn write_gateway_applied_stamp(
    data_dir: &Path,
    yaml_written_at: chrono::DateTime<chrono::Utc>,
    pid: u32,
) -> std::io::Result<()> {
    let path = gateway_applied_stamp_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, gateway_applied_stamp_json(yaml_written_at, pid))?;
    std::fs::rename(&tmp, &path)
}

/// Parse an applied stamp's `at`. `None` for anything unparseable.
pub fn parse_gateway_applied_at(text: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    serde_json::from_str::<GatewayAppliedStamp>(text)
        .ok()
        .map(|s| s.at)
}

/// When the configuration the running gateway serves was written, as
/// recorded by the last fully successful start-gateway run. `None`
/// when no (parseable) stamp exists — pre-upgrade installs, where the
/// caller falls back to the YAML mtime.
pub fn gateway_applied_at(data_dir: &Path) -> Option<chrono::DateTime<chrono::Utc>> {
    let text = std::fs::read_to_string(gateway_applied_stamp_path(data_dir)).ok()?;
    parse_gateway_applied_at(&text)
}

fn yaml_fingerprint(yaml: &str) -> String {
    use sha2::Digest as _;
    format!(
        "{:x}",
        sha2::Sha256::digest(normalize_gateway_yaml(yaml).as_bytes())
    )
}

/// The SHA-256 hash of a generated gateway YAML document, with the
/// same CRLF/trailing-newline normalisation the drift check uses.
/// Public for the activation journal (design §10.2), which records
/// the pre-change YAML hash for drift visibility in the same terms
/// the spawn state uses.
pub fn gateway_yaml_hash(yaml: &str) -> String {
    yaml_fingerprint(yaml)
}

/// Serialize a successful spawn's state (the drift check's
/// baseline) for the caller to persist at
/// [`gateway_spawn_state_path`] — through the ops layer, so step
/// tests stay hermetic. Written by the start-gateway step after
/// the gateway is healthy.
pub fn gateway_spawn_state_json(
    fingerprint: &GatewaySpawnFingerprint,
    generated_yaml: &str,
    litellm_version: Option<&str>,
) -> String {
    let state = GatewaySpawnState {
        fingerprint: fingerprint.clone(),
        yaml_sha256: yaml_fingerprint(generated_yaml),
        litellm_version: litellm_version.map(str::to_string),
    };
    serde_json::to_string_pretty(&state).unwrap()
}

/// The LiteLLM version recorded in the gateway spawn-state record
/// at `<data_dir>/gateway/spawn-state.json` (MCP design §4.3), or
/// `None` when the record is absent, unparseable, or carries no
/// version (a spawn recorded before the field existed, or whose
/// probe could not determine one). Callers render `None` as
/// "not measured" — never as a guess (design principle 12).
pub fn recorded_litellm_version(data_dir: &Path) -> Option<String> {
    recorded_gateway_spawn(data_dir)?
        .litellm_version
        .filter(|v| !v.trim().is_empty())
}

/// Whether the running gateway's recorded spawn state still matches
/// the current configuration — the full drift check. The
/// start-gateway step rewrites `<data_dir>/gateway/config.yaml`
/// from the live config on every start, so an audit "gateway
/// healthy" result (or a persisted Done for the step) only proves
/// the step complete while the gateway was spawned from the current
/// configuration — a stale gateway answering the health probe says
/// nothing about the saved configuration.
///
/// The YAML alone does not carry every spawn input: the port and
/// bind host travel via argv, the database URL and the credential
/// values via the environment. Comparing only the file adopted a
/// stale `Done` after a port change, a bind change, a credential
/// rotation, or a PostgreSQL install — the gateway kept serving the
/// old configuration while the checklist said complete. So this
/// requires, in addition to the YAML matching what the current
/// configuration generates, a spawn-state record whose fingerprint
/// equals `current` and whose YAML hash equals the YAML on disk.
/// A missing record (a gateway started by an older build), drifted
/// contents, or a configuration that no longer generates (e.g. no
/// enabled groups) all count as NOT current.
/// `mcp_can_resolve` is the arming predicate for the drift path
/// (see [`armed_gateway_mcp_servers`]): the YAML this check
/// regenerates must be the YAML a start would write NOW, which is
/// generated from the armed-only filtered config — an unarmed
/// Gateway server omitted at the last start must not read as drift
/// forever, and a server that has become armed since must. The
/// app layer resolves this predicate by credential PRESENCE
/// (`credential_exists`), not by reading values: the accepted
/// semantic gap documented on [`armed_gateway_mcp_servers`].
pub fn gateway_config_is_current(
    config: &AppConfig,
    data_dir: &Path,
    current: &GatewaySpawnFingerprint,
    mcp_can_resolve: impl FnMut(&str) -> bool,
) -> bool {
    let filtered = config_with_armed_gateway_mcp_servers(config, mcp_can_resolve);
    let Ok(generated) = manager_gateway::generate_config_yaml(&filtered) else {
        return false;
    };
    let Ok(on_disk) = std::fs::read_to_string(data_dir.join("gateway").join("config.yaml")) else {
        return false;
    };
    if normalize_gateway_yaml(&generated) != normalize_gateway_yaml(&on_disk) {
        return false;
    }
    // The routing callback file is a second generated artifact
    // (routing-rules design §6.3): rule changes can alter it without
    // altering the YAML (a window or reserve change leaves the YAML
    // keys untouched), so it is byte-compared too — present and
    // equal when a typed rule is enabled, absent when none is.
    let routing_path = data_dir.join("gateway").join("llmdil_routing.py");
    match manager_gateway::generate_routing_callback(&filtered) {
        Some(expected) => {
            let Ok(disk_py) = std::fs::read_to_string(&routing_path) else {
                return false;
            };
            if normalize_gateway_yaml(&expected) != normalize_gateway_yaml(&disk_py) {
                return false;
            }
        }
        None => {
            if routing_path.exists() {
                return false;
            }
        }
    }
    let Some(state) = recorded_gateway_spawn(data_dir) else {
        return false;
    };
    state.fingerprint == *current && state.yaml_sha256 == yaml_fingerprint(&on_disk)
}

fn normalize_gateway_yaml(text: &str) -> String {
    text.replace("\r\n", "\n").trim_end().to_string()
}
