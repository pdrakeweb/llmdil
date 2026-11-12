//! Serving config: backends, server flags, the gateway and its
//! groups / routing rules, the cloud tier, and registered models.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{default_gateway_bind_host, default_true, is_false};

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
    /// Advanced flags mode (tracker #60): while true, this
    /// backend's `raw_flags` are edited directly as text and are
    /// the single source of truth for its launch flags — the
    /// structured flags write paths (flags form, offload slider,
    /// MTP control, placement apply) are refused by the command
    /// layer, and the backend card presents the raw string rather
    /// than structured readouts. Editor state only: the flag
    /// itself changes nothing about a launch. Serde-defaulted and
    /// skipped when false, so configurations written before the
    /// field existed load — and re-serialize — unchanged.
    #[serde(default, skip_serializing_if = "is_false")]
    pub flags_advanced: bool,
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
        /// (validated in [`validate`](crate::validate())).
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
    /// Operator-declared MoE geometry for a model the curated
    /// catalog does not carry: the number of blocks that carry
    /// routed-expert tensors (`--n-cpu-moe` counts these layers).
    /// The catalog pins the same figures on its `ModelSpec`; a
    /// user-added model can only acquire expert-offload controls
    /// (the Edit Flags slider, the creation default) when the
    /// operator declares them here. `None` for dense models and
    /// for models whose capability was never declared. Catalog
    /// values win where a catalog entry exists — these fields are
    /// the fallback source, never an override of a catalog pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moe_layers: Option<u32>,
    /// The operator-declared default expert offload for a backend
    /// serving this model, in MoE layers — the declared
    /// counterpart of the catalog's `default_offload_layers`: the
    /// offload a newly created or newly assigned backend starts
    /// with, and the offload slider's green limit. Meaningful
    /// only alongside [`ModelConfig::moe_layers`]; validation
    /// requires it not exceed that count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_offload_layers: Option<u32>,
    /// Operator-declared MTP (multi-token prediction) capability
    /// for a model the curated catalog does not carry: whether the
    /// model's GGUF carries an MTP head llama.cpp's
    /// `--spec-type draft-mtp` can draft with. The catalog pins
    /// the same fact on its `ModelSpec`; a user-added model can
    /// only acquire the MTP draft control (backend Add/Edit,
    /// Edit Flags) when the operator declares it here. `None`
    /// when capability was never declared. A catalog pin wins
    /// where the catalog knows the model — this field is the
    /// fallback source, never an override of a catalog pin (see
    /// `manager_models::effective_mtp`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtp: Option<bool>,
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

/// Validate an operator MoE declaration for
/// [`ModelConfig::moe_layers`] / [`ModelConfig::default_offload_layers`]
/// as the write paths (model add, model offload-spec edit) accept
/// it: the pair is atomic — both fields or neither — the layer
/// count is positive, and the default does not exceed it. Returns
/// the error text on rejection. (Config validation is the lenient
/// backstop for hand-edited files; this is the strict rule the
/// forms and ops write by.)
pub fn validate_moe_declaration(
    moe_layers: Option<u32>,
    default_offload_layers: Option<u32>,
) -> Result<(), String> {
    match (moe_layers, default_offload_layers) {
        (None, None) => Ok(()),
        (Some(layers), Some(default)) => {
            if layers == 0 {
                return Err("MoE layer count must be greater than zero".to_string());
            }
            if default > layers {
                return Err(format!(
                    "default offload of {default} layers exceeds the model's {layers} MoE layers"
                ));
            }
            Ok(())
        }
        (Some(_), None) => Err(
            "a MoE declaration needs both the layer count and the default offload layers"
                .to_string(),
        ),
        (None, Some(_)) => {
            Err("a default offload requires the MoE layer count it counts against".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    #[test]
    fn moe_declaration_loads_legacy_and_validates_pairs() {
        // A config written before the declaration existed loads
        // with both fields absent, and they stay out of the
        // serialized form while unset.
        let legacy = r#"{"id":"m","gguf_path":"models/m.gguf","quant":"Q4_K_M","params_b":27.0,"sha256":"","verified_at":null,"enabled":true}"#;
        let m: ModelConfig = serde_json::from_str(legacy).unwrap();
        assert_eq!(m.moe_layers, None);
        assert_eq!(m.default_offload_layers, None);
        let ser = serde_json::to_string(&m).unwrap();
        assert!(!ser.contains("moe_layers"), "{ser}");
        assert!(!ser.contains("default_offload_layers"), "{ser}");
        // A declared pair round-trips.
        let declared: ModelConfig = serde_json::from_str(
            r#"{"id":"m","gguf_path":"models/m.gguf","quant":"Q4_K_M","params_b":27.0,"sha256":"","verified_at":null,"enabled":true,"moe_layers":48,"default_offload_layers":16}"#,
        )
        .unwrap();
        assert_eq!(declared.moe_layers, Some(48));
        assert_eq!(declared.default_offload_layers, Some(16));

        let cfg_with = |moe: Option<u32>, default: Option<u32>| {
            let mut m: ModelConfig = serde_json::from_str(legacy).unwrap();
            m.moe_layers = moe;
            m.default_offload_layers = default;
            let mut cfg = AppConfig::default_config();
            cfg.models = vec![m];
            cfg
        };
        // Consistent declarations validate: full pair, layers-only.
        assert!(validate(&cfg_with(Some(48), Some(48))).is_ok());
        assert!(validate(&cfg_with(Some(48), Some(0))).is_ok());
        assert!(validate(&cfg_with(Some(48), None)).is_ok());
        assert!(validate(&cfg_with(None, None)).is_ok());
        // Inconsistent ones do not: a default with no layer count,
        // a default past the count, a zero count.
        assert!(validate(&cfg_with(None, Some(16))).is_err());
        assert!(validate(&cfg_with(Some(48), Some(49))).is_err());
        assert!(validate(&cfg_with(Some(0), None)).is_err());
        assert!(validate(&cfg_with(Some(0), Some(0))).is_err());
    }

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
    fn flags_advanced_defaults_off_and_round_trips() {
        // A backend JSON written before the field existed loads
        // in structured mode (tracker #60).
        let legacy = r#"{"id":"planner","enabled":false,"model_file":"models/p.gguf","port":8081,"flags":{"n_ctx":32768,"n_batch":512,"tensor_split":[],"split_mode":"none"},"raw_flags":null,"restart_policy":{"max_retries":5,"backoff_base_secs":2},"overrides_global":false}"#;
        let b: BackendConfig = serde_json::from_str(legacy).unwrap();
        assert!(!b.flags_advanced);
        // While off the field stays out of the serialized form, so
        // a pre-field configuration re-serializes unchanged.
        assert!(!serde_json::to_string(&b)
            .unwrap()
            .contains("flags_advanced"));
        // On round-trips.
        let advanced: BackendConfig = serde_json::from_str(
            r#"{"id":"planner","enabled":false,"model_file":"models/p.gguf","port":8081,"flags":{"n_ctx":32768,"n_batch":512,"tensor_split":[],"split_mode":"none"},"raw_flags":"--parallel 1","restart_policy":{"max_retries":5,"backoff_base_secs":2},"overrides_global":false,"flags_advanced":true}"#,
        )
        .unwrap();
        assert!(advanced.flags_advanced);
        assert!(serde_json::to_string(&advanced)
            .unwrap()
            .contains("\"flags_advanced\":true"));
    }
}
