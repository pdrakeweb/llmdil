//! Versioned, serde-typed application configuration (spec §10).
//!
//! Single JSON file at `%APPDATA%\local-llm-service-manager\config.json`
//! (schema `version: u32`, current [`CURRENT_VERSION`]). Secrets are never
//! stored here — only credential *references* (Credential Manager target names).
//!
//! Loading: parse → [`migrate`] → [`validate`]. If the primary file fails to
//! parse or validate, the `.bak` last-known-good copy is tried; if that also
//! fails the primary error is returned. A missing file is an I/O error —
//! callers fall back to [`AppConfig::default_config`] on first run.
//!
//! Saving: [`validate`] first, then rotate the existing file to `.bak`,
//! then atomic write (temp file + rename).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub mod diagnostics;
pub mod diff;

pub use diagnostics::export_diagnostics;
pub use diff::{apply_config_diff, diff_config, BackendPatch, ConfigDiff, FieldChange};

/// Current config schema version.
pub const CURRENT_VERSION: u32 = 1;

/// Tolerance for tensor_split sum-to-1.0 checks.
const SPLIT_SUM_EPSILON: f32 = 1e-4;

/// Upper bound for the gateway request ring buffer length.
const MAX_REQUEST_LOG_LEN: usize = 100_000;

/// Top-level application configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub version: u32,
    /// Models, builds, logs, managed Python env.
    pub data_dir: PathBuf,
    /// window_id -> preferred presenter.
    pub view_modes: HashMap<String, ViewMode>,
    pub backends: Vec<BackendConfig>,
    pub gateway: GatewayConfig,
    pub models: Vec<ModelConfig>,
    /// Poll interval and retention.
    pub telemetry: TelemetryConfig,
    pub mxc: MxcPolicyRef,
    pub winml: WinMlConfig,
    /// The app-managed PostgreSQL server backing the LiteLLM
    /// gateway's database (admin UI, spend logs). Installed by the
    /// wizard's install-postgres step under the data dir; only its
    /// port is configuration — the binaries, cluster, and password
    /// are managed state (the password lives in Credential Manager).
    #[serde(default)]
    pub postgres: PostgresConfig,
    /// Per-component pin / auto-update toggles.
    pub updates: UpdatePolicy,
    pub notifications: NotificationConfig,
    /// Set when the user finishes the setup wizard; the UI opens the wizard
    /// on launch until this is true.
    #[serde(default)]
    pub setup_completed: bool,
    /// Set once the curated model catalog has been seeded into `models`
    /// (first run). Seeding keys off this marker rather than
    /// `models.is_empty()`: an operator who deletes every model must not
    /// have the catalog resurrected on the next launch.
    #[serde(default)]
    pub catalog_seeded: bool,
    /// Set once the one-time defaults repair has run (see
    /// [`repair_catalog_defaults`]): configurations written before the
    /// catalog carried real hash pins and parameter counts, and before
    /// the default backends were enabled by default, are repaired
    /// exactly once. Configurations created by current code are born
    /// with this set.
    #[serde(default)]
    pub defaults_repaired: bool,
    /// The catalog revision this configuration has synced to (see
    /// `CATALOG_REVISION` in manager-models). When the curated
    /// catalog gains entries, an existing configuration appends the
    /// missing models exactly once per revision (see
    /// [`append_missing_catalog_models`]) — a model the operator
    /// deliberately deleted is not resurrected until the catalog
    /// next changes.
    #[serde(default)]
    pub catalog_revision: u32,
}

/// Presenter mode for a data window (spec §4 hybrid view matrix).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ViewMode {
    Appliance,
    Console,
    Topology,
}

/// One llama-server backend (planner | coder | coder-fast | tool-runner).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendConfig {
    pub id: String,
    /// Operator switch: a disabled backend cannot be started or restarted
    /// (the command layer rejects it); stopping is always allowed.
    pub enabled: bool,
    pub model_file: PathBuf,
    pub port: u16,
    /// Structured server flags.
    pub flags: ServerFlags,
    /// Raw extra flags for llama-server, tokenized on whitespace when the
    /// launch command is built (values containing spaces are not
    /// supported). `--host` is reserved and filtered out: backends bind
    /// loopback and the gateway is the only network entry point. None
    /// means no extra flags.
    pub raw_flags: Option<String>,
    pub restart_policy: RestartPolicy,
    pub overrides_global: bool,
}

/// Structured llama-server flags.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerFlags {
    pub n_ctx: u32,
    pub n_batch: u32,
    /// Per-GPU fractions; must sum to 1.0 when non-empty.
    pub tensor_split: Vec<f32>,
    pub split_mode: SplitMode,
}

/// llama.cpp tensor-split placement mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SplitMode {
    None,
    Layer,
    Row,
}

/// Crash-restart policy for a supervised backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestartPolicy {
    pub max_retries: u32,
    pub backoff_base_secs: u64,
}

/// LiteLLM gateway configuration (spec §7).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    /// 4000 by convention.
    pub port: u16,
    /// Address the gateway binds. `0.0.0.0` (all interfaces) by
    /// default — Pete's decision (2026-10-15) so other machines on
    /// the local network can use the gateway; `127.0.0.1` restricts
    /// it to this computer. Authentication is unaffected: the
    /// master key is required on every interface. Must parse as an
    /// IP address (validated).
    #[serde(default = "default_gateway_bind_host")]
    pub bind_host: String,
    pub groups: Vec<ModelGroup>,
    /// Plain-language rule + LiteLLM fragment + source.
    pub routing_rules: Vec<RoutingRule>,
    pub openrouter: CloudTierConfig,
    /// Request ring length (default 100).
    pub request_log_len: usize,
}

/// A LiteLLM model group routing to one or more backends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelGroup {
    pub name: String,
    /// Backend ids (e.g. "planner"), or "host:port" references.
    pub members: Vec<String>,
    pub strategy: RoutingStrategy,
    /// Ordered fallback chain, e.g. ["coder", "openrouter"].
    pub fallbacks: Vec<String>,
}

/// Routing strategy for a model group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RoutingStrategy {
    SimpleShuffle,
    LeastBusy,
    LatencyBased,
}

/// A plain-language routing rule with its LiteLLM fragment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingRule {
    pub id: String,
    pub description: String,
    pub litellm_fragment: serde_json::Value,
    /// Where the rule came from (wizard default | user).
    pub source: String,
}

/// OpenRouter cloud tier: failover, overflow, rubric-based escalation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudTierConfig {
    pub enabled: bool,
    /// Credential Manager target name — never the key itself.
    pub cred_ref: Option<String>,
    pub daily_cap_usd: f64,
    pub model_allowlist: Vec<String>,
}

/// Serde default for [`ModelConfig::enabled`]: configurations written
/// before the flag existed carry models that were always in scope, so
/// an absent flag means enabled.
fn default_model_enabled() -> bool {
    true
}

/// A downloaded GGUF model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub id: String,
    pub gguf_path: PathBuf,
    pub quant: String,
    pub params_b: f32,
    pub sha256: String,
    pub verified_at: Option<DateTime<Utc>>,
    /// Where the file came from (spec §10). Empty for locally-added files.
    #[serde(default)]
    pub source_url: String,
    /// Backend ids this model is assigned to (spec §10).
    #[serde(default)]
    pub assigned_backends: Vec<String>,
    /// Whether the setup wizard's download step includes this model.
    /// The curated defaults are seeded enabled; disabling a model keeps
    /// its file and configuration but excludes it from wizard downloads.
    #[serde(default = "default_model_enabled")]
    pub enabled: bool,
}

/// Telemetry polling configuration (spec §5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryConfig {
    /// 1 s default; 0 = off.
    pub poll_interval_ms: u64,
    /// High-res retention window in seconds.
    pub retention_secs: u64,
}

/// Reference to the MXC sandbox policy (spec §11).
///
/// The policy JSON itself lives outside this config; only the path and the
/// enforcement mode are stored here. Secrets are never in the policy JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MxcPolicyRef {
    pub path: PathBuf,
    pub mode: MxcMode,
}

/// MXC enforcement mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MxcMode {
    Learning,
    Enforced,
    Disabled,
}

/// Windows ML secondary backend registration (spec §6.5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinMlConfig {
    pub enabled: bool,
    /// 8090 by convention.
    pub port: u16,
    pub model_id: Option<String>,
}

/// The app-managed PostgreSQL server's configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostgresConfig {
    /// Loopback port the server binds when the wizard has installed
    /// it. 54329 by default — deliberately not the standard 5432,
    /// which a developer machine may already run its own PostgreSQL
    /// on. The port is claimed in validation whether or not the
    /// server is installed, so no backend can take it.
    pub port: u16,
}

impl Default for PostgresConfig {
    fn default() -> Self {
        Self { port: 54329 }
    }
}

/// Per-component update pins and auto-update toggles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatePolicy {
    pub components: HashMap<String, ComponentUpdate>,
}

/// Update policy for one component (llama.cpp build, LiteLLM, models, ...).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentUpdate {
    pub pinned_version: Option<String>,
    pub auto_update: bool,
    pub last_checked: Option<DateTime<Utc>>,
}

/// Notification preferences.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationConfig {
    pub enabled: bool,
}

/// Errors from config load/save/validate/migrate.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("validation failed: {0}")]
    Validation(String),
    #[error("migration failed: {0}")]
    MigrationFailed(String),
    #[error("backup failed: {0}")]
    BackupFailed(String),
    #[error("diagnostics bundle failed: {0}")]
    Zip(String),
}

impl AppConfig {
    /// Sensible first-run defaults matching the spec's port/strategy conventions.
    ///
    /// The four conventional backends (planner :8081, coder :8082,
    /// coder-fast :8083, tool-runner :8084) start disabled in this raw
    /// default because their model files are not registered models
    /// until the catalog is seeded; the first-run seeding pass aligns
    /// their model files to the seeded models and enables them, so the
    /// persisted configuration the app runs with has the default fleet
    /// enabled. A backend whose model is not downloaded and verified
    /// yet fails its start preflight rather than starting
    /// unconfigured.
    pub fn default_config() -> Self {
        let strategies = [
            (
                "planner",
                RoutingStrategy::LatencyBased,
                vec!["coder", "openrouter"],
            ),
            (
                "coder-fast",
                RoutingStrategy::LeastBusy,
                vec!["coder", "openrouter"],
            ),
            ("coder", RoutingStrategy::SimpleShuffle, vec!["openrouter"]),
            (
                "tool-runner",
                RoutingStrategy::LeastBusy,
                vec!["openrouter"],
            ),
        ];
        let groups = strategies
            .into_iter()
            .map(|(name, strategy, fallbacks)| ModelGroup {
                name: name.to_string(),
                members: vec![name.to_string()],
                strategy,
                fallbacks: fallbacks.into_iter().map(str::to_string).collect(),
            })
            .collect();
        let backends = ["planner", "coder", "coder-fast", "tool-runner"]
            .into_iter()
            .zip([8081u16, 8082, 8083, 8084])
            .map(|(id, port)| BackendConfig {
                id: id.to_string(),
                enabled: false,
                model_file: PathBuf::from(format!("models/{id}.gguf")),
                port,
                flags: ServerFlags {
                    n_ctx: 32768,
                    n_batch: 512,
                    tensor_split: Vec::new(),
                    split_mode: SplitMode::None,
                },
                raw_flags: if id == "coder" {
                    // The coder model (Qwen3-Coder-Next) is served
                    // by expert offload: expert tensors stay in
                    // system RAM, the rest runs on the GPU.
                    Some("-ot exps=CPU".to_string())
                } else {
                    None
                },
                restart_policy: RestartPolicy {
                    max_retries: 5,
                    backoff_base_secs: 2,
                },
                overrides_global: false,
            })
            .collect();
        Self {
            version: CURRENT_VERSION,
            data_dir: default_data_dir(),
            view_modes: HashMap::new(),
            backends,
            gateway: GatewayConfig {
                port: 4000,
                bind_host: default_gateway_bind_host(),
                groups,
                routing_rules: Vec::new(),
                openrouter: CloudTierConfig {
                    enabled: false,
                    cred_ref: None,
                    daily_cap_usd: 5.0,
                    model_allowlist: Vec::new(),
                },
                request_log_len: 100,
            },
            models: Vec::new(),
            telemetry: TelemetryConfig {
                poll_interval_ms: 1000,
                retention_secs: 60,
            },
            mxc: MxcPolicyRef {
                // Same location the wizard's MxcPolicies step installs
                // and validates: <data_dir>/mxc/policy.json.
                path: default_data_dir().join("mxc").join("policy.json"),
                mode: MxcMode::Learning,
            },
            winml: WinMlConfig {
                enabled: false,
                port: 8090,
                model_id: None,
            },
            postgres: PostgresConfig::default(),
            updates: UpdatePolicy {
                components: HashMap::new(),
            },
            notifications: NotificationConfig { enabled: true },
            setup_completed: false,
            catalog_seeded: false,
            defaults_repaired: true,
            catalog_revision: 0,
        }
    }
}

/// One curated catalog entry's repair data for
/// [`repair_catalog_defaults`]: the values a configuration seeded by an
/// older version may be missing or hold as placeholders.
pub struct CatalogModelDefaults {
    pub model_id: String,
    pub sha256: String,
    pub params_b: f32,
    /// The default backend this model belongs to.
    pub backend_id: String,
}

/// One-time repair for configurations written before the catalog
/// carried real values. For each catalog model present in the config:
///
/// - a `sha256` still equal to `placeholder_sha` is replaced with the
///   catalog's real pinned hash (a deliberately emptied or otherwise
///   edited hash is left alone);
/// - a non-positive `params_b` is set from the catalog (validation
///   requires a positive value, and older seeding wrote 0);
/// - the model's default backend is recorded in `assigned_backends`
///   when missing.
///
/// For each catalog backend: a `model_file` still equal to the
/// untouched default (`models/<backend-id>.gguf`) is aligned to its
/// model's registered path, and the backend is enabled (older
/// defaults shipped the fleet disabled; enabling is what the current
/// defaults do — a backend whose model is not downloaded and verified
/// still fails its start preflight). Returns true when anything
/// changed. The caller gates this on [`AppConfig::defaults_repaired`]
/// so it runs exactly once and never overrides later operator choices.
pub fn repair_catalog_defaults(
    cfg: &mut AppConfig,
    catalog: &[CatalogModelDefaults],
    placeholder_sha: &str,
) -> bool {
    let mut changed = false;
    for entry in catalog {
        let Some(model) = cfg.models.iter_mut().find(|m| m.id == entry.model_id) else {
            continue;
        };
        if model.sha256 == placeholder_sha {
            model.sha256 = entry.sha256.clone();
            changed = true;
        }
        if model.params_b <= 0.0 {
            model.params_b = entry.params_b;
            changed = true;
        }
        if !model.assigned_backends.contains(&entry.backend_id) {
            model.assigned_backends.push(entry.backend_id.clone());
            changed = true;
        }
        let gguf_path = model.gguf_path.clone();
        if let Some(backend) = cfg.backends.iter_mut().find(|b| b.id == entry.backend_id) {
            if backend.model_file == format!("models/{}.gguf", entry.backend_id) {
                backend.model_file = gguf_path;
                changed = true;
            }
            if !backend.enabled {
                backend.enabled = true;
                changed = true;
            }
        }
    }
    changed
}

/// One catalog entry for [`append_missing_catalog_models`]: a model
/// a configuration may predate.
#[derive(Debug, Clone)]
pub struct CatalogModelEntry {
    pub model_id: String,
    pub gguf_file_name: String,
    pub quant: String,
    pub params_b: f32,
    pub sha256: String,
    pub source_url: String,
    pub assigned_backends: Vec<String>,
    pub enabled: bool,
}

/// Append catalog models the configuration does not have yet (by
/// id). Used when the curated catalog gains entries after a
/// configuration was seeded: the new models arrive with their
/// catalog defaults (opt-in candidates disabled and unassigned)
/// instead of never reaching installed systems. Existing entries —
/// including operator-edited ones — are untouched. The caller
/// gates this on [`AppConfig::catalog_revision`] so it runs once
/// per catalog revision. Returns true when anything was appended.
pub fn append_missing_catalog_models(cfg: &mut AppConfig, entries: &[CatalogModelEntry]) -> bool {
    let mut changed = false;
    for entry in entries {
        if cfg.models.iter().any(|m| m.id == entry.model_id) {
            continue;
        }
        cfg.models.push(ModelConfig {
            id: entry.model_id.clone(),
            gguf_path: PathBuf::from("models").join(&entry.gguf_file_name),
            quant: entry.quant.clone(),
            params_b: entry.params_b,
            sha256: entry.sha256.clone(),
            verified_at: None,
            source_url: entry.source_url.clone(),
            assigned_backends: entry.assigned_backends.clone(),
            enabled: entry.enabled,
        });
        changed = true;
    }
    changed
}

/// One catalog definition replacement for
/// [`refresh_catalog_definition`]: the catalog's current entry for a
/// default-fleet model, the backend it serves, and the serving
/// change that rides the replacement.
pub struct CatalogDefinitionRefresh {
    pub entry: CatalogModelEntry,
    pub backend_id: String,
    /// Clear the backend's tensor split: the replacement model is
    /// served unsplit (planner) or by expert offload (coder), not
    /// by the split the previous definition needed.
    pub clear_split: bool,
    /// Raw llama-server flags the replacement model requires (for
    /// the coder, expert offload: `-ot exps=CPU`). Applied only
    /// when the backend has no raw flags of its own.
    pub raw_flags: Option<String>,
}

/// Apply a catalog definition replacement to an existing
/// configuration (catalog revision 2 replaced the planner and coder
/// models). When the stored pin differs from the catalog's, the
/// model entry takes the catalog's fields and its verification is
/// cleared — the replacement file has not been downloaded — and a
/// default backend still pointing at the model's previous file is
/// realigned to the new one. The riding serving change applies in
/// the same case: the backend's tensor split is cleared when
/// requested, and the required raw flags are set when the backend
/// has none. A backend the operator reassigned to another model,
/// or gave raw flags, keeps them. The caller gates this on
/// [`AppConfig::catalog_revision`] so it runs once per catalog
/// revision. Returns true when the definition was replaced.
pub fn refresh_catalog_definition(cfg: &mut AppConfig, refresh: &CatalogDefinitionRefresh) -> bool {
    let entry = &refresh.entry;
    let Some(model) = cfg.models.iter_mut().find(|m| m.id == entry.model_id) else {
        return false;
    };
    if model.sha256 == entry.sha256 {
        return false;
    }
    let old_path = model.gguf_path.clone();
    model.gguf_path = PathBuf::from("models").join(&entry.gguf_file_name);
    model.quant = entry.quant.clone();
    model.params_b = entry.params_b;
    model.sha256 = entry.sha256.clone();
    model.source_url = entry.source_url.clone();
    model.verified_at = None;
    let new_path = model.gguf_path.clone();
    if let Some(backend) = cfg.backends.iter_mut().find(|b| b.id == refresh.backend_id) {
        // The serving change applies only to a backend still
        // serving this model; a backend the operator reassigned to
        // another model's file keeps its file, split, and flags.
        let serving = backend.model_file == old_path;
        if serving {
            backend.model_file = new_path;
            if refresh.clear_split
                && (!backend.flags.tensor_split.is_empty()
                    || backend.flags.split_mode != SplitMode::None)
            {
                backend.flags.tensor_split = Vec::new();
                backend.flags.split_mode = SplitMode::None;
            }
            if let Some(raw) = &refresh.raw_flags {
                if backend.raw_flags.is_none() {
                    backend.raw_flags = Some(raw.clone());
                }
            }
        }
    }
    true
}

/// Load, migrate, and validate the config at `path`.
///
/// If the primary file is missing, unreadable, unparsable, or fails
/// validation, the `.bak` last-known-good copy is tried. If that also fails,
/// the primary error is returned. A missing file is an I/O error —
/// callers fall back to [`AppConfig::default_config`] on first run.
///
/// Use [`load_detailed`] when the caller needs to know whether the `.bak`
/// fallback was used (spec §10 requires a UI banner in that case).
pub fn load(path: &PathBuf) -> Result<AppConfig, ConfigError> {
    load_detailed(path).map(|r| r.config)
}

/// Result of [`load_detailed`].
#[derive(Debug)]
pub struct LoadReport {
    pub config: AppConfig,
    /// True when the primary file failed and the `.bak` copy was used.
    /// Spec §10: the UI must banner this condition.
    pub used_backup: bool,
}

/// Like [`load`], but also reports whether the `.bak` fallback was used.
pub fn load_detailed(path: &PathBuf) -> Result<LoadReport, ConfigError> {
    match load_one(path) {
        Ok(config) => Ok(LoadReport {
            config,
            used_backup: false,
        }),
        Err(primary_err) => {
            let bak = bak_path(path);
            match load_one(&bak) {
                Ok(config) => Ok(LoadReport {
                    config,
                    used_backup: true,
                }),
                Err(_) => Err(primary_err),
            }
        }
    }
}

/// Validate and save the config.
///
/// The existing file (if any) is rotated to `.bak` first; the new file is
/// written atomically (temp file + rename). The config is validated *before*
/// anything is written, so a failed save never clobbers the last-known-good
/// copy.
pub fn save(config: &AppConfig, path: &PathBuf) -> Result<(), ConfigError> {
    validate(config)?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let json = serde_json::to_string_pretty(config)?;
    if path.exists() {
        let bak = bak_path(path);
        replace_file(path, &bak).map_err(|e| {
            ConfigError::BackupFailed(format!(
                "could not rotate {} to {}: {e}",
                path.display(),
                bak.display()
            ))
        })?;
    }
    let tmp = tmp_path(path);
    fs::write(&tmp, json)?;
    replace_file(&tmp, path)?;
    Ok(())
}

/// Structural validation. Collects every problem found and reports them
/// together; returns `Ok(())` only when the config is fully consistent.
///
/// Rules: backend ids unique and non-empty; ports (gateway, backends, WinML
/// when enabled) unique and non-zero; `tensor_split` empty (single GPU) or
/// elements in [0, 1] summing to 1.0, consistent with `split_mode`; enabled
/// backends reference a registered model; model ids unique, `sha256` empty
/// or 64 hex chars; group members are backend ids or `host:port`; fallbacks
/// are backend ids or `"openrouter"`; group/rule names unique; numeric
/// ranges sane.
pub fn validate(config: &AppConfig) -> Result<(), ConfigError> {
    let mut errors: Vec<String> = Vec::new();

    if config.version != CURRENT_VERSION {
        errors.push(format!(
            "config version {} != supported version {CURRENT_VERSION}; load it through migrate() first",
            config.version
        ));
    }

    // --- backend ids and flags ---
    let mut backend_ids: HashSet<String> = HashSet::new();
    for b in &config.backends {
        if b.id.trim().is_empty() {
            errors.push("backend with empty id".to_string());
        } else if !backend_ids.insert(b.id.clone()) {
            errors.push(format!("duplicate backend id {:?}", b.id));
        }
        // Group member references use "host:port"; a ':' in a backend id
        // would make the member parser misclassify it.
        if b.id.contains(':') {
            errors.push(format!("backend id {:?} must not contain ':'", b.id));
        }
        if b.flags.n_ctx == 0 {
            errors.push(format!("backend {:?}: n_ctx must be > 0", b.id));
        }
        if b.flags.n_batch == 0 {
            errors.push(format!("backend {:?}: n_batch must be > 0", b.id));
        }
        validate_split(&b.id, &b.flags, &mut errors);
        if b.restart_policy.backoff_base_secs == 0 {
            errors.push(format!("backend {:?}: backoff_base_secs must be > 0", b.id));
        }
    }

    if config
        .gateway
        .bind_host
        .parse::<std::net::IpAddr>()
        .is_err()
    {
        errors.push(format!(
            "gateway bind_host {:?} is not an IP address",
            config.gateway.bind_host
        ));
    }

    // --- ports: gateway, backends, WinML (when enabled) ---
    let mut ports: HashMap<u16, String> = HashMap::new();
    let mut claim_port = |port: u16, owner: String| {
        if port == 0 {
            errors.push(format!("{owner}: port 0 is invalid"));
        } else if let Some(prev) = ports.insert(port, owner.clone()) {
            errors.push(format!("port {port} claimed by both {prev} and {owner}"));
        }
    };
    claim_port(config.gateway.port, "gateway".to_string());
    for b in &config.backends {
        claim_port(b.port, format!("backend {:?}", b.id));
    }
    if config.winml.enabled {
        claim_port(config.winml.port, "winml".to_string());
    }
    claim_port(config.postgres.port, "postgres".to_string());

    // --- model references (enabled backends must point at a registered model) ---
    let known_models: HashSet<String> = config
        .models
        .iter()
        .map(|m| normalize_path(&m.gguf_path))
        .collect();
    for b in &config.backends {
        if b.enabled && !known_models.contains(&normalize_path(&b.model_file)) {
            errors.push(format!(
                "backend {:?}: model_file {:?} is not a registered model",
                b.id, b.model_file
            ));
        }
    }

    // --- models ---
    let mut model_ids: HashSet<String> = HashSet::new();
    for m in &config.models {
        if m.id.trim().is_empty() {
            errors.push("model with empty id".to_string());
        } else if !model_ids.insert(m.id.clone()) {
            errors.push(format!("duplicate model id {:?}", m.id));
        }
        if m.gguf_path.as_os_str().is_empty() {
            errors.push(format!("model {:?}: gguf_path must not be empty", m.id));
        }
        if m.params_b.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
            errors.push(format!("model {:?}: params_b must be > 0", m.id));
        }
        if !m.sha256.is_empty() && !is_hex64(&m.sha256) {
            errors.push(format!(
                "model {:?}: sha256 must be empty or 64 hex characters",
                m.id
            ));
        }
    }

    // --- gateway ---
    if config.gateway.request_log_len == 0 || config.gateway.request_log_len > MAX_REQUEST_LOG_LEN {
        errors.push(format!(
            "gateway.request_log_len {} out of range 1..={}",
            config.gateway.request_log_len, MAX_REQUEST_LOG_LEN
        ));
    }
    let oc = &config.gateway.openrouter;
    if !oc.daily_cap_usd.is_finite() || oc.daily_cap_usd < 0.0 {
        errors.push("gateway.openrouter.daily_cap_usd must be a finite value >= 0".to_string());
    }
    let mut group_names: HashSet<String> = HashSet::new();
    for g in &config.gateway.groups {
        if g.name.trim().is_empty() {
            errors.push("model group with empty name".to_string());
        } else if !group_names.insert(g.name.clone()) {
            errors.push(format!("duplicate model group {:?}", g.name));
        }
        for member in &g.members {
            match member.rsplit_once(':') {
                // "host:port" reference — backend ids never contain ':'.
                Some((host, port_str)) => {
                    let port_ok = !host.is_empty()
                        && port_str.parse::<u16>().map(|p| p != 0).unwrap_or(false);
                    if !port_ok {
                        errors.push(format!(
                            "group {:?}: invalid member reference {member:?}",
                            g.name
                        ));
                    }
                }
                None => {
                    if !backend_ids.contains(member) {
                        errors.push(format!(
                            "group {:?}: unknown backend member {member:?}",
                            g.name
                        ));
                    }
                }
            }
        }
        for fb in &g.fallbacks {
            if fb != "openrouter" && !backend_ids.contains(fb) {
                errors.push(format!("group {:?}: unknown fallback {fb:?}", g.name));
            }
        }
    }
    let mut rule_ids: HashSet<String> = HashSet::new();
    for r in &config.gateway.routing_rules {
        if !rule_ids.insert(r.id.clone()) {
            errors.push(format!("duplicate routing rule id {:?}", r.id));
        }
    }

    // --- telemetry ---
    if config.telemetry.retention_secs == 0 {
        errors.push("telemetry.retention_secs must be > 0".to_string());
    }

    // --- mxc ---
    if config.mxc.path.as_os_str().is_empty() {
        errors.push("mxc.path must not be empty".to_string());
    }

    // --- winml ---
    if config.winml.enabled {
        match &config.winml.model_id {
            Some(id) if !id.trim().is_empty() => {}
            _ => errors.push("winml is enabled but model_id is not set".to_string()),
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(ConfigError::Validation(errors.join("; ")))
    }
}

/// Migrate a raw JSON config value forward to [`CURRENT_VERSION`].
///
/// Version detection: the `version` key, defaulting to 0 when absent (v0 =
/// pre-versioned configs). v0 → v1 overlays the stored values onto current
/// defaults so newly added fields get sane values, then stamps `version: 1`.
/// Unknown fields are ignored. A version newer than [`CURRENT_VERSION`] or a
/// non-object root is a [`ConfigError::MigrationFailed`].
pub fn migrate(raw: serde_json::Value) -> Result<AppConfig, ConfigError> {
    if !raw.is_object() {
        return Err(ConfigError::MigrationFailed(
            "config root must be a JSON object".to_string(),
        ));
    }
    let version = raw.get("version").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    if version > CURRENT_VERSION {
        return Err(ConfigError::MigrationFailed(format!(
            "config version {version} is newer than supported version {CURRENT_VERSION}"
        )));
    }
    let mut merged = raw;
    apply_defaults(&mut merged)?;
    if version != CURRENT_VERSION {
        if let Some(obj) = merged.as_object_mut() {
            obj.insert("version".to_string(), serde_json::json!(CURRENT_VERSION));
        }
    }
    serde_json::from_value(merged).map_err(|e| {
        ConfigError::MigrationFailed(format!("migrated config failed schema check: {e}"))
    })
}

/// Default data directory: `%APPDATA%\local-llm-service-manager`
/// (falls back to `~/.local-llm-service-manager` off-Windows).
/// Serde default for [`GatewayConfig::bind_host`]: all interfaces.
pub fn default_gateway_bind_host() -> String {
    "0.0.0.0".to_string()
}

pub fn default_data_dir() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        PathBuf::from(appdata).join("local-llm-service-manager")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local-llm-service-manager")
    } else {
        PathBuf::from(".local-llm-service-manager")
    }
}

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------

fn load_one(path: &PathBuf) -> Result<AppConfig, ConfigError> {
    let data = fs::read(path)?;
    let raw: serde_json::Value = serde_json::from_slice(&data)?;
    let cfg = migrate(raw)?;
    validate(&cfg).map_err(|e| match e {
        ConfigError::Validation(msg) => {
            ConfigError::Validation(format!("{}: {msg}", path.display()))
        }
        other => other,
    })?;
    Ok(cfg)
}

/// Deep-merge `raw` over the current defaults: stored values win, missing
/// keys are filled from [`AppConfig::default_config`].
fn apply_defaults(raw: &mut serde_json::Value) -> Result<(), ConfigError> {
    let defaults = serde_json::to_value(AppConfig::default_config()).map_err(|e| {
        ConfigError::MigrationFailed(format!("default config failed to serialize: {e}"))
    })?;
    let merged = deep_merge(defaults, raw.take());
    *raw = merged;
    Ok(())
}

fn deep_merge(base: serde_json::Value, over: serde_json::Value) -> serde_json::Value {
    match (base, over) {
        (serde_json::Value::Object(mut b), serde_json::Value::Object(o)) => {
            for (k, v) in o {
                let merged = match b.remove(&k) {
                    Some(bv) => deep_merge(bv, v),
                    None => v,
                };
                b.insert(k, merged);
            }
            serde_json::Value::Object(b)
        }
        // Arrays and scalars: the stored value wins wholesale.
        (_, over) => over,
    }
}

fn validate_split(backend_id: &str, flags: &ServerFlags, errors: &mut Vec<String>) {
    let split = &flags.tensor_split;
    if split.is_empty() {
        if flags.split_mode != SplitMode::None {
            errors.push(format!(
                "backend {backend_id:?}: split_mode is {:?} but tensor_split is empty",
                flags.split_mode
            ));
        }
        return;
    }
    if flags.split_mode == SplitMode::None {
        errors.push(format!(
            "backend {backend_id:?}: tensor_split is set but split_mode is none"
        ));
    }
    for (i, v) in split.iter().enumerate() {
        if !v.is_finite() || *v < 0.0 || *v > 1.0 {
            errors.push(format!(
                "backend {backend_id:?}: tensor_split[{i}] = {v} out of range [0, 1]"
            ));
        }
    }
    let sum: f32 = split.iter().sum();
    if (sum - 1.0).abs() > SPLIT_SUM_EPSILON {
        errors.push(format!(
            "backend {backend_id:?}: tensor_split sums to {sum}, expected 1.0"
        ));
    }
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Separator- and case-insensitive path comparison for model registry
/// lookups (Windows paths are case-insensitive).
fn normalize_path(p: &std::path::Path) -> String {
    p.to_string_lossy().replace('\\', "/").to_lowercase()
}

fn bak_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".bak");
    PathBuf::from(s)
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

/// Move `src` onto `dst`, replacing `dst` (portable across Windows/POSIX —
/// `fs::rename` alone does not replace on Windows).
fn replace_file(src: &PathBuf, dst: &PathBuf) -> std::io::Result<()> {
    if dst.exists() {
        fs::remove_file(dst)?;
    }
    fs::rename(src, dst)
}

#[cfg(test)]
mod tests {
    #[test]
    fn append_missing_catalog_models_appends_only_missing() {
        let mut cfg = AppConfig::default_config();
        cfg.models.push(ModelConfig {
            id: "existing".to_string(),
            gguf_path: PathBuf::from("models/existing.gguf"),
            quant: "Q4_K_M".to_string(),
            params_b: 8.0,
            sha256: String::new(),
            verified_at: None,
            source_url: String::new(),
            assigned_backends: Vec::new(),
            enabled: true,
        });
        let entry = |id: &str, enabled: bool| CatalogModelEntry {
            model_id: id.to_string(),
            gguf_file_name: format!("{id}.gguf"),
            quant: "Q4_K_M".to_string(),
            params_b: 31.6,
            sha256: "a".repeat(64),
            source_url: "https://example.com/x.gguf".to_string(),
            assigned_backends: Vec::new(),
            enabled,
        };
        // "existing" is skipped; "new-opt-in" is appended disabled.
        let changed = append_missing_catalog_models(
            &mut cfg,
            &[entry("existing", true), entry("new-opt-in", false)],
        );
        assert!(changed);
        assert_eq!(cfg.models.len(), 2);
        let added = cfg.models.iter().find(|m| m.id == "new-opt-in").unwrap();
        assert!(!added.enabled);
        assert_eq!(added.gguf_path, PathBuf::from("models/new-opt-in.gguf"));
        assert_eq!(added.params_b, 31.6);
        // A second run appends nothing.
        assert!(!append_missing_catalog_models(
            &mut cfg,
            &[entry("new-opt-in", false)]
        ));
    }

    #[test]
    fn refresh_catalog_definition_replaces_pin_and_serving() {
        let mut cfg = AppConfig::default_config();
        // Stored state as seeded under the old definitions: the
        // planner model carries the old pin and is verified; the
        // planner backend points at the old file and carries a
        // tensor split from the old model's serving mode.
        cfg.models.push(ModelConfig {
            id: "qwen3-27b-planner".to_string(),
            gguf_path: PathBuf::from("models/OldPlanner.gguf"),
            quant: "Q4_K_M".to_string(),
            params_b: 32.0,
            sha256: "b".repeat(64),
            verified_at: Some(Utc::now()),
            source_url: "https://example.com/OldPlanner.gguf".to_string(),
            assigned_backends: vec!["planner".to_string()],
            enabled: true,
        });
        let planner = cfg.backends.iter_mut().find(|b| b.id == "planner").unwrap();
        planner.model_file = PathBuf::from("models/OldPlanner.gguf");
        planner.flags.tensor_split = vec![0.5, 0.5];
        planner.flags.split_mode = SplitMode::Layer;
        let refresh = CatalogDefinitionRefresh {
            entry: CatalogModelEntry {
                model_id: "qwen3-27b-planner".to_string(),
                gguf_file_name: "NewPlanner.gguf".to_string(),
                quant: "IQ3_S".to_string(),
                params_b: 27.0,
                sha256: "c".repeat(64),
                source_url: "https://example.com/NewPlanner.gguf".to_string(),
                assigned_backends: vec!["planner".to_string()],
                enabled: true,
            },
            backend_id: "planner".to_string(),
            clear_split: true,
            raw_flags: None,
        };
        assert!(refresh_catalog_definition(&mut cfg, &refresh));
        let model = cfg
            .models
            .iter()
            .find(|m| m.id == "qwen3-27b-planner")
            .unwrap();
        assert_eq!(model.gguf_path, PathBuf::from("models/NewPlanner.gguf"));
        assert_eq!(model.sha256, "c".repeat(64));
        assert_eq!(model.quant, "IQ3_S");
        assert!(model.verified_at.is_none());
        let planner = cfg.backends.iter().find(|b| b.id == "planner").unwrap();
        assert_eq!(planner.model_file, PathBuf::from("models/NewPlanner.gguf"));
        assert!(planner.flags.tensor_split.is_empty());
        assert_eq!(planner.flags.split_mode, SplitMode::None);
        // Same pin again: no-op.
        assert!(!refresh_catalog_definition(&mut cfg, &refresh));
    }

    #[test]
    fn refresh_catalog_definition_preserves_operator_changes() {
        let mut cfg = AppConfig::default_config();
        cfg.models.push(ModelConfig {
            id: "qwen3-coder-30b".to_string(),
            gguf_path: PathBuf::from("models/OldCoder.gguf"),
            quant: "Q4_K_M".to_string(),
            params_b: 30.0,
            sha256: "b".repeat(64),
            verified_at: Some(Utc::now()),
            source_url: "https://example.com/OldCoder.gguf".to_string(),
            assigned_backends: vec!["coder".to_string()],
            enabled: true,
        });
        // The operator moved the coder backend to another model's
        // file and set their own raw flags.
        let coder = cfg.backends.iter_mut().find(|b| b.id == "coder").unwrap();
        coder.model_file = PathBuf::from("models/SomethingElse.gguf");
        coder.raw_flags = Some("--threads 8".to_string());
        let refresh = CatalogDefinitionRefresh {
            entry: CatalogModelEntry {
                model_id: "qwen3-coder-30b".to_string(),
                gguf_file_name: "NewCoder.gguf".to_string(),
                quant: "UD-Q4_K_M".to_string(),
                params_b: 80.0,
                sha256: "c".repeat(64),
                source_url: "https://example.com/NewCoder.gguf".to_string(),
                assigned_backends: vec!["coder".to_string()],
                enabled: true,
            },
            backend_id: "coder".to_string(),
            clear_split: true,
            raw_flags: Some("-ot exps=CPU".to_string()),
        };
        assert!(refresh_catalog_definition(&mut cfg, &refresh));
        let coder = cfg.backends.iter().find(|b| b.id == "coder").unwrap();
        // Not realigned and no serving change (operator's assignment
        // stands): the file and the operator's raw flags are kept.
        assert_eq!(coder.model_file, PathBuf::from("models/SomethingElse.gguf"));
        assert_eq!(coder.raw_flags.as_deref(), Some("--threads 8"));
        // A model the configuration does not have is a no-op.
        let missing = CatalogDefinitionRefresh {
            entry: CatalogModelEntry {
                model_id: "not-present".to_string(),
                gguf_file_name: "X.gguf".to_string(),
                quant: "Q4_K_M".to_string(),
                params_b: 1.0,
                sha256: "d".repeat(64),
                source_url: String::new(),
                assigned_backends: Vec::new(),
                enabled: true,
            },
            backend_id: "coder".to_string(),
            clear_split: false,
            raw_flags: None,
        };
        assert!(!refresh_catalog_definition(&mut cfg, &missing));
    }

    use super::*;

    fn unique_tmpdir(name: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!(
            "manager-config-unit-{}-{}",
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Config with one registered model and one enabled backend using it.
    fn working_config() -> AppConfig {
        let mut cfg = AppConfig::default_config();
        cfg.models.push(ModelConfig {
            id: "qwen3-8b".to_string(),
            gguf_path: PathBuf::from("models/qwen3-8b.gguf"),
            quant: "Q4_K_M".to_string(),
            params_b: 8.0,
            sha256: "ab".repeat(32),
            verified_at: None,
            source_url: "https://example.com/qwen3-8b.gguf".to_string(),
            assigned_backends: vec!["tool-runner".to_string()],
            enabled: true,
        });
        let backend = cfg
            .backends
            .iter_mut()
            .find(|b| b.id == "tool-runner")
            .unwrap();
        backend.enabled = true;
        backend.model_file = PathBuf::from("models/qwen3-8b.gguf");
        backend.flags.tensor_split = vec![0.6, 0.4];
        backend.flags.split_mode = SplitMode::Layer;
        cfg
    }

    #[test]
    fn default_config_round_trips() {
        let cfg = AppConfig::default_config();
        assert_eq!(cfg.version, CURRENT_VERSION);
        assert_eq!(cfg.gateway.port, 4000);
        assert_eq!(cfg.gateway.groups.len(), 4);
        assert!(!cfg.setup_completed, "fresh installs must run the wizard");
        let json = serde_json::to_string(&cfg).expect("serialize");
        let back: AppConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.version, CURRENT_VERSION);
        assert!(!back.setup_completed);
        assert_eq!(
            back.gateway.groups[0].strategy,
            RoutingStrategy::LatencyBased
        );
    }

    #[test]
    fn setup_completed_defaults_false_for_old_configs() {
        // Configs written before the flag existed must still parse.
        let cfg = AppConfig::default_config();
        let mut v = serde_json::to_value(&cfg).expect("serialize");
        v.as_object_mut().expect("object").remove("setup_completed");
        let back: AppConfig = serde_json::from_value(v).expect("deserialize");
        assert!(!back.setup_completed);
    }

    #[test]
    fn default_config_is_valid() {
        validate(&AppConfig::default_config()).expect("defaults must validate");
    }

    #[test]
    fn working_config_is_valid() {
        validate(&working_config()).expect("working config must validate");
    }

    #[test]
    fn gateway_bind_host_defaults_to_all_interfaces() {
        // Pete's 2026-10-15 decision: the gateway is LAN-reachable out
        // of the box; loopback-only is the opt-in.
        assert_eq!(AppConfig::default_config().gateway.bind_host, "0.0.0.0");
        // A config file written before bind_host existed deserializes
        // to the same default.
        let cfg: AppConfig = serde_json::from_str(
            &serde_json::to_string(&AppConfig::default_config())
                .unwrap()
                .replace(r#""bind_host":"0.0.0.0","#, ""),
        )
        .unwrap();
        assert_eq!(cfg.gateway.bind_host, "0.0.0.0");
    }

    #[test]
    fn gateway_bind_host_must_be_an_ip() {
        let mut cfg = working_config();
        cfg.gateway.bind_host = "localhost".to_string();
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("bind_host"), "unexpected: {err}");
        cfg.gateway.bind_host = "127.0.0.1".to_string();
        validate(&cfg).expect("loopback bind validates");
    }

    #[test]
    fn duplicate_backend_ports_rejected() {
        let mut cfg = working_config();
        cfg.backends[1].port = cfg.backends[0].port;
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("claimed by both"), "unexpected: {err}");
    }

    #[test]
    fn gateway_port_collision_rejected() {
        let mut cfg = working_config();
        cfg.backends[0].port = cfg.gateway.port;
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("claimed by both"), "unexpected: {err}");
    }

    #[test]
    fn zero_port_rejected() {
        let mut cfg = working_config();
        cfg.backends[0].port = 0;
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("port 0 is invalid"), "unexpected: {err}");
    }

    #[test]
    fn winml_port_collision_rejected_when_enabled() {
        let mut cfg = working_config();
        cfg.winml.enabled = true;
        cfg.winml.model_id = Some("qwen3-8b".to_string());
        cfg.winml.port = cfg.gateway.port;
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("claimed by both"), "unexpected: {err}");
    }

    #[test]
    fn winml_port_not_claimed_when_disabled() {
        let mut cfg = working_config();
        // 8090 free for a backend while winml is disabled.
        cfg.backends[0].port = 8090;
        validate(&cfg).expect("disabled winml must not claim its port");
    }

    #[test]
    fn tensor_split_must_sum_to_one() {
        let mut cfg = working_config();
        let b = cfg
            .backends
            .iter_mut()
            .find(|b| b.id == "tool-runner")
            .unwrap();
        b.flags.tensor_split = vec![0.5, 0.4];
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("sums to 0.9"), "unexpected: {err}");
    }

    #[test]
    fn tensor_split_empty_is_ok() {
        let cfg = working_config();
        let b = cfg.backends.iter().find(|b| b.id == "planner").unwrap();
        assert!(b.flags.tensor_split.is_empty());
        validate(&cfg).expect("empty split must validate");
    }

    #[test]
    fn tensor_split_rejects_negative_and_over_one() {
        let mut cfg = working_config();
        let b = cfg
            .backends
            .iter_mut()
            .find(|b| b.id == "tool-runner")
            .unwrap();
        b.flags.tensor_split = vec![-0.2, 1.2];
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("out of range [0, 1]"), "unexpected: {err}");
    }

    #[test]
    fn split_mode_mismatch_rejected() {
        // Mode set but no split values.
        let mut cfg = working_config();
        let b = cfg.backends.iter_mut().find(|b| b.id == "planner").unwrap();
        b.flags.split_mode = SplitMode::Layer;
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("tensor_split is empty"), "unexpected: {err}");

        // Split values but mode none.
        let mut cfg = working_config();
        let b = cfg
            .backends
            .iter_mut()
            .find(|b| b.id == "tool-runner")
            .unwrap();
        b.flags.split_mode = SplitMode::None;
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("split_mode is none"), "unexpected: {err}");
    }

    #[test]
    fn enabled_backend_unknown_model_rejected() {
        let mut cfg = working_config();
        let b = cfg
            .backends
            .iter_mut()
            .find(|b| b.id == "tool-runner")
            .unwrap();
        b.model_file = PathBuf::from("models/does-not-exist.gguf");
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("not a registered model"), "unexpected: {err}");
    }

    #[test]
    fn disabled_backend_unknown_model_ok() {
        let cfg = working_config();
        // planner is disabled with a placeholder model path.
        let b = cfg.backends.iter().find(|b| b.id == "planner").unwrap();
        assert!(!b.enabled);
        validate(&cfg).expect("disabled backend with placeholder model must validate");
    }

    #[test]
    fn duplicate_backend_ids_rejected() {
        let mut cfg = working_config();
        let mut dup = cfg.backends[0].clone();
        dup.port = 18081;
        cfg.backends.push(dup);
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("duplicate backend id"), "unexpected: {err}");
    }

    #[test]
    fn model_sha256_rules() {
        // Malformed hash rejected.
        let mut cfg = working_config();
        cfg.models[0].sha256 = "not-a-hash".to_string();
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("sha256"), "unexpected: {err}");

        // Empty hash (unverified model) is fine.
        let mut cfg = working_config();
        cfg.models[0].sha256 = String::new();
        validate(&cfg).expect("empty sha256 must validate");
    }

    #[test]
    fn repair_catalog_defaults_heals_stale_seed() {
        // A config as an older version wrote it: placeholder hash,
        // params_b 0, no backend assignment, backend disabled with the
        // untouched default model_file (the state that made every
        // backend enable fail validation).
        let mut cfg = AppConfig::default_config();
        cfg.models.push(ModelConfig {
            id: "qwen3-8b".to_string(),
            gguf_path: PathBuf::from("models/Qwen3-8B-Q4_K_M.gguf"),
            quant: "Q4_K_M".to_string(),
            params_b: 0.0,
            sha256: "PLACEHOLDER-pin".to_string(),
            verified_at: None,
            source_url: "https://example.com/q.gguf".to_string(),
            assigned_backends: Vec::new(),
            enabled: true,
        });
        let catalog = vec![CatalogModelDefaults {
            model_id: "qwen3-8b".to_string(),
            sha256: "a".repeat(64),
            params_b: 8.0,
            backend_id: "tool-runner".to_string(),
        }];
        assert!(repair_catalog_defaults(
            &mut cfg,
            &catalog,
            "PLACEHOLDER-pin"
        ));
        let m = &cfg.models[0];
        assert_eq!(m.sha256, "a".repeat(64));
        assert_eq!(m.params_b, 8.0);
        assert_eq!(m.assigned_backends, vec!["tool-runner".to_string()]);
        let b = cfg.backends.iter().find(|b| b.id == "tool-runner").unwrap();
        assert!(b.enabled);
        assert_eq!(b.model_file, PathBuf::from("models/Qwen3-8B-Q4_K_M.gguf"));
        validate(&cfg).expect("repaired config validates");
        // A second pass changes nothing.
        assert!(!repair_catalog_defaults(
            &mut cfg,
            &catalog,
            "PLACEHOLDER-pin"
        ));
    }

    #[test]
    fn repair_preserves_custom_hash_and_model_file() {
        let mut cfg = AppConfig::default_config();
        cfg.models.push(ModelConfig {
            id: "qwen3-8b".to_string(),
            gguf_path: PathBuf::from("models/custom.gguf"),
            quant: "Q8_0".to_string(),
            params_b: 8.0,
            sha256: String::new(), // deliberately unpinned
            verified_at: None,
            source_url: "https://example.com/q.gguf".to_string(),
            assigned_backends: vec!["tool-runner".to_string()],
            enabled: true,
        });
        // The operator pointed the backend at a custom file; neither
        // the file nor the model's hash/params may be rewritten. (The
        // one-time pass does enable the default backend — that is its
        // documented purpose.)
        cfg.backends
            .iter_mut()
            .find(|b| b.id == "tool-runner")
            .unwrap()
            .model_file = PathBuf::from("models/custom.gguf");
        let catalog = vec![CatalogModelDefaults {
            model_id: "qwen3-8b".to_string(),
            sha256: "a".repeat(64),
            params_b: 8.0,
            backend_id: "tool-runner".to_string(),
        }];
        repair_catalog_defaults(&mut cfg, &catalog, "PLACEHOLDER-pin");
        assert_eq!(cfg.models[0].sha256, "");
        assert_eq!(cfg.models[0].params_b, 8.0);
        let b = cfg.backends.iter().find(|b| b.id == "tool-runner").unwrap();
        assert_eq!(b.model_file, PathBuf::from("models/custom.gguf"));
    }

    #[test]
    fn duplicate_model_ids_rejected() {
        let mut cfg = working_config();
        let dup = cfg.models[0].clone();
        cfg.models.push(dup);
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("duplicate model id"), "unexpected: {err}");
    }

    #[test]
    fn zero_n_ctx_rejected() {
        let mut cfg = working_config();
        cfg.backends[0].flags.n_ctx = 0;
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("n_ctx"), "unexpected: {err}");
    }

    #[test]
    fn unknown_group_member_rejected() {
        let mut cfg = working_config();
        cfg.gateway.groups[0].members = vec!["nope".to_string()];
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("unknown backend member"), "unexpected: {err}");
    }

    #[test]
    fn host_port_member_accepted() {
        let mut cfg = working_config();
        cfg.gateway.groups[0].members = vec!["127.0.0.1:18081".to_string()];
        validate(&cfg).expect("host:port member must validate");
    }

    #[test]
    fn malformed_host_port_member_rejected() {
        let mut cfg = working_config();
        cfg.gateway.groups[0].members = vec!["127.0.0.1:notaport".to_string()];
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(
            err.contains("invalid member reference"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn unknown_fallback_rejected_but_openrouter_ok() {
        let mut cfg = working_config();
        cfg.gateway.groups[0].fallbacks = vec!["nope".to_string()];
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("unknown fallback"), "unexpected: {err}");

        let mut cfg = working_config();
        cfg.gateway.groups[0].fallbacks = vec!["openrouter".to_string()];
        validate(&cfg).expect("openrouter fallback must validate");
    }

    #[test]
    fn duplicate_group_names_rejected() {
        let mut cfg = working_config();
        let dup = cfg.gateway.groups[0].clone();
        cfg.gateway.groups.push(dup);
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("duplicate model group"), "unexpected: {err}");
    }

    #[test]
    fn negative_daily_cap_rejected() {
        let mut cfg = working_config();
        cfg.gateway.openrouter.daily_cap_usd = -1.0;
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("daily_cap_usd"), "unexpected: {err}");
    }

    #[test]
    fn winml_enabled_without_model_id_rejected() {
        let mut cfg = working_config();
        cfg.winml.enabled = true;
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("model_id is not set"), "unexpected: {err}");
    }

    #[test]
    fn request_log_len_bounds() {
        let mut cfg = working_config();
        cfg.gateway.request_log_len = 0;
        assert!(validate(&cfg).is_err());
        let mut cfg = working_config();
        cfg.gateway.request_log_len = MAX_REQUEST_LOG_LEN + 1;
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn retention_secs_zero_rejected() {
        let mut cfg = working_config();
        cfg.telemetry.retention_secs = 0;
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("retention_secs"), "unexpected: {err}");
    }

    #[test]
    fn migrate_v0_without_version_key() {
        // v0: no "version" key, partial fields.
        let raw = serde_json::json!({
            "data_dir": "/tmp/llm-test",
            "gateway": { "port": 4000 },
            "telemetry": { "poll_interval_ms": 500 }
        });
        let cfg = migrate(raw).expect("v0 must migrate");
        assert_eq!(cfg.version, CURRENT_VERSION);
        assert_eq!(cfg.gateway.port, 4000);
        // New-in-v1 fields filled from defaults.
        assert_eq!(cfg.gateway.request_log_len, 100);
        assert_eq!(cfg.telemetry.poll_interval_ms, 500);
        assert_eq!(cfg.telemetry.retention_secs, 60);
        assert_eq!(cfg.gateway.groups.len(), 4);
        validate(&cfg).expect("migrated config must validate");
    }

    #[test]
    fn migrate_current_version_passthrough() {
        let raw = serde_json::to_value(working_config()).unwrap();
        let cfg = migrate(raw).expect("v1 must pass through");
        assert_eq!(cfg.version, CURRENT_VERSION);
        assert_eq!(cfg.backends.len(), 4);
    }

    #[test]
    fn migrate_future_version_rejected() {
        let raw = serde_json::json!({ "version": CURRENT_VERSION + 1 });
        let err = migrate(raw).unwrap_err().to_string();
        assert!(err.contains("newer than supported"), "unexpected: {err}");
    }

    #[test]
    fn migrate_non_object_rejected() {
        let err = migrate(serde_json::json!([1, 2, 3])).unwrap_err();
        assert!(matches!(err, ConfigError::MigrationFailed(_)));
    }

    #[test]
    fn migrate_ignores_unknown_fields() {
        let mut raw = serde_json::to_value(working_config()).unwrap();
        raw["some_future_field"] = serde_json::json!({ "nested": true });
        raw["backends"][0]["another_future_field"] = serde_json::json!(42);
        let cfg = migrate(raw).expect("unknown fields must be ignored");
        assert_eq!(cfg.version, CURRENT_VERSION);
        validate(&cfg).expect("must still validate");
    }

    #[test]
    fn save_validates_before_writing() {
        let dir = unique_tmpdir("save-invalid");
        let path = dir.join("config.json");
        let mut cfg = working_config();
        cfg.backends[0].port = cfg.backends[1].port; // invalid
        let err = save(&cfg, &path).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
        assert!(!path.exists(), "invalid config must not be written");
    }

    #[test]
    fn load_missing_file_is_io_error() {
        let dir = unique_tmpdir("load-missing");
        let err = load(&dir.join("nope.json")).unwrap_err();
        assert!(matches!(err, ConfigError::Io(_)), "unexpected: {err}");
    }

    #[test]
    fn backend_id_with_colon_rejected() {
        // Group member references use "host:port"; a backend id containing
        // ':' would be misclassified by the member parser.
        let mut cfg = working_config();
        cfg.backends[0].id = "weird:name".to_string();
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("must not contain ':'"), "unexpected: {err}");
    }

    #[test]
    fn version_mismatch_rejected() {
        let mut cfg = working_config();
        cfg.version = CURRENT_VERSION + 1;
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("config version"), "unexpected: {err}");
    }

    #[test]
    fn model_path_match_is_case_and_separator_insensitive() {
        // Windows paths are case-insensitive; backslashes equal slashes.
        let mut cfg = working_config();
        let b = cfg
            .backends
            .iter_mut()
            .find(|b| b.id == "tool-runner")
            .unwrap();
        b.model_file = PathBuf::from("MODELS\\QWEN3-8B.GGUF");
        validate(&cfg).expect("case/separator-insensitive match must validate");
    }
}
