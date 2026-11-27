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
//! Saving: [`validate`] first, then write + fsync a temp file, copy the
//! existing file to `.bak`, and rename the temp over the primary — so
//! `config.json` exists at every instant (see [`save`]).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

pub mod diagnostics;
pub mod diff;

mod catalog;
mod mcp;
mod migrate;
mod persist;
mod serving;
mod settings;
mod storage;
#[cfg(test)]
mod test_fixtures;
mod validate;

pub use diagnostics::export_diagnostics;
pub use diff::{apply_config_diff, diff_config, BackendPatch, ConfigDiff, FieldChange};

// The pre-split crate root's public surface, re-exported so every
// `manager_config::X` path is unchanged.
pub use catalog::*;
pub use mcp::*;
pub use migrate::*;
pub use persist::*;
pub use serving::*;
pub use settings::*;
pub use storage::*;
pub use validate::*;

/// Current config schema version.
pub const CURRENT_VERSION: u32 = 1;

/// Default LiteLLM pin for the gateway venv (finding F1 in
/// `docs/code-review-v0.1.60.md`).
///
/// The wizard installs `litellm[proxy]==<pin>` when
/// `updates.components["litellm"].pinned_version` is set and an
/// unpinned `litellm[proxy]` (whatever PyPI serves that day) when it is
/// not. An unpinned install is a supply-chain hazard — LiteLLM 1.107.0
/// was published malicious on Oct 21, 2026 — so the default config
/// carries this pin and [`migrate`] backfills it into any loaded config
/// whose litellm component has no pin. 1.105.1 is the version installed
/// and verified on Prometheus; the wizard's prisma-client-py pin
/// comments are written against LiteLLM 1.104/1.105. To move LiteLLM
/// forward, set a newer explicit pin (clearing it re-backfills this one).
pub const DEFAULT_LITELLM_PIN: &str = "1.105.1";

/// Component key of the LiteLLM gateway in [`UpdatePolicy::components`].
pub const LITELLM_COMPONENT: &str = "litellm";

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
    /// How backends and the gateway are supervised: directly spawned
    /// processes (the default, unchanged behavior) or registered
    /// Windows services driven through the SCM (feature branch
    /// `feature/windows-services`; see docs/windows-services-design.md).
    /// Absent in configurations written before the setting existed.
    #[serde(default)]
    pub supervision: SupervisionConfig,
    /// Admin REST API (admin-api plan): opt-in remote administration.
    /// Disabled by default; when enabled the API binds `bind_mode`
    /// (all interfaces by default — loopback only, or one interface
    /// address, are the alternatives) on `port`, with a firewall rule
    /// scoped to the local subnet and the Tailscale range whenever the
    /// bind is not loopback. Only enablement, the port and the bind
    /// choice are configuration — users, sessions, and tokens live in the
    /// dedicated `admin-users.json` store, never here, so a config
    /// write can never mint or elevate an API user.
    #[serde(default)]
    pub admin_api: AdminApiConfig,
    /// Log streaming (log-streaming design D1–D10): the in-app
    /// shipper pushes redacted log records to a remote destination
    /// Muse can review in near realtime. Disabled by default (D5);
    /// the ingest token is NOT configuration — it lives in Credential
    /// Manager under [`LOG_STREAMING_TOKEN_CRED_TARGET`] (D6), so a
    /// config write (or a diagnostics bundle carrying the config) can
    /// never leak it. Absent in configurations written before the
    /// feature existed.
    #[serde(default)]
    pub log_streaming: LogStreamingConfig,
    /// Debug console (docs/debug-console.md): the remote-execution
    /// gate, the API raw-shell gate, and the executable allowlist for
    /// recipe steps. Absent in configurations written before the
    /// console existed; the defaults are the locked-down ones.
    #[serde(default)]
    pub diagnostics: DiagnosticsConfig,
    /// Tiered model storage (docs/tiered-model-storage.md): cold tiers
    /// and the promotion/demotion policy. Absent in configurations
    /// written before the feature existed; the default has no cold
    /// tiers, so every model resolves to the hot tier exactly as before.
    #[serde(default)]
    pub model_storage: ModelStorageConfig,
    /// Per-component pin / auto-update toggles.
    pub updates: UpdatePolicy,
    /// App self-update preferences (Settings → Updates & diagnostics):
    /// whether LLMdil itself checks GitHub Releases for a newer
    /// version and whether a found release is downloaded and staged
    /// automatically. Distinct from `updates` above, which pins
    /// *components* (LiteLLM, llama.cpp builds, …) the wizard manages;
    /// the two never share state. Absent in configurations written
    /// before the section existed — the defaults are the Pete-safe
    /// pair (checking on, auto-update off).
    #[serde(default)]
    pub app_update: AppUpdateConfig,
    pub notifications: NotificationConfig,
    /// Set when the user finishes the setup wizard; the UI opens the wizard
    /// on launch until this is true.
    #[serde(default)]
    pub setup_completed: bool,
    /// Deployment purpose for the Optimize page's consultant (Optimize
    /// design §2.3): what this deployment is for, in the operator's own
    /// words. Empty = unseeded; the Optimize page seeds it from the
    /// install on first use and the operator owns it afterwards. Sent
    /// only to a local backend during consultation — never a secret.
    #[serde(default)]
    pub optimize_purpose: String,
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
    /// Persisted MCP server registry (MCP design §3.1). Serde-defaulted
    /// so configurations written before the section existed load with
    /// the seeded legacy registry (see [`McpConfig::default`]); an
    /// explicit section — even an empty one — is preserved verbatim by
    /// [`migrate`] (see §10.1 migration there).
    #[serde(default)]
    pub mcp: McpConfig,
    /// Set once the Phase 3 curated MCP servers (`brave_search` /
    /// `web_fetch`, MCP design §3.3) have been seeded into
    /// [`AppConfig::mcp`]. Follows the `defaults_repaired` precedent:
    /// configurations created by current code are born with this set
    /// (their registry already carries the curated entries), and
    /// [`migrate`] performs the one-time append for older files —
    /// keyed on the marker's ABSENCE from the raw JSON, because the
    /// defaults overlay would otherwise fill it in. Once set, an
    /// operator who deletes the curated entries is never reseeded.
    #[serde(default)]
    pub mcp_curated_seeded: bool,
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
                purpose: String::new(),
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
                flags_advanced: false,
                restart_policy: RestartPolicy {
                    max_retries: 5,
                    backoff_base_secs: 2,
                },
                overrides_global: false,
                tool_call_probe: None,
                purpose: String::new(),
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
                    overflow: OverflowEscalationConfig::default(),
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
            supervision: SupervisionConfig::default(),
            admin_api: AdminApiConfig::default(),
            log_streaming: LogStreamingConfig::default(),
            diagnostics: DiagnosticsConfig::default(),
            model_storage: ModelStorageConfig::default(),
            updates: UpdatePolicy {
                components: HashMap::from([(
                    LITELLM_COMPONENT.to_string(),
                    ComponentUpdate {
                        pinned_version: Some(DEFAULT_LITELLM_PIN.to_string()),
                        auto_update: false,
                        last_checked: None,
                    },
                )]),
            },
            app_update: AppUpdateConfig::default(),
            notifications: NotificationConfig::default(),
            setup_completed: false,
            optimize_purpose: String::new(),
            catalog_seeded: false,
            defaults_repaired: true,
            catalog_revision: 0,
            mcp: McpConfig::seeded(),
            // Born seeded: the registry above already carries the
            // curated entries (see the field's documentation).
            mcp_curated_seeded: true,
        }
    }
}

impl AppConfig {
    /// Resolve a model's logical path against the storage tiers (see
    /// [`resolve_model_path`]): the file every filesystem access to a
    /// model must use.
    pub fn resolve_model(&self, logical: &std::path::Path) -> ResolvedModel {
        resolve_model_path(&self.data_dir, &self.model_storage, logical)
    }
}

/// Serde default for [`GatewayConfig::bind_host`]: all interfaces.
pub fn default_gateway_bind_host() -> String {
    "0.0.0.0".to_string()
}

/// Default data directory: `%APPDATA%\local-llm-service-manager`
/// (falls back to `$HOME/.local-llm-service-manager` when `APPDATA` is
/// unset, and to a relative `.local-llm-service-manager` when neither is).
pub fn default_data_dir() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        PathBuf::from(appdata).join("local-llm-service-manager")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local-llm-service-manager")
    } else {
        PathBuf::from(".local-llm-service-manager")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
