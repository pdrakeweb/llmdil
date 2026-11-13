//! Service and operations settings: telemetry, MXC / WinML,
//! Postgres, supervision, the admin API, log streaming, updates, and
//! notifications.

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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

/// How backends and the gateway are supervised. `Process` is the
/// historical behavior: the app spawns and supervises child processes
/// itself (manager-supervisor). `Service`: each backend and the
/// gateway are registered as Windows services and lifecycle calls
/// route to the SCM (manager-services); a service that is not
/// registered still falls back to process supervision, so the mode is
/// effectively per-service by registration state — see
/// docs/windows-services-design.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SupervisionMode {
    #[default]
    Process,
    Service,
}

/// Supervision settings. Defaults to process mode so configurations
/// written before the feature — and every install that never opts
/// in — behave exactly as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SupervisionConfig {
    #[serde(default)]
    pub mode: SupervisionMode,
}

/// Which local addresses the Admin API listener binds (auth-plan §5,
/// "Bind choices"). Applied when the listener starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AdminApiBindMode {
    /// 127.0.0.1 only: reachable from this machine (or through a
    /// tunnel) and nothing else. No firewall rule is needed.
    Loopback,
    /// Every interface (0.0.0.0), with the inbound firewall rule
    /// scoped to the local subnet and the Tailscale range. The
    /// default, because it is what every configuration written before
    /// the setting existed did — an existing install keeps exactly
    /// its listening surface.
    #[default]
    All,
    /// One specific local address ([`AdminApiConfig::bind_ip`]),
    /// under the same scoped firewall rule. If the address is not
    /// assigned to this machine when the listener starts, the
    /// listener falls back to loopback and says why — it never widens.
    Interface,
}

/// Admin REST API settings (see [`AppConfig::admin_api`](crate::AppConfig::admin_api)).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminApiConfig {
    /// Master switch. The listener exists only while this is true
    /// (persistent across restarts once enabled).
    pub enabled: bool,
    /// TCP port for the API listener (47821 per the plan; chosen to
    /// sit clear of the gateway and backend port ranges).
    pub port: u16,
    /// Listening surface. Absent in configs written before the
    /// setting existed, which therefore keep binding all interfaces.
    #[serde(default)]
    pub bind_mode: AdminApiBindMode,
    /// The address bound when `bind_mode` is `interface` (an IP
    /// literal, chosen from the machine's addresses in the UI).
    /// Ignored, but kept, in the other modes.
    #[serde(default)]
    pub bind_ip: Option<String>,
}

impl Default for AdminApiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 47821,
            bind_mode: AdminApiBindMode::All,
            bind_ip: None,
        }
    }
}

/// Credential Manager target for the log-streaming ingest token
/// (log-streaming design D6). Per-machine, write-only, revocable;
/// entered by the operator, read by the shipper at start, never
/// written into `config.json` and never readable back through any op.
pub const LOG_STREAMING_TOKEN_CRED_TARGET: &str = "local-llm-service-manager/log-streaming-token";

/// Destination kind for log streaming (design D1/D10: exactly one
/// destination; the wire format and redactor are identical for both).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LogStreamingTransport {
    /// The self-hosted receiver on Muse's VM (design Option B).
    #[default]
    Receiver,
    /// A hosted log service's HTTPS ingestion endpoint (Option A).
    Hosted,
}

/// Severity floor for shipped records (design §6.5). Records at or
/// above the floor ship; errors are the top severity, so they ship
/// at every floor. The raw severity ordering is Error > Warn > Info >
/// Debug.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LogStreamingLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
}

impl LogStreamingLevel {
    /// Severity rank (higher ships first-class): a record ships when
    /// its rank is >= the floor's rank.
    pub fn rank(self) -> u8 {
        match self {
            Self::Debug => 0,
            Self::Info => 1,
            Self::Warn => 2,
            Self::Error => 3,
        }
    }
}

/// Per-source streaming toggles (design §2.3 defaults). Every
/// toggle controls a real tap in the shipper (docs/log-streaming.md,
/// "Phase 2"); the shipper reads them when the app starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogStreamingSources {
    /// S3 wizard file tail.
    #[serde(default = "default_true")]
    pub wizard: bool,
    /// S4 optimize file tail.
    #[serde(default = "default_true")]
    pub optimize: bool,
    /// S2 gateway file tails.
    #[serde(default = "default_true")]
    pub gateway: bool,
    /// S5 app / supervisor events.
    #[serde(default = "default_true")]
    pub app: bool,
    /// S6 service-manage file tail.
    #[serde(default = "default_true")]
    pub service_manage: bool,
    /// S7 Admin API audit log tail.
    #[serde(default = "default_true")]
    pub audit: bool,
    /// S1 per-backend logs, opt-in (D3). One switch for every
    /// configured backend: there is no per-backend setting.
    #[serde(default)]
    pub backends: bool,
}

impl Default for LogStreamingSources {
    fn default() -> Self {
        Self {
            wizard: true,
            optimize: true,
            gateway: true,
            app: true,
            service_manage: true,
            audit: true,
            backends: false,
        }
    }
}

/// Log streaming settings (see [`AppConfig::log_streaming`](crate::AppConfig::log_streaming)).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogStreamingConfig {
    /// Master switch; default false — opt-in (D5).
    #[serde(default)]
    pub enabled: bool,
    /// Destination kind (D1).
    #[serde(default)]
    pub transport: LogStreamingTransport,
    /// Destination base URL (the receiver/hosted ingest origin; the
    /// shipper appends `/v1/batches`). Empty until the operator
    /// configures a destination.
    #[serde(default)]
    pub endpoint: String,
    /// Stable machine identity stamped on every record; defaults to
    /// the OS hostname when unset.
    #[serde(default)]
    pub machine_name: Option<String>,
    /// Severity floor for shipped records.
    #[serde(default)]
    pub level_floor: LogStreamingLevel,
    /// Per-source toggles.
    #[serde(default)]
    pub sources: LogStreamingSources,
}

impl Default for LogStreamingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            transport: LogStreamingTransport::Receiver,
            endpoint: String::new(),
            machine_name: None,
            level_floor: LogStreamingLevel::Info,
            sources: LogStreamingSources::default(),
        }
    }
}

/// Per-component update pins and auto-update toggles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatePolicy {
    pub components: HashMap<String, ComponentUpdate>,
}

/// App self-update preferences. Only the two operator switches are
/// configuration; everything else about self-update (last check,
/// staged installer, apply outcomes) is runtime state kept in the
/// app-update state file under the data directory, never here, so a
/// config write can neither forge nor erase a staged update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppUpdateConfig {
    /// Check GitHub Releases for a newer LLMdil at startup and
    /// periodically while running.
    #[serde(default = "default_true")]
    pub check_enabled: bool,
    /// Download and stage a newer release automatically when a
    /// check finds one. The staged installer is only ever *applied*
    /// by the operator pressing Restart (see manager-app's
    /// app_update module); staging never touches the running install.
    #[serde(default)]
    pub auto_update: bool,
}

impl Default for AppUpdateConfig {
    fn default() -> Self {
        Self {
            check_enabled: true,
            auto_update: false,
        }
    }
}

/// Update policy for one component (llama.cpp build, LiteLLM, models, ...).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentUpdate {
    pub pinned_version: Option<String>,
    pub auto_update: bool,
    pub last_checked: Option<DateTime<Utc>>,
}

/// Notification preferences (Windows toast notifications).
///
/// `enabled` is the master switch; the three category switches gate
/// the event classes the app toasts about (see the app layer's
/// notification wiring for the taxonomy). The category fields carry
/// serde defaults of `true` so configurations written before they
/// existed — which hold only `enabled` — load with every category on,
/// matching the behavior their single switch implied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationConfig {
    pub enabled: bool,
    /// Long-running work finishing: setup step runs, model downloads.
    #[serde(default = "default_true")]
    pub operations: bool,
    /// Things breaking: backends stopping unexpectedly, the gateway
    /// going down or failing to start, step failures, download
    /// failures.
    #[serde(default = "default_true")]
    pub failures: bool,
    /// An activation plan finishing applying, or failing to apply.
    #[serde(default = "default_true")]
    pub activation: bool,
}

/// Serde skip predicate for bool fields that serialize only when
/// set (BackendConfig::flags_advanced): a pre-field configuration
/// re-serializes byte-identically.
pub(crate) fn is_false(v: &bool) -> bool {
    !*v
}

pub(crate) fn default_true() -> bool {
    true
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            operations: true,
            failures: true,
            activation: true,
        }
    }
}

impl NotificationConfig {
    /// Whether operations-category toasts may fire (master + category).
    pub fn operations_enabled(&self) -> bool {
        self.enabled && self.operations
    }

    /// Whether failure-category toasts may fire (master + category).
    pub fn failures_enabled(&self) -> bool {
        self.enabled && self.failures
    }

    /// Whether activation-category toasts may fire (master + category).
    pub fn activation_enabled(&self) -> bool {
        self.enabled && self.activation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    #[test]
    fn supervision_defaults_to_process_mode() {
        let cfg = AppConfig::default_config();
        assert_eq!(cfg.supervision.mode, SupervisionMode::Process);
        // Configs written before the setting existed parse as process mode.
        let mut v = serde_json::to_value(&cfg).expect("serialize");
        v.as_object_mut().expect("object").remove("supervision");
        let back: AppConfig = serde_json::from_value(v).expect("deserialize");
        assert_eq!(back.supervision.mode, SupervisionMode::Process);
    }

    #[test]
    fn supervision_mode_round_trips() {
        let mut cfg = AppConfig::default_config();
        cfg.supervision.mode = SupervisionMode::Service;
        let json = serde_json::to_string(&cfg).expect("serialize");
        assert!(json.contains(r#""supervision":{"mode":"service"}"#));
        let back: AppConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.supervision.mode, SupervisionMode::Service);
    }

    #[test]
    fn notification_config_legacy_shape_loads_with_categories_on() {
        // Configurations written before the category switches exist
        // hold only `enabled`; the serde defaults must switch every
        // category on so the upgrade does not silently change what
        // the operator's single switch used to mean.
        let legacy: NotificationConfig = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert!(legacy.enabled);
        assert!(legacy.operations && legacy.failures && legacy.activation);
        assert!(legacy.operations_enabled());
        assert!(legacy.failures_enabled());
        assert!(legacy.activation_enabled());

        // A legacy config with the master switch off stays fully off.
        let legacy_off: NotificationConfig = serde_json::from_str(r#"{"enabled":false}"#).unwrap();
        assert!(!legacy_off.operations_enabled());
        assert!(!legacy_off.failures_enabled());
        assert!(!legacy_off.activation_enabled());
    }

    #[test]
    fn notification_config_category_gates_compose_with_master() {
        let mut n = NotificationConfig::default();
        assert!(n.operations_enabled() && n.failures_enabled() && n.activation_enabled());
        // Category off gates only its own category.
        n.failures = false;
        assert!(n.operations_enabled());
        assert!(!n.failures_enabled());
        assert!(n.activation_enabled());
        // Master off gates everything, categories preserved.
        n.enabled = false;
        assert!(!n.operations_enabled());
        assert!(!n.failures_enabled());
        assert!(!n.activation_enabled());
        // Round-trip keeps the explicit category choices.
        let s = serde_json::to_string(&n).unwrap();
        let back: NotificationConfig = serde_json::from_str(&s).unwrap();
        assert!(!back.enabled && !back.failures && back.operations && back.activation);
        // The default whole-config shape still validates.
        assert!(validate(&AppConfig::default_config()).is_ok());
    }
}
