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
    /// How backends and the gateway are supervised: directly spawned
    /// processes (the default, unchanged behavior) or registered
    /// Windows services driven through the SCM (feature branch
    /// `feature/windows-services`; see docs/windows-services-design.md).
    /// Absent in configurations written before the setting existed.
    #[serde(default)]
    pub supervision: SupervisionConfig,
    /// Per-component pin / auto-update toggles.
    pub updates: UpdatePolicy,
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

/// MCP transport, as persisted in [`McpServerConfig`].
///
/// Defined here, in the schema crate, rather than reusing
/// `manager_mcp::McpTransport`: `manager-config` is a leaf crate and
/// `manager-mcp` already depends on the leaf supervisor crate — having
/// config depend on `manager-mcp` would invert that layering. The serde
/// shape is identical to the `manager-mcp` enum (lowercase `"stdio"` /
/// `"http"`), and `manager-mcp` converts between the two (it may depend
/// on this leaf crate without a cycle). This is a deliberate
/// design-vs-code discrepancy from design §3.1, which assumed the
/// existing `manager-mcp` enum could be reused directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    #[default]
    Stdio,
    Http,
}

/// Where an MCP server is exposed (MCP design §3.1). Replaces the
/// session-only `McpMode` overlay for persistence. Serde naming follows
/// this crate's convention for mode enums (`lowercase`, as on
/// [`ViewMode`]/[`MxcMode`]): `"gateway"` / `"local"` / `"disabled"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum McpExposure {
    /// Registered in the generated LiteLLM gateway config.
    Gateway,
    /// App-supervised local process (the legacy github/gdrive behaviour).
    Local,
    /// Not registered and not spawned.
    #[default]
    Disabled,
}

/// Credential Manager target for the legacy GitHub MCP server, in the
/// unified namespace the wizard uses (MCP design §10.1). The same
/// string is `GITHUB_CRED_TARGET` in `manager-wizard`; it is defined
/// here because the registry seed lives in this leaf crate and the
/// wizard depends on config, not the reverse.
pub const GITHUB_MCP_CRED_REF: &str = "local-llm-service-manager/github-mcp";
/// Credential Manager target for the legacy Google Drive MCP server,
/// unified namespace (see [`GITHUB_MCP_CRED_REF`]).
pub const GDRIVE_MCP_CRED_REF: &str = "local-llm-service-manager/gdrive-mcp";
/// Pre-unification credential target for the GitHub server
/// (`llm-manager/mcp/github`, from `manager_mcp::default_servers`).
/// Presence checks accept either target during the transition
/// (MCP design §10.1).
pub const LEGACY_GITHUB_MCP_CRED_REF: &str = "llm-manager/mcp/github";
/// Pre-unification credential target for the Drive server.
pub const LEGACY_GDRIVE_MCP_CRED_REF: &str = "llm-manager/mcp/gdrive";
/// Credential Manager target for the curated Brave Search MCP server
/// (MCP design §3.3).
pub const BRAVE_SEARCH_MCP_CRED_REF: &str = "local-llm-service-manager/brave-search-mcp";

/// Pinned Brave Search MCP server package (MCP design §3.3 / §6.1).
///
/// Pin: `@brave/brave-search-mcp-server@2.1.4` — the npm registry's
/// `dist-tags.latest` for `@brave/brave-search-mcp-server`, resolved
/// 2026-10-17. Recorded in the design's §3.3 implementation record.
/// Bumping it is a code change, never a runtime resolution.
pub const BRAVE_SEARCH_PACKAGE_PIN: &str = "@brave/brave-search-mcp-server@2.1.4";
/// Pinned URL-fetch MCP server package (MCP design §3.3 / §6.2).
///
/// Pin: `mcp-server-fetch@2026.10.10` — the PyPI latest release of
/// the date-versioned `mcp-server-fetch` project, resolved
/// 2026-10-17. Recorded in the design's §3.3 implementation record.
pub const WEB_FETCH_PACKAGE_PIN: &str = "mcp-server-fetch@2026.10.10";
/// The Brave tools exposed by default (MCP design §3.3): the other
/// six Brave tools stay off until the operator edits the server.
/// Emitted both as LiteLLM `allowed_tools` and as the server's own
/// `BRAVE_MCP_ENABLED_TOOLS` trimming env (by `manager-gateway`).
pub const BRAVE_DEFAULT_ALLOWED_TOOLS: &[&str] = &["brave_web_search", "brave_llm_context"];

/// SearXNG search-provider settings (MCP design §6.3): SearXNG is
/// NOT an MCP server — LiteLLM ships it as a built-in search
/// provider, reached through its Search API. These settings live on
/// [`McpConfig`] because §6.3 specifies the feature inside the MCP
/// capabilities design and the registry section is where its toggle
/// belongs in the persisted shape; generation emits a top-level
/// `search_tools:` block (never inside `mcp_servers:`) and the
/// gateway spawn env gains `SEARXNG_API_BASE`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearxngSettings {
    /// Operator toggle; off by default (no instance is stood up by
    /// the app — §6.3's container is the operator's to run).
    pub enabled: bool,
    /// Base URL of the operator's SearXNG instance, e.g.
    /// `http://127.0.0.1:8080`. Not a secret: it is plain config,
    /// shown in the gateway plan like other non-secret values.
    /// Validated as a non-empty http(s) URL only while `enabled`.
    pub api_base: String,
}

/// The persisted MCP server registry (MCP design §3.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
    /// SearXNG search-provider settings (§6.3). Serde-defaulted so
    /// registry JSON written before the settings existed loads with
    /// SearXNG disabled and no instance URL.
    #[serde(default)]
    pub searxng: SearxngSettings,
}

impl McpConfig {
    /// The seeded registry: the two legacy servers from
    /// `manager_mcp::default_servers()`, as `Local` entries, but with
    /// the unified credential namespace and the Drive env key
    /// canonicalised to `GOOGLE_APPLICATION_CREDENTIALS` (MCP design
    /// §10.1; the legacy registry's `GDRIVE_CREDENTIALS_PATH` was the
    /// divergent name — the Drive server reads
    /// `GOOGLE_APPLICATION_CREDENTIALS`, which is also what the wizard
    /// step always wrote). Command/args/package parity with
    /// `default_servers()` is otherwise exact: `npx -y` on the same
    /// `@modelcontextprotocol/server-*` packages. Phase 3 appends the
    /// two curated servers ([`curated_mcp_servers`]) after the legacy
    /// pair, so a fresh registry carries all four entries.
    pub fn seeded() -> Self {
        Self {
            servers: seeded_mcp_servers(),
            searxng: SearxngSettings::default(),
        }
    }
}

/// `Default` for [`McpConfig`] is the *seeded* registry, not an empty
/// one: `AppConfig`'s `#[serde(default)] mcp` field uses this, so a
/// config file with no `mcp` key at all loads with the legacy servers
/// present (the §10.1 migration for pre-feature configs). An explicit
/// empty registry is written as `McpConfig { servers: Vec::new() }`
/// and is preserved as empty — serde only invokes this default when
/// the key is absent, and [`migrate`] preserves an explicit section
/// verbatim.
impl Default for McpConfig {
    fn default() -> Self {
        Self::seeded()
    }
}

/// The curated Phase 3 seed servers (MCP design §3.3): Brave Search
/// (Gateway-exposed, metered) and URL fetch (Disabled by default,
/// §13d). Pins are the [`BRAVE_SEARCH_PACKAGE_PIN`] /
/// [`WEB_FETCH_PACKAGE_PIN`] constants — exact versions resolved
/// from the primary registries (npm / PyPI) on 2026-10-17 and
/// recorded in the design's §3.3 implementation record.
pub fn curated_mcp_servers() -> Vec<McpServerConfig> {
    vec![
        McpServerConfig {
            id: "brave_search".to_string(),
            display_name: "Brave Search".to_string(),
            transport: McpTransport::Stdio,
            exposure: McpExposure::Gateway,
            command: Some("npx".to_string()),
            args: vec![
                "-y".to_string(),
                BRAVE_SEARCH_PACKAGE_PIN.to_string(),
                "--transport".to_string(),
                "stdio".to_string(),
            ],
            url: None,
            env_keys: vec!["BRAVE_API_KEY".to_string()],
            cred_ref: Some(BRAVE_SEARCH_MCP_CRED_REF.to_string()),
            allowed_tools: Some(
                BRAVE_DEFAULT_ALLOWED_TOOLS
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
            metered: true,
            // Stored on the entry and surfaced via `mcp_list` only.
            // It is deliberately NOT emitted into the generated
            // LiteLLM YAML: LiteLLM's `mcp_rpm_limit` is a key/team
            // property, not a server-config property, and emitting
            // an invented server key risks a LiteLLM config
            // rejection. Actual enforcement attaches at virtual-key
            // issuance (design §5.5) — which this app does not
            // perform (there is no virtual-key issuance surface;
            // the Phase 2 discrepancy recorded in
            // docs/mcp-capabilities-design.md §12 / the Phase 2
            // implementation), so in v1 the value is the documented
            // cap the operator sees, not a gateway-enforced limit.
            rpm_limit: Some(20),
        },
        McpServerConfig {
            id: "web_fetch".to_string(),
            // URL fetching only — never described as search (§6.2).
            display_name: "Web Fetch".to_string(),
            transport: McpTransport::Stdio,
            // Disabled until explicitly enabled (§5.2 / §13d).
            exposure: McpExposure::Disabled,
            command: Some("uvx".to_string()),
            args: vec![WEB_FETCH_PACKAGE_PIN.to_string()],
            url: None,
            env_keys: Vec::new(),
            cred_ref: None,
            // `fetch` is the server's only tool (§6.2).
            allowed_tools: Some(vec!["fetch".to_string()]),
            metered: false,
            rpm_limit: None,
        },
    ]
}

/// The seed servers for [`McpConfig::seeded`] (see its documentation
/// for the unification/canonicalisation rules applied): the two
/// legacy Local servers followed by the Phase 3 curated pair
/// ([`curated_mcp_servers`]).
pub fn seeded_mcp_servers() -> Vec<McpServerConfig> {
    let mut servers = vec![
        McpServerConfig {
            id: "github".to_string(),
            display_name: "GitHub".to_string(),
            transport: McpTransport::Stdio,
            exposure: McpExposure::Local,
            command: Some("npx".to_string()),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-github".to_string(),
            ],
            url: None,
            env_keys: vec!["GITHUB_PERSONAL_ACCESS_TOKEN".to_string()],
            cred_ref: Some(GITHUB_MCP_CRED_REF.to_string()),
            allowed_tools: None,
            metered: false,
            rpm_limit: None,
        },
        McpServerConfig {
            id: "gdrive".to_string(),
            display_name: "Google Drive".to_string(),
            transport: McpTransport::Stdio,
            exposure: McpExposure::Local,
            command: Some("npx".to_string()),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-gdrive".to_string(),
            ],
            url: None,
            // Canonicalised (design §10.1): not the legacy
            // GDRIVE_CREDENTIALS_PATH.
            env_keys: vec!["GOOGLE_APPLICATION_CREDENTIALS".to_string()],
            cred_ref: Some(GDRIVE_MCP_CRED_REF.to_string()),
            allowed_tools: None,
            metered: false,
            rpm_limit: None,
        },
    ];
    servers.extend(curated_mcp_servers());
    servers
}

/// One persisted MCP server definition (MCP design §3.1). Every field
/// carries a serde default so a partial JSON object loads: missing
/// strings are empty, missing lists are empty, missing options are
/// `None`, `transport` defaults to `stdio`, and `exposure` defaults
/// to `disabled` (the safe choice for a partially-specified server —
/// it is neither registered with the gateway nor spawned).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    pub id: String,
    pub display_name: String,
    pub transport: McpTransport,
    pub exposure: McpExposure,
    /// Stdio command; allowlist-validated at save (§3.2).
    pub command: Option<String>,
    /// Stdio args; pinned package versions for curated servers.
    pub args: Vec<String>,
    /// HTTP transport base URL.
    pub url: Option<String>,
    /// Env var NAMES the server needs; never values.
    pub env_keys: Vec<String>,
    /// Credential Manager target name for the `env_keys` values.
    pub cred_ref: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    /// Paid upstream; drives the rpm cap + UI cost labelling.
    pub metered: bool,
    pub rpm_limit: Option<u32>,
}

/// Presenter mode for a data window (spec §4 hybrid view matrix).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ViewMode {
    Appliance,
    Console,
    Topology,
}

/// Outcome of a backend tool-call probe (MCP design §7.2): whether
/// the backend actually emits structured `tool_calls`. Mirrors the
/// supervisor probe's outcomes (`manager_supervisor::ToolCallProbeOutcome`)
/// with serde-friendly lowercase names for the persisted config form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolCallProbeOutcome {
    /// `choices[0].message.tool_calls` was present, named the probe
    /// tool, and its arguments parsed as JSON.
    Verified,
    /// No structured `tool_calls`, but tool-call markup (e.g.
    /// `<tool_call>…`) leaked into the message content.
    Unparsed,
    /// Anything else: health gate failed, transport/HTTP error, or a
    /// plain-content reply with no tool call.
    Failed,
}

/// A persisted tool-call probe result for one backend (MCP design
/// §7.2). Runtime state, not operator configuration: it is measured
/// by the `backend_test_tool_call` command, never edited by the
/// operator, never appears in a config diff / activation change, and
/// resets to `None` (not measured) when the backend's `model_file`
/// or `raw_flags` change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallProbe {
    pub outcome: ToolCallProbeOutcome,
    /// When the probe ran, as a Unix-epoch millisecond timestamp.
    pub at_ms: u64,
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
    /// loopback and the gateway is the only network entry point.
    /// `--no-jinja` is likewise reserved and filtered out (MCP design
    /// §7.1): `--jinja` is app-owned so tool calling stays enabled.
    /// None means no extra flags.
    pub raw_flags: Option<String>,
    pub restart_policy: RestartPolicy,
    pub overrides_global: bool,
    /// The last tool-call probe result for this backend, or `None`
    /// when it has not been measured (MCP design §7.2). Serde-defaulted
    /// so configurations written before the field existed load with
    /// no probe recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_probe: Option<ToolCallProbe>,
    /// What this backend is for, in the operator's words (Optimize
    /// design §2.3). Empty = unstated; the consultant is told to infer
    /// cautiously from names when intent is missing. Consultation
    /// metadata only — it never affects serving.
    #[serde(default)]
    pub purpose: String,
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
    /// What this group is for, in the operator's words (Optimize
    /// design §2.3). Empty = unstated. Consultation metadata only.
    #[serde(default)]
    pub purpose: String,
}

/// Routing strategy for a model group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RoutingStrategy {
    SimpleShuffle,
    LeastBusy,
    LatencyBased,
}

/// A routing rule (routing-rules design §6.2). Typed kinds are
/// validated and emitted by the gateway config generator; a rule
/// carrying only a legacy `litellm_fragment` is inert (never
/// emitted) — see docs/routing-rules-design.md §6.1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingRule {
    pub id: String,
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub kind: Option<RoutingRuleKind>,
    /// Legacy raw fragment. Deprecated: retained so old configs
    /// load; never emitted into the generated gateway config.
    #[serde(default)]
    pub litellm_fragment: Option<serde_json::Value>,
    /// Where the rule came from (wizard default | user).
    pub source: String,
}

/// The typed behavior of a [`RoutingRule`]. New kinds are added
/// here as new variants; the tag is the serialized `type` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum RoutingRuleKind {
    /// Requests too large for their group are rerouted to a
    /// large-context group; past the optimal window the serving
    /// model is instructed to advise compaction (design §4–§5).
    ContextOverflow {
        /// The group oversized requests are rerouted to. Must name
        /// an existing configured group; `openrouter` is rejected
        /// (validated in [`validate`]).
        target_group: String,
        /// Advisory threshold as a fraction of the serving group's
        /// hard window, in (0, 1). Ignored when `optimal_tokens`
        /// is set.
        #[serde(default = "default_optimal_fraction")]
        optimal_fraction: f64,
        /// Absolute advisory threshold in tokens; wins over
        /// `optimal_fraction` when set.
        #[serde(default)]
        optimal_tokens: Option<u32>,
        /// Output tokens assumed when a request sets no
        /// `max_tokens` (design decision D3).
        #[serde(default = "default_output_reserve")]
        output_reserve_tokens: u32,
        /// Whether the compaction advisory is injected past the
        /// optimal window; off = silent rerouting only.
        #[serde(default = "default_true")]
        advisory: bool,
    },
}

/// Serde default for the context-overflow rule's optimal fraction
/// (design decision D4: half the serving group's hard window).
fn default_optimal_fraction() -> f64 {
    0.5
}

/// Serde default for the context-overflow rule's output reserve
/// (design decision D3).
fn default_output_reserve() -> u32 {
    1024
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
    /// The pinned SHA-256 the file was verified against when
    /// `verified_at` was stamped. The launch-time audit's
    /// verified-by-stamp shortcut is valid only while this still
    /// equals the current pin (`sha256` above): a stamp alone does
    /// not say WHAT was verified, and a catalog refresh that
    /// replaces the pin must invalidate stamps taken against the
    /// old one (code-review 2.4 — previously the shortcut trusted
    /// the timestamp plus the file length, so a same-length
    /// replacement file, or a re-pin, inherited trust it never
    /// earned). Absent in configurations written before the field
    /// existed; those models are re-hashed once and re-stamped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_sha256: Option<String>,
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
    /// Operator-chosen short display name (renaming in the Models
    /// view). Wins over the curated catalog name wherever a short
    /// name is shown; `None` falls back to the catalog name, then
    /// the model id. Catalog definition refreshes never touch it —
    /// it is the operator's own label, not catalog data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// Validate an operator-chosen model display name for
/// [`ModelConfig::display_name`]: trims surrounding whitespace,
/// rejects empty and over-long names, and returns the name to store.
/// Embedded control characters (newlines included) are rejected:
/// the name renders in single-line UI labels and log lines, which
/// inherit whatever it carries.
pub fn validate_display_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("display name must not be empty".to_string());
    }
    if name.chars().count() > 64 {
        return Err("display name must be at most 64 characters".to_string());
    }
    if name.chars().any(|c| c.is_control()) {
        return Err("display name must not contain control characters".to_string());
    }
    Ok(name.to_string())
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

fn default_true() -> bool {
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
            updates: UpdatePolicy {
                components: HashMap::new(),
            },
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
            verified_sha256: None,
            source_url: entry.source_url.clone(),
            assigned_backends: entry.assigned_backends.clone(),
            enabled: entry.enabled,
            display_name: None,
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
    model.verified_sha256 = None;
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

/// Reset tool-call probes that a serving change has made stale
/// (MCP design §7.2): for every backend in `current` whose
/// `model_file` or `raw_flags` differs from the same-id backend in
/// `previous`, the recorded probe is cleared to `None` (not
/// measured) — it described a different model or different launch
/// flags. Backends new in `current` keep whatever probe they carry
/// (a new backend is constructed with `None`). Returns true when
/// any probe was reset.
pub fn reset_stale_tool_call_probes(previous: &AppConfig, current: &mut AppConfig) -> bool {
    let mut changed = false;
    for backend in current.backends.iter_mut() {
        if backend.tool_call_probe.is_none() {
            continue;
        }
        let Some(old) = previous.backends.iter().find(|b| b.id == backend.id) else {
            continue;
        };
        if old.model_file != backend.model_file || old.raw_flags != backend.raw_flags {
            backend.tool_call_probe = None;
            changed = true;
        }
    }
    changed
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
        if let Some(name) = &m.display_name {
            if let Err(e) = validate_display_name(name) {
                errors.push(format!("model {:?}: display_name: {e}", m.id));
            }
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
    let mut enabled_overflow_rules = 0usize;
    for r in &config.gateway.routing_rules {
        if !rule_ids.insert(r.id.clone()) {
            errors.push(format!("duplicate routing rule id {:?}", r.id));
        }
        // Typed-rule validation (routing-rules design §6.2). Legacy
        // fragment-only rules (kind: None) are inert and unchecked
        // beyond the id. Target checks apply to ENABLED rules only:
        // a disabled rule is inert, and checking its target here
        // would make deleting a group fail every unrelated save
        // while a disabled rule still points at it — the target is
        // re-validated when the rule is re-enabled, because enabling
        // goes through this same validation.
        let Some(kind) = &r.kind else { continue };
        match kind {
            RoutingRuleKind::ContextOverflow {
                target_group,
                optimal_fraction,
                optimal_tokens,
                ..
            } => {
                if !optimal_fraction.is_finite()
                    || *optimal_fraction <= 0.0
                    || *optimal_fraction >= 1.0
                {
                    errors.push(format!(
                        "routing rule {:?}: optimal_fraction must be in (0, 1)",
                        r.id
                    ));
                }
                if let Some(t) = optimal_tokens {
                    if *t == 0 {
                        errors.push(format!(
                            "routing rule {:?}: optimal_tokens must be > 0 when set",
                            r.id
                        ));
                    }
                }
                if r.enabled {
                    enabled_overflow_rules += 1;
                    if target_group == "openrouter" {
                        errors.push(format!(
                            "routing rule {:?}: \"openrouter\" cannot be a context-overflow target (oversized local traffic must never route to the metered cloud tier)",
                            r.id
                        ));
                    } else if !group_names.contains(target_group) {
                        errors.push(format!(
                            "routing rule {:?}: target group {target_group:?} does not exist",
                            r.id
                        ));
                    } else if let Some(t) = optimal_tokens {
                        // optimal_tokens must sit below the target's
                        // hard window. The exact window applies the
                        // catalog context cap (manager-models), which
                        // this crate cannot see; the enabled members'
                        // configured n_ctx is an upper bound on it, so
                        // a value at or above that bound is certainly
                        // wrong (design §6.2: "when computable").
                        let bound = config
                            .gateway
                            .groups
                            .iter()
                            .find(|g| &g.name == target_group)
                            .and_then(|g| {
                                g.members
                                    .iter()
                                    .filter_map(|m| {
                                        config.backends.iter().find(|b| &b.id == m && b.enabled)
                                    })
                                    .map(|b| b.flags.n_ctx)
                                    .min()
                            });
                        if let Some(bound) = bound {
                            if *t >= bound {
                                errors.push(format!(
                                    "routing rule {:?}: optimal_tokens {t} must be below the target group's hard window (at most {bound} from its enabled members' configured context)",
                                    r.id
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    if enabled_overflow_rules > 1 {
        errors.push(
            "at most one enabled context-overflow routing rule is supported".to_string(),
        );
    }

    // --- mcp servers (design §3.2) ---
    validate_mcp_servers(&config.mcp, &mut errors);
    validate_searxng(&config.mcp.searxng, &mut errors);

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
    // MCP registry one-time migration (design §10.1), via the raw-JSON
    // route: capture an explicit `mcp` section before defaults are
    // overlaid. `apply_defaults` deep-merges over `default_config()`
    // (whose `mcp` is the seed), so a config with NO `mcp` key gets the
    // seeded registry injected — exactly once, at load; the seeded
    // registry is then written back on the next save, so the key
    // exists from then on. An explicit section, even an empty
    // `{"servers": []}`, is restored verbatim after the merge, so an
    // operator who later deletes every server is NOT resurrected on
    // the next load. (A `null` mcp is treated as absent.)
    let explicit_mcp = raw
        .as_object()
        .and_then(|o| o.get("mcp"))
        .filter(|v| !v.is_null())
        .cloned();
    // Curated-seeding marker (Phase 3, design §3.3): whether the raw
    // file carries `mcp_curated_seeded` at all. The defaults overlay
    // below fills a missing key from `default_config()` — where the
    // marker is born set — so presence in the RAW JSON is the only
    // honest signal that the one-time append already ran for this
    // file. A pre-Phase-3 file has no marker key; a Phase 3 file
    // always does (the field serializes unconditionally).
    let mcp_curated_marker_present = raw
        .as_object()
        .map(|o| o.contains_key("mcp_curated_seeded"))
        .unwrap_or(false);
    let mut merged = raw;
    apply_defaults(&mut merged)?;
    if let Some(mcp) = explicit_mcp {
        if let Some(obj) = merged.as_object_mut() {
            obj.insert("mcp".to_string(), mcp);
        }
    }
    if !mcp_curated_marker_present {
        if let Some(obj) = merged.as_object_mut() {
            obj.insert("mcp_curated_seeded".to_string(), serde_json::json!(false));
        }
    }
    if version != CURRENT_VERSION {
        if let Some(obj) = merged.as_object_mut() {
            obj.insert("version".to_string(), serde_json::json!(CURRENT_VERSION));
        }
    }
    let mut cfg: AppConfig = serde_json::from_value(merged).map_err(|e| {
        ConfigError::MigrationFailed(format!("migrated config failed schema check: {e}"))
    })?;
    // One-time curated append (Phase 3): a configuration that has
    // never been through this migration gains any curated server it
    // lacks (by id — an operator-renamed or edited entry with a
    // curated id is left alone), exactly once: the marker is set on
    // the returned config and persists on the next save, so curated
    // entries the operator later deletes stay deleted. This composes
    // with the verbatim explicit-`mcp` restore above: the restore
    // decides what the file said; this append is the migration the
    // design orders for pre-Phase-3 files.
    if !cfg.mcp_curated_seeded {
        for server in curated_mcp_servers() {
            if !cfg.mcp.servers.iter().any(|s| s.id == server.id) {
                cfg.mcp.servers.push(server);
            }
        }
        cfg.mcp_curated_seeded = true;
    }
    Ok(cfg)
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

/// Stdio commands LiteLLM allows for MCP servers; the manager mirrors
/// the list at save time (MCP design §3.2 / §5.1).
const MCP_STDIO_COMMAND_ALLOWLIST: &[&str] =
    &["npx", "uvx", "python", "python3", "node", "docker", "deno"];

/// Banned MCP server args (MCP design §3.2): rejected at save, never
/// stripped, so the operator sees the refusal.
const MCP_BANNED_ARGS: &[&str] = &["--allow-private-ips", "--ignore-robots-txt"];

/// True when `id` matches `^[a-z][a-z0-9_]*$` (hand-rolled: this crate
/// has no regex dependency, and the alphabet is small).
fn is_valid_mcp_server_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The basename used for the stdio allowlist check: the last path
/// component, splitting on BOTH `/` and `\` (a persisted config may
/// carry a Windows-style path such as `C:\tools\npx.cmd` regardless of
/// the OS reading it), lowercased, with a single trailing executable
/// extension (`.exe` / `.cmd` / `.bat`) removed. So `npx`,
/// `C:\tools\npx.cmd`, `/usr/local/bin/NPX.EXE`, and `npx.exe` all
/// normalise to `npx`.
fn mcp_command_basename(command: &str) -> String {
    let base = command
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(command)
        .to_lowercase();
    for ext in [".exe", ".cmd", ".bat"] {
        if let Some(stripped) = base.strip_suffix(ext) {
            return stripped.to_string();
        }
    }
    base
}

/// Pragmatic HTTP(S) URL check for MCP http servers (this crate has
/// no `url` dependency; the existing style is hand-rolled checks like
/// the gateway `bind_host` IP parse). Returns the lowercase host on
/// success. Rejects: no `://`, a scheme other than http/https, an
/// empty host, whitespace/control characters, and a non-numeric port.
fn parse_mcp_http_url(url: &str) -> Result<(String, String), String> {
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("url must not contain whitespace or control characters".to_string());
    }
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| "url must be of the form <scheme>://<host>[/...]".to_string())?;
    let scheme = scheme.to_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(format!("url scheme {scheme:?} is not http or https"));
    }
    // Authority ends at the first path/query/fragment delimiter.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Strip any userinfo.
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if authority.is_empty() {
        return Err("url has no host".to_string());
    }
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        // IPv6 literal: [::1] or [::1]:port
        let (h, tail) = bracketed
            .split_once(']')
            .ok_or_else(|| "url has an unterminated IPv6 host".to_string())?;
        if let Some(port) = tail.strip_prefix(':') {
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("url port {port:?} is not numeric"));
            }
        } else if !tail.is_empty() {
            return Err("url has unexpected characters after the IPv6 host".to_string());
        }
        h.to_lowercase()
    } else {
        match authority.rsplit_once(':') {
            Some((h, port)) => {
                if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(format!("url port {port:?} is not numeric"));
                }
                h.to_lowercase()
            }
            None => authority.to_lowercase(),
        }
    };
    if host.is_empty() {
        return Err("url has no host".to_string());
    }
    Ok((scheme, host))
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// MCP registry validation (MCP design §3.2), called from [`validate`].
fn validate_mcp_servers(mcp: &McpConfig, errors: &mut Vec<String>) {
    let mut ids: HashSet<String> = HashSet::new();
    for s in &mcp.servers {
        if !is_valid_mcp_server_id(&s.id) {
            errors.push(format!(
                "mcp server id {:?} must match ^[a-z][a-z0-9_]*$",
                s.id
            ));
        } else if !ids.insert(s.id.clone()) {
            errors.push(format!("duplicate mcp server id {:?}", s.id));
        }
        for arg in &s.args {
            for banned in MCP_BANNED_ARGS {
                if arg == banned || arg.starts_with(&format!("{banned}=")) {
                    errors.push(format!(
                        "mcp server {:?}: arg {banned:?} is banned and is rejected, not stripped",
                        s.id
                    ));
                }
            }
        }
        match s.transport {
            McpTransport::Stdio => match s.command.as_deref().map(str::trim) {
                Some(cmd) if !cmd.is_empty() => {
                    let base = mcp_command_basename(cmd);
                    if !MCP_STDIO_COMMAND_ALLOWLIST.contains(&base.as_str()) {
                        errors.push(format!(
                                "mcp server {:?}: command basename {base:?} is not in the stdio allowlist {:?}",
                                s.id, MCP_STDIO_COMMAND_ALLOWLIST
                            ));
                    }
                }
                _ => errors.push(format!(
                    "mcp server {:?}: stdio transport requires a command",
                    s.id
                )),
            },
            McpTransport::Http => match s.url.as_deref().map(str::trim) {
                Some(url) if !url.is_empty() => match parse_mcp_http_url(url) {
                    Ok((scheme, host)) => {
                        if scheme == "http" && !is_loopback_host(&host) {
                            errors.push(format!(
                                    "mcp server {:?}: http url is only allowed on loopback (127.0.0.1 / localhost / ::1); use https off-loopback",
                                    s.id
                                ));
                        }
                    }
                    Err(e) => errors.push(format!("mcp server {:?}: {e}", s.id)),
                },
                _ => errors.push(format!(
                    "mcp server {:?}: http transport requires a url",
                    s.id
                )),
            },
        }
        if s.exposure == McpExposure::Gateway
            && !s.env_keys.is_empty()
            && s.cred_ref
                .as_deref()
                .map(str::trim)
                .map(|r| r.is_empty())
                .unwrap_or(true)
        {
            errors.push(format!(
                "mcp server {:?}: gateway-exposed server with env_keys must name a cred_ref (the credential itself may be missing — that is a runtime state, not a save error)",
                s.id
            ));
        }
    }
}

/// SearXNG settings validation (MCP design §6.3): only the enabled
/// state constrains anything — `api_base` must then be a non-empty
/// http(s) URL (checked with the same pragmatic parser as MCP http
/// servers). Unlike MCP http servers there is no https-off-loopback
/// rule: a SearXNG instance is operator infrastructure, bound to
/// loopback or the LAN side per §6.3, so plain http to a LAN address
/// is a legitimate configuration. Disabled carries no constraint at
/// all, so a half-typed URL can be saved with the toggle off.
fn validate_searxng(searxng: &SearxngSettings, errors: &mut Vec<String>) {
    if !searxng.enabled {
        return;
    }
    let api_base = searxng.api_base.trim();
    if api_base.is_empty() {
        errors.push(
            "mcp.searxng: api_base must not be empty while searxng is enabled".to_string(),
        );
        return;
    }
    if let Err(e) = parse_mcp_http_url(api_base) {
        errors.push(format!("mcp.searxng: api_base: {e}"));
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
    fn display_name_validates_and_defaults() {
        assert_eq!(
            validate_display_name("  Coder Next  ").unwrap(),
            "Coder Next"
        );
        assert!(validate_display_name("   ").is_err());
        assert!(validate_display_name(&"x".repeat(65)).is_err());
        assert_eq!(validate_display_name(&"x".repeat(64)).unwrap().len(), 64);
        // Embedded control characters (a newline would break out
        // of single-line labels and log lines) are rejected.
        assert!(validate_display_name("two\nlines").is_err());
        assert!(validate_display_name("tab\tinside").is_err());
        // A config written before the field existed loads with no
        // override (serde default), and the field stays out of the
        // serialized form while unset.
        let legacy = r#"{"id":"m","gguf_path":"models/m.gguf","quant":"Q4_K_M","params_b":1.0,"sha256":"","verified_at":null,"enabled":true}"#;
        let m: ModelConfig = serde_json::from_str(legacy).unwrap();
        assert_eq!(m.display_name, None);
        assert!(!serde_json::to_string(&m).unwrap().contains("display_name"));
        let renamed: ModelConfig = serde_json::from_str(
            r#"{"id":"m","gguf_path":"models/m.gguf","quant":"Q4_K_M","params_b":1.0,"sha256":"","verified_at":null,"enabled":true,"display_name":"Coder"}"#,
        )
        .unwrap();
        assert_eq!(renamed.display_name.as_deref(), Some("Coder"));
        // And validate() enforces the same rule on a loaded config:
        // an invalid stored name must not load and re-save cleanly.
        let mut m: ModelConfig = serde_json::from_str(legacy).unwrap();
        let mut cfg = AppConfig::default_config();
        m.display_name = Some("bad\nname".to_string());
        cfg.models = vec![m.clone()];
        assert!(validate(&cfg).is_err());
        m.display_name = Some(String::new());
        cfg.models = vec![m.clone()];
        assert!(validate(&cfg).is_err());
        m.display_name = Some("Fine Name".to_string());
        cfg.models = vec![m];
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn tool_call_probe_defaults_to_none_and_round_trips() {
        // A backend JSON written before the field existed loads
        // with no probe recorded (not measured).
        let legacy = r#"{"id":"planner","enabled":false,"model_file":"models/p.gguf","port":8081,"flags":{"n_ctx":32768,"n_batch":512,"tensor_split":[],"split_mode":"none"},"raw_flags":null,"restart_policy":{"max_retries":5,"backoff_base_secs":2},"overrides_global":false}"#;
        let b: BackendConfig = serde_json::from_str(legacy).unwrap();
        assert_eq!(b.tool_call_probe, None);
        // While unset the field stays out of the serialized form.
        assert!(!serde_json::to_string(&b)
            .unwrap()
            .contains("tool_call_probe"));
        // A recorded probe round-trips with its lowercase outcome.
        let probed: BackendConfig = serde_json::from_str(
            r#"{"id":"planner","enabled":false,"model_file":"models/p.gguf","port":8081,"flags":{"n_ctx":32768,"n_batch":512,"tensor_split":[],"split_mode":"none"},"raw_flags":null,"restart_policy":{"max_retries":5,"backoff_base_secs":2},"overrides_global":false,"tool_call_probe":{"outcome":"verified","at_ms":1700000000000}}"#,
        )
        .unwrap();
        assert_eq!(
            probed.tool_call_probe,
            Some(ToolCallProbe {
                outcome: ToolCallProbeOutcome::Verified,
                at_ms: 1_700_000_000_000,
            })
        );
        assert!(serde_json::to_string(&probed)
            .unwrap()
            .contains("\"outcome\":\"verified\""));
    }

    #[test]
    fn reset_stale_tool_call_probes_only_on_model_or_raw_change() {
        let probe = Some(ToolCallProbe {
            outcome: ToolCallProbeOutcome::Unparsed,
            at_ms: 42,
        });
        let mut previous = AppConfig::default_config();
        for b in previous.backends.iter_mut() {
            b.tool_call_probe = probe;
        }
        // Identical config: nothing resets.
        let mut current = previous.clone();
        assert!(!reset_stale_tool_call_probes(&previous, &mut current));
        assert!(current.backends.iter().all(|b| b.tool_call_probe == probe));
        // One backend's raw flags change, another's model file:
        // exactly those two probes reset.
        let mut current = previous.clone();
        current.backends[0].raw_flags = Some("--threads 8".to_string());
        current.backends[1].model_file = PathBuf::from("models/other.gguf");
        assert!(reset_stale_tool_call_probes(&previous, &mut current));
        assert_eq!(current.backends[0].tool_call_probe, None);
        assert_eq!(current.backends[1].tool_call_probe, None);
        assert_eq!(current.backends[2].tool_call_probe, probe);
        assert_eq!(current.backends[3].tool_call_probe, probe);
    }

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
            verified_sha256: None,
            source_url: String::new(),
            assigned_backends: Vec::new(),
            enabled: true,
            display_name: None,
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
            verified_sha256: None,
            source_url: "https://example.com/OldPlanner.gguf".to_string(),
            assigned_backends: vec!["planner".to_string()],
            enabled: true,
            display_name: None,
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
            verified_sha256: None,
            source_url: "https://example.com/OldCoder.gguf".to_string(),
            assigned_backends: vec!["coder".to_string()],
            enabled: true,
            display_name: None,
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

    #[test]
    fn refresh_applies_catalog_offload_only_without_operator_flags() {
        // The catalog default offload rides a definition refresh
        // as raw_flags (e.g. Nemotron's --n-cpu-moe 16): it lands
        // on a serving backend that has no raw flags of its own,
        // and never replaces an operator-changed offload.
        let build = |backend_raw: Option<&str>| {
            let mut cfg = AppConfig::default_config();
            cfg.models.push(ModelConfig {
                id: "nemotron-3.5-lightning-30b".to_string(),
                gguf_path: PathBuf::from("models/OldNemotron.gguf"),
                quant: "Q4_K_M".to_string(),
                params_b: 30.0,
                sha256: "b".repeat(64),
                verified_at: Some(Utc::now()),
                verified_sha256: None,
                source_url: "https://example.com/OldNemotron.gguf".to_string(),
                assigned_backends: vec!["coder".to_string()],
                enabled: true,
                display_name: None,
            });
            let coder = cfg.backends.iter_mut().find(|b| b.id == "coder").unwrap();
            coder.model_file = PathBuf::from("models/OldNemotron.gguf");
            coder.raw_flags = backend_raw.map(str::to_string);
            cfg
        };
        let refresh = CatalogDefinitionRefresh {
            entry: CatalogModelEntry {
                model_id: "nemotron-3.5-lightning-30b".to_string(),
                gguf_file_name: "NewNemotron.gguf".to_string(),
                quant: "Q4_K_M".to_string(),
                params_b: 31.6,
                sha256: "c".repeat(64),
                source_url: "https://example.com/NewNemotron.gguf".to_string(),
                assigned_backends: vec!["coder".to_string()],
                enabled: true,
            },
            backend_id: "coder".to_string(),
            clear_split: false,
            raw_flags: Some("--n-cpu-moe 16".to_string()),
        };
        // No operator flags: the catalog default offload applies.
        let mut cfg = build(None);
        assert!(refresh_catalog_definition(&mut cfg, &refresh));
        let coder = cfg.backends.iter().find(|b| b.id == "coder").unwrap();
        assert_eq!(coder.raw_flags.as_deref(), Some("--n-cpu-moe 16"));
        // Operator-changed offload: preserved verbatim.
        let mut cfg = build(Some("--n-cpu-moe 4"));
        assert!(refresh_catalog_definition(&mut cfg, &refresh));
        let coder = cfg.backends.iter().find(|b| b.id == "coder").unwrap();
        assert_eq!(coder.raw_flags.as_deref(), Some("--n-cpu-moe 4"));
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
            verified_sha256: None,
            source_url: "https://example.com/qwen3-8b.gguf".to_string(),
            assigned_backends: vec!["tool-runner".to_string()],
            enabled: true,
            display_name: None,
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

    fn overflow_rule(id: &str, target: &str) -> RoutingRule {
        RoutingRule {
            id: id.to_string(),
            description: "test rule".to_string(),
            enabled: true,
            kind: Some(RoutingRuleKind::ContextOverflow {
                target_group: target.to_string(),
                optimal_fraction: 0.5,
                optimal_tokens: None,
                output_reserve_tokens: 1024,
                advisory: true,
            }),
            litellm_fragment: None,
            source: "user".to_string(),
        }
    }

    #[test]
    fn context_overflow_rule_validates() {
        let mut cfg = working_config();
        cfg.gateway
            .routing_rules
            .push(overflow_rule("context-overflow", "tool-runner"));
        validate(&cfg).expect("an enabled rule with an existing target validates");
    }

    #[test]
    fn context_overflow_openrouter_target_rejected() {
        let mut cfg = working_config();
        cfg.gateway
            .routing_rules
            .push(overflow_rule("context-overflow", "openrouter"));
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("openrouter"), "unexpected: {err}");
    }

    #[test]
    fn context_overflow_unknown_target_rejected() {
        let mut cfg = working_config();
        cfg.gateway
            .routing_rules
            .push(overflow_rule("context-overflow", "no-such-group"));
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("does not exist"), "unexpected: {err}");
    }

    #[test]
    fn context_overflow_second_enabled_rule_rejected() {
        let mut cfg = working_config();
        cfg.gateway
            .routing_rules
            .push(overflow_rule("one", "tool-runner"));
        cfg.gateway
            .routing_rules
            .push(overflow_rule("two", "tool-runner"));
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("at most one enabled"), "unexpected: {err}");
        // A disabled second rule is inert and allowed.
        cfg.gateway.routing_rules[1].enabled = false;
        validate(&cfg).expect("one enabled + one disabled validates");
    }

    #[test]
    fn disabled_rule_target_is_not_validated() {
        // A disabled rule is inert: its target may dangle (e.g. the
        // group was deleted) without blocking unrelated saves.
        let mut cfg = working_config();
        let mut rule = overflow_rule("context-overflow", "deleted-group");
        rule.enabled = false;
        cfg.gateway.routing_rules.push(rule);
        validate(&cfg).expect("disabled rule with a dangling target validates");
    }

    #[test]
    fn context_overflow_fraction_range_checked() {
        for bad in [0.0, 1.0, f64::NAN, -0.5, 1.5] {
            let mut cfg = working_config();
            let mut rule = overflow_rule("context-overflow", "tool-runner");
            if let Some(RoutingRuleKind::ContextOverflow {
                optimal_fraction, ..
            }) = &mut rule.kind
            {
                *optimal_fraction = bad;
            }
            cfg.gateway.routing_rules.push(rule);
            let err = validate(&cfg).unwrap_err().to_string();
            assert!(err.contains("optimal_fraction"), "fraction {bad}: {err}");
        }
    }

    #[test]
    fn context_overflow_optimal_tokens_checked() {
        // 0 is not a usable threshold.
        let mut cfg = working_config();
        let mut rule = overflow_rule("context-overflow", "tool-runner");
        if let Some(RoutingRuleKind::ContextOverflow {
            optimal_tokens, ..
        }) = &mut rule.kind
        {
            *optimal_tokens = Some(0);
        }
        cfg.gateway.routing_rules.push(rule);
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("optimal_tokens"), "unexpected: {err}");

        // At/above the target's enabled members' configured context
        // (tool-runner: 32768) the threshold can never fire.
        let mut cfg = working_config();
        let mut rule = overflow_rule("context-overflow", "tool-runner");
        if let Some(RoutingRuleKind::ContextOverflow {
            optimal_tokens, ..
        }) = &mut rule.kind
        {
            *optimal_tokens = Some(32768);
        }
        cfg.gateway.routing_rules.push(rule);
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("hard window"), "unexpected: {err}");

        // Below it validates.
        let mut cfg = working_config();
        let mut rule = overflow_rule("context-overflow", "tool-runner");
        if let Some(RoutingRuleKind::ContextOverflow {
            optimal_tokens, ..
        }) = &mut rule.kind
        {
            *optimal_tokens = Some(16384);
        }
        cfg.gateway.routing_rules.push(rule);
        validate(&cfg).expect("optimal_tokens below the bound validates");
    }

    #[test]
    fn legacy_fragment_rule_still_loads() {
        // The pre-typed-rule shape: no `enabled`, no `kind`, the
        // fragment a plain value. It must deserialize as an inert
        // legacy rule (design §6.1) and validate.
        let raw = serde_json::json!({
            "id": "legacy",
            "description": "old fragment rule",
            "litellm_fragment": {"router_settings": {"num_retries": 2}},
            "source": "user"
        });
        let rule: RoutingRule = serde_json::from_value(raw).expect("legacy rule deserializes");
        assert!(rule.enabled, "absent enabled defaults to true");
        assert!(rule.kind.is_none());
        assert!(rule.litellm_fragment.is_some());
        let mut cfg = working_config();
        cfg.gateway.routing_rules.push(rule);
        validate(&cfg).expect("legacy rule validates");
    }

    #[test]
    fn context_overflow_serializes_with_kebab_type_tag() {
        let rule = overflow_rule("context-overflow", "large-context");
        let v = serde_json::to_value(&rule).expect("serialize");
        assert_eq!(v["kind"]["type"], "context-overflow");
        assert_eq!(v["kind"]["target_group"], "large-context");
        assert_eq!(v["kind"]["optimal_fraction"], 0.5);
        assert_eq!(v["kind"]["output_reserve_tokens"], 1024);
        assert_eq!(v["kind"]["advisory"], true);
        let back: RoutingRule = serde_json::from_value(v).expect("round trip");
        assert!(matches!(
            back.kind,
            Some(RoutingRuleKind::ContextOverflow { .. })
        ));
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
            verified_sha256: None,
            source_url: "https://example.com/q.gguf".to_string(),
            assigned_backends: Vec::new(),
            enabled: true,
            display_name: None,
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
            verified_sha256: None,
            source_url: "https://example.com/q.gguf".to_string(),
            assigned_backends: vec!["tool-runner".to_string()],
            enabled: true,
            display_name: None,
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

    fn mcp_server(id: &str) -> McpServerConfig {
        McpServerConfig {
            id: id.to_string(),
            display_name: id.to_string(),
            transport: McpTransport::Stdio,
            exposure: McpExposure::Local,
            command: Some("npx".to_string()),
            args: vec!["-y".to_string(), "pkg".to_string()],
            url: None,
            env_keys: Vec::new(),
            cred_ref: None,
            allowed_tools: None,
            metered: false,
            rpm_limit: None,
        }
    }

    fn cfg_with_mcp(servers: Vec<McpServerConfig>) -> AppConfig {
        let mut cfg = working_config();
        cfg.mcp = McpConfig {
            servers,
            searxng: SearxngSettings::default(),
        };
        cfg
    }

    #[test]
    fn mcp_seed_is_legacy_servers_with_unified_credentials() {
        let seed = seeded_mcp_servers();
        // Phase 3: the legacy pair followed by the curated pair.
        assert_eq!(seed.len(), 4);
        let gh = &seed[0];
        assert_eq!(gh.id, "github");
        assert_eq!(gh.display_name, "GitHub");
        assert_eq!(gh.exposure, McpExposure::Local);
        assert_eq!(gh.transport, McpTransport::Stdio);
        assert_eq!(gh.command.as_deref(), Some("npx"));
        assert_eq!(gh.args, vec!["-y", "@modelcontextprotocol/server-github"]);
        assert_eq!(gh.env_keys, vec!["GITHUB_PERSONAL_ACCESS_TOKEN"]);
        assert_eq!(gh.cred_ref.as_deref(), Some(GITHUB_MCP_CRED_REF));
        assert_eq!(GITHUB_MCP_CRED_REF, "local-llm-service-manager/github-mcp");
        let gd = &seed[1];
        assert_eq!(gd.id, "gdrive");
        assert_eq!(gd.cred_ref.as_deref(), Some(GDRIVE_MCP_CRED_REF));
        // Canonicalised env key, not the legacy divergent name.
        assert_eq!(gd.env_keys, vec!["GOOGLE_APPLICATION_CREDENTIALS"]);
        assert_eq!(gd.args, vec!["-y", "@modelcontextprotocol/server-gdrive"]);
        // default_config carries the seed.
        assert_eq!(AppConfig::default_config().mcp.servers, seed);
        validate(&cfg_with_mcp(seed)).expect("seed must validate");
    }

    #[test]
    fn mcp_curated_seed_entries_match_design() {
        let curated = curated_mcp_servers();
        assert_eq!(curated.len(), 2);
        let brave = &curated[0];
        assert_eq!(brave.id, "brave_search");
        assert_eq!(brave.display_name, "Brave Search");
        assert_eq!(brave.transport, McpTransport::Stdio);
        assert_eq!(brave.exposure, McpExposure::Gateway);
        assert_eq!(brave.command.as_deref(), Some("npx"));
        assert_eq!(
            brave.args,
            vec![
                "-y",
                "@brave/brave-search-mcp-server@2.1.4",
                "--transport",
                "stdio"
            ]
        );
        assert_eq!(brave.env_keys, vec!["BRAVE_API_KEY"]);
        assert_eq!(brave.cred_ref.as_deref(), Some(BRAVE_SEARCH_MCP_CRED_REF));
        assert_eq!(BRAVE_SEARCH_MCP_CRED_REF, "local-llm-service-manager/brave-search-mcp");
        assert_eq!(
            brave.allowed_tools.as_deref(),
            Some(&["brave_web_search".to_string(), "brave_llm_context".to_string()][..])
        );
        assert!(brave.metered);
        assert_eq!(brave.rpm_limit, Some(20));
        let fetch = &curated[1];
        assert_eq!(fetch.id, "web_fetch");
        assert_eq!(fetch.display_name, "Web Fetch");
        assert_eq!(fetch.transport, McpTransport::Stdio);
        assert_eq!(fetch.exposure, McpExposure::Disabled);
        assert_eq!(fetch.command.as_deref(), Some("uvx"));
        assert_eq!(fetch.args, vec!["mcp-server-fetch@2026.10.10"]);
        assert!(fetch.env_keys.is_empty());
        assert_eq!(fetch.cred_ref, None);
        assert_eq!(fetch.allowed_tools.as_deref(), Some(&["fetch".to_string()][..]));
        assert!(!fetch.metered);
        assert_eq!(fetch.rpm_limit, None);
        // A fresh default config carries all four entries and is
        // born with the curated-seeded marker set.
        let default = AppConfig::default_config();
        assert_eq!(default.mcp.servers.len(), 4);
        assert!(default.mcp_curated_seeded);
        assert_eq!(default.mcp.searxng, SearxngSettings::default());
        validate(&default).expect("default with curated seed must validate");
    }

    #[test]
    fn migrate_pre_phase3_config_gains_curated_exactly_once() {
        // A Phase 1/2-era file: an explicit mcp section holding only
        // the legacy pair, and NO mcp_curated_seeded key anywhere.
        let mut raw = serde_json::to_value(working_config()).unwrap();
        raw.as_object_mut().unwrap().remove("mcp_curated_seeded");
        raw["mcp"] = serde_json::json!({ "servers": [
            {
                "id": "github", "display_name": "GitHub", "transport": "stdio",
                "exposure": "local", "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-github"],
                "url": null, "env_keys": ["GITHUB_PERSONAL_ACCESS_TOKEN"],
                "cred_ref": "local-llm-service-manager/github-mcp",
                "allowed_tools": null, "metered": false, "rpm_limit": null
            },
            {
                "id": "gdrive", "display_name": "Google Drive", "transport": "stdio",
                "exposure": "local", "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-gdrive"],
                "url": null, "env_keys": ["GOOGLE_APPLICATION_CREDENTIALS"],
                "cred_ref": "local-llm-service-manager/gdrive-mcp",
                "allowed_tools": null, "metered": false, "rpm_limit": null
            }
        ]});
        let cfg = migrate(raw).expect("pre-Phase-3 config must migrate");
        let ids: Vec<&str> = cfg.mcp.servers.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["github", "gdrive", "brave_search", "web_fetch"]);
        assert!(cfg.mcp_curated_seeded);
        validate(&cfg).expect("migrated config validates");
        // Second load (of the now-Phase-3 shape): no duplication.
        let cfg2 = migrate(serde_json::to_value(&cfg).unwrap()).expect("second load");
        assert_eq!(cfg2.mcp.servers.len(), 4);
        assert!(cfg2.mcp_curated_seeded);
    }

    #[test]
    fn migrate_curated_entries_deleted_stay_deleted() {
        // A Phase 3 file whose operator deleted both curated entries:
        // the marker is present and set, so nothing is resurrected.
        let mut cfg = working_config();
        cfg.mcp.servers
            .retain(|s| s.id == "github" || s.id == "gdrive");
        assert_eq!(cfg.mcp.servers.len(), 2);
        assert!(cfg.mcp_curated_seeded);
        let back = migrate(serde_json::to_value(&cfg).unwrap()).expect("reload");
        let ids: Vec<&str> = back.mcp.servers.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["github", "gdrive"]);
        // An entirely empty registry with the marker set also stays empty.
        let mut cfg = working_config();
        cfg.mcp.servers.clear();
        let back = migrate(serde_json::to_value(&cfg).unwrap()).expect("reload empty");
        assert!(back.mcp.servers.is_empty());
    }

    #[test]
    fn searxng_settings_default_and_validation() {
        // Registry JSON written before the settings existed loads
        // with SearXNG disabled and an empty base.
        let mcp: McpConfig = serde_json::from_str(r#"{"servers":[]}"#).unwrap();
        assert_eq!(mcp.searxng, SearxngSettings::default());
        // Disabled: no constraint, even on a half-typed base.
        let mut cfg = cfg_with_mcp(vec![]);
        cfg.mcp.searxng = SearxngSettings {
            enabled: false,
            api_base: "not a url".to_string(),
        };
        validate(&cfg).expect("disabled searxng carries no constraint");
        // Enabled with an empty base is rejected.
        cfg.mcp.searxng = SearxngSettings {
            enabled: true,
            api_base: "  ".to_string(),
        };
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("searxng"), "{err}");
        // Enabled with a non-URL / wrong scheme is rejected.
        cfg.mcp.searxng.api_base = "ftp://example.com".to_string();
        assert!(validate(&cfg).is_err());
        cfg.mcp.searxng.api_base = "not-a-url".to_string();
        assert!(validate(&cfg).is_err());
        // Enabled with loopback http (the §6.3 self-host shape) and
        // with https both validate.
        cfg.mcp.searxng.api_base = "http://127.0.0.1:8080".to_string();
        validate(&cfg).expect("loopback http searxng validates");
        cfg.mcp.searxng.api_base = "https://search.example.com/".to_string();
        validate(&cfg).expect("https searxng validates");
    }

    #[test]
    fn mcp_partial_server_json_loads_with_defaults() {
        let s: McpServerConfig = serde_json::from_str(r#"{"id":"x"}"#).unwrap();
        assert_eq!(s.transport, McpTransport::Stdio);
        assert_eq!(s.exposure, McpExposure::Disabled);
        assert_eq!(s.command, None);
        assert!(s.args.is_empty());
        assert!(!s.metered);
    }

    #[test]
    fn mcp_id_rules() {
        for bad in ["", "GitHub", "1abc", "brave-search", "a b", "a.b", "_a"] {
            let err = validate(&cfg_with_mcp(vec![mcp_server(bad)]))
                .unwrap_err()
                .to_string();
            assert!(err.contains("must match"), "id {bad:?}: {err}");
        }
        for good in ["a", "brave_search", "a1", "web_fetch2"] {
            validate(&cfg_with_mcp(vec![mcp_server(good)])).expect("valid id");
        }
        // Duplicate.
        let err = validate(&cfg_with_mcp(vec![mcp_server("dup"), mcp_server("dup")]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicate mcp server id"), "{err}");
    }

    #[test]
    fn mcp_stdio_command_allowlist_and_windows_basename() {
        // Missing command.
        let mut s = mcp_server("x");
        s.command = None;
        assert!(validate(&cfg_with_mcp(vec![s.clone()])).is_err());
        // Not allowlisted.
        s.command = Some("powershell".to_string());
        assert!(validate(&cfg_with_mcp(vec![s.clone()])).is_err());
        // Windows-style path + .cmd suffix normalises to npx.
        s.command = Some(r"C:\tools\node\npx.cmd".to_string());
        validate(&cfg_with_mcp(vec![s.clone()])).expect("windows npx.cmd basename");
        // Unix path + .exe, upper case.
        s.command = Some("/usr/local/bin/UVX.EXE".to_string());
        validate(&cfg_with_mcp(vec![s.clone()])).expect("uvx.exe basename");
        for ok in ["npx", "uvx", "python", "python3", "node", "docker", "deno"] {
            s.command = Some(ok.to_string());
            validate(&cfg_with_mcp(vec![s.clone()])).expect("allowlisted");
        }
    }

    #[test]
    fn mcp_banned_args_rejected() {
        for banned in ["--allow-private-ips", "--ignore-robots-txt"] {
            let mut s = mcp_server("x");
            s.args.push(banned.to_string());
            let err = validate(&cfg_with_mcp(vec![s])).unwrap_err().to_string();
            assert!(err.contains(banned), "{err}");
            assert!(err.contains("rejected"), "{err}");
        }
        // `=value` form is also the banned token.
        let mut s = mcp_server("x");
        s.args.push("--allow-private-ips=true".to_string());
        assert!(validate(&cfg_with_mcp(vec![s])).is_err());
    }

    #[test]
    fn mcp_http_url_rules() {
        let http_server = |url: Option<&str>| McpServerConfig {
            transport: McpTransport::Http,
            command: None,
            url: url.map(str::to_string),
            ..mcp_server("remote")
        };
        // Missing url.
        assert!(validate(&cfg_with_mcp(vec![http_server(None)])).is_err());
        // Unparseable / wrong scheme.
        for bad in [
            "not-a-url",
            "ftp://example.com",
            "https://",
            "https://exa mple.com",
        ] {
            assert!(
                validate(&cfg_with_mcp(vec![http_server(Some(bad))])).is_err(),
                "{bad}"
            );
        }
        // https off-loopback is fine.
        validate(&cfg_with_mcp(vec![http_server(Some(
            "https://example.com/mcp",
        ))]))
        .expect("https");
        // http off-loopback is not.
        assert!(validate(&cfg_with_mcp(vec![http_server(Some("http://example.com"))])).is_err());
        // http on loopback is allowed, in all three spellings (+ port).
        for ok in [
            "http://127.0.0.1:8080",
            "http://localhost/mcp",
            "http://[::1]:8080/mcp",
        ] {
            validate(&cfg_with_mcp(vec![http_server(Some(ok))])).expect("loopback http");
        }
    }

    #[test]
    fn mcp_gateway_with_env_keys_requires_cred_ref() {
        let mut s = mcp_server("gw");
        s.exposure = McpExposure::Gateway;
        s.env_keys = vec!["SOME_KEY".to_string()];
        s.cred_ref = None;
        let err = validate(&cfg_with_mcp(vec![s.clone()]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cred_ref"), "{err}");
        s.cred_ref = Some("local-llm-service-manager/gw".to_string());
        // Credential itself missing is NOT a save error — only the ref.
        validate(&cfg_with_mcp(vec![s.clone()])).expect("cred_ref named");
        // Local with env keys and no cred_ref is allowed by this rule.
        s.exposure = McpExposure::Local;
        s.cred_ref = None;
        validate(&cfg_with_mcp(vec![s])).expect("local needs no cred_ref by §3.2");
        // Gateway with no env keys needs no cred_ref.
        let mut s2 = mcp_server("gw2");
        s2.exposure = McpExposure::Gateway;
        validate(&cfg_with_mcp(vec![s2])).expect("gateway without env keys");
    }

    #[test]
    fn migrate_without_mcp_key_seeds_registry() {
        // Pre-feature config: a full current config JSON with the mcp
        // key removed.
        let mut raw = serde_json::to_value(working_config()).unwrap();
        raw.as_object_mut().unwrap().remove("mcp");
        let cfg = migrate(raw).expect("pre-feature config must migrate");
        assert_eq!(cfg.mcp.servers, seeded_mcp_servers());
        validate(&cfg).expect("seeded config validates");
    }

    #[test]
    fn migrate_with_explicit_mcp_preserves_it_verbatim() {
        // Explicit empty registry is preserved — no resurrection.
        let mut raw = serde_json::to_value(working_config()).unwrap();
        raw["mcp"] = serde_json::json!({ "servers": [] });
        let cfg = migrate(raw).expect("explicit empty mcp");
        assert!(cfg.mcp.servers.is_empty());
        // Explicit custom registry is preserved.
        let mut raw = serde_json::to_value(working_config()).unwrap();
        raw["mcp"] = serde_json::json!({ "servers": [{
            "id": "custom", "display_name": "Custom", "transport": "http",
            "exposure": "disabled", "command": null, "args": [],
            "url": "https://example.com/mcp", "env_keys": [],
            "cred_ref": null, "allowed_tools": null, "metered": true,
            "rpm_limit": 7
        }]});
        let cfg = migrate(raw).expect("explicit custom mcp");
        assert_eq!(cfg.mcp.servers.len(), 1);
        assert_eq!(cfg.mcp.servers[0].id, "custom");
        assert_eq!(cfg.mcp.servers[0].rpm_limit, Some(7));
        assert!(cfg.mcp.servers[0].metered);
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
