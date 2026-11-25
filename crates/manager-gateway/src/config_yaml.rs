//! LiteLLM `config.yaml` generation: model entries, MCP server emission,
//! router settings, and fallbacks.

use crate::*;
use manager_config::{AppConfig, McpExposure, McpTransport, ModelGroup, RoutingStrategy};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// LiteLLM model entries
// ---------------------------------------------------------------------------

/// A LiteLLM `model` entry generated from a [`ModelGroup`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LiteLlmModelEntry {
    pub model_name: String,
    pub litellm_params: LiteLlmParams,
}

/// The `litellm_params` block: OpenAI-compatible endpoint + routing knobs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LiteLlmParams {
    pub api_base: String,
    pub api_key: String,
    /// Backend-specific strategy hint consumed by the strategy picker.
    pub strategy: RoutingStrategy,
    /// LiteLLM provider/model selector, e.g. `openai/planner` or
    /// `openrouter/qwen/qwen3-235b-a22b`.
    pub model: String,
}

/// OpenRouter fallback target for a group.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenRouterTarget {
    pub enabled: bool,
    pub model_allowlist: Vec<String>,
    pub daily_cap_usd: f64,
}

/// Dummy API key sent to local llama-server backends (they ignore it;
/// LiteLLM requires a non-empty value).
pub const LOCAL_API_KEY: &str = "local";

/// Environment-variable reference LiteLLM resolves for the OpenRouter key.
/// The real key lives in Windows Credential Manager and is injected into the
/// LiteLLM process environment at spawn — never written to the YAML.
pub const OPENROUTER_API_KEY_ENV: &str = "os.environ/OPENROUTER_API_KEY";

/// Canonical alias for the OpenRouter fallback deployments.
pub const OPENROUTER_MODEL_NAME: &str = "openrouter";

// ---------------------------------------------------------------------------
// config.yaml generation
// ---------------------------------------------------------------------------

/// Build the `model` entries for one group (members + OpenRouter fallback
/// deployments when the group lists `openrouter` in its fallbacks).
pub fn model_entries_for_group(
    group: &ModelGroup,
    config: &AppConfig,
) -> Result<Vec<LiteLlmModelEntry>, GatewayError> {
    let mut entries = Vec::new();
    let backends: HashMap<&str, &manager_config::BackendConfig> =
        config.backends.iter().map(|b| (b.id.as_str(), b)).collect();

    // In backend-list order (external endpoints last): the member order
    // means nothing to LiteLLM, and moving a backend out of a group and
    // back must generate the same file.
    let mut members: Vec<&String> = group.members.iter().collect();
    members.sort_by_key(|m| {
        config
            .backends
            .iter()
            .position(|b| b.id == **m)
            .unwrap_or(usize::MAX)
    });
    for member in members {
        if let Some(backend) = backends.get(member.as_str()) {
            if !backend.enabled {
                continue;
            }
            entries.push(LiteLlmModelEntry {
                model_name: group.name.clone(),
                litellm_params: LiteLlmParams {
                    api_base: format!("http://127.0.0.1:{}/v1", backend.port),
                    api_key: LOCAL_API_KEY.to_string(),
                    strategy: group.strategy,
                    model: format!("openai/{}", backend.id),
                },
            });
        } else if let Some((host, port)) = parse_host_port(member) {
            entries.push(LiteLlmModelEntry {
                model_name: group.name.clone(),
                litellm_params: LiteLlmParams {
                    api_base: format!("http://{host}:{port}/v1"),
                    api_key: LOCAL_API_KEY.to_string(),
                    strategy: group.strategy,
                    model: format!("openai/{member}"),
                },
            });
        } else {
            return Err(GatewayError::ConfigInvalid(format!(
                "group '{}' references unknown backend '{member}'",
                group.name
            )));
        }
    }

    // WinML secondary registers under the tool-runner group (spec §6.5).
    if config.winml.enabled && group.name == "tool-runner" {
        entries.push(LiteLlmModelEntry {
            model_name: group.name.clone(),
            litellm_params: LiteLlmParams {
                api_base: format!("http://127.0.0.1:{}/v1", config.winml.port),
                api_key: LOCAL_API_KEY.to_string(),
                strategy: group.strategy,
                model: "openai/winml".to_string(),
            },
        });
    }

    // OpenRouter fallback deployments, one per allowlisted model. A
    // zero daily cap means "never spend" (spec §7: fail closed to
    // local): no deployments are generated, so no fallback row can
    // reference the cloud tier. LiteLLM's provider budget cannot
    // express this itself — it treats a zero budget as unlimited.
    if group.fallbacks.iter().any(|f| f == OPENROUTER_MODEL_NAME) {
        let cloud = &config.gateway.openrouter;
        if cloud.enabled && openrouter_cap_allows_spend(cloud.daily_cap_usd) {
            if cloud.model_allowlist.is_empty() {
                return Err(GatewayError::ConfigInvalid(
                    "openrouter fallback enabled but model_allowlist is empty".to_string(),
                ));
            }
            for model_id in &cloud.model_allowlist {
                entries.push(LiteLlmModelEntry {
                    model_name: OPENROUTER_MODEL_NAME.to_string(),
                    litellm_params: LiteLlmParams {
                        api_base: "https://openrouter.ai/api/v1".to_string(),
                        api_key: OPENROUTER_API_KEY_ENV.to_string(),
                        strategy: RoutingStrategy::SimpleShuffle,
                        model: format!("openrouter/{model_id}"),
                    },
                });
            }
        }
    }

    Ok(entries)
}

/// Parse a `host:port` member reference; `None` when not in that form.
pub(crate) fn parse_host_port(member: &str) -> Option<(&str, u16)> {
    let (host, port) = member.rsplit_once(':')?;
    if host.is_empty() {
        return None;
    }
    port.parse::<u16>().ok().map(|p| (host, p))
}

fn strategy_to_litellm(s: RoutingStrategy) -> &'static str {
    match s {
        RoutingStrategy::SimpleShuffle => "simple-shuffle",
        RoutingStrategy::LeastBusy => "least-busy",
        RoutingStrategy::LatencyBased => "latency-based-routing",
    }
}

fn strategy_from_litellm(s: &str) -> Option<RoutingStrategy> {
    [
        RoutingStrategy::SimpleShuffle,
        RoutingStrategy::LeastBusy,
        RoutingStrategy::LatencyBased,
    ]
    .into_iter()
    .find(|r| strategy_to_litellm(*r) == s)
}

/// Generate the full LiteLLM `config.yaml` from the app config.
///
/// The YAML is built with a deterministic string writer (stable key
/// order for the snapshot test). It is written to disk by the Setup
/// Wizard's start-gateway step each time the gateway is (re)started.
/// `router_settings.routing_strategy` is the first configured group's
/// strategy. Every other group with more than one deployment and a
/// different strategy gets its own `router_settings.routing_groups`
/// entry (LiteLLM >= [`ROUTING_GROUPS_MIN_VERSION`]); on an older pin
/// every group routes under the one global strategy
/// ([`applied_group_strategies`]). [`pick_backend`] serves only the
/// routing-trace explainer.
///
/// Groups are included when they produce deployments: enabled
/// groups (a group's enabled state is derived from its member
/// backends — [`group_enabled`] — never stored), plus groups whose
/// only deployments come from WinML or the OpenRouter fallback.
/// A group with no deployments at all is omitted entirely — its
/// (empty) deployment list and its router fallback rows alike.
pub fn generate_config_yaml(config: &AppConfig) -> Result<String, GatewayError> {
    if config.gateway.groups.is_empty() {
        return Err(GatewayError::ConfigInvalid(
            "no model groups defined".to_string(),
        ));
    }

    // Collect entries per group (validates member references for
    // every group, enabled or not), then keep the groups that
    // produce deployments. That is the enabled groups — plus the
    // two deployment sources that do not come from members: the
    // WinML deployment (registered on the tool-runner group
    // whenever WinML is enabled) and a group's OpenRouter fallback
    // deployments. A group whose members are all disabled but
    // which still yields one of those deployments stays in the
    // config; member-derived enablement alone would omit it and
    // make the deployment unreachable (a v0.1.32 regression — the
    // pre-derivation generator emitted any group whose entries
    // were non-empty).
    let group_entries = included_group_entries(config)?;
    if group_entries.is_empty() {
        return Err(GatewayError::ConfigInvalid(
            "no enabled model groups (every group's member backends are disabled)".to_string(),
        ));
    }

    let mut out = String::new();
    out.push_str("# Generated by Local LLM Service Manager — DO NOT HAND-EDIT.\n");
    out.push_str("# Regenerated from the app configuration whenever the gateway is (re)started.\n");

    // --- model_list ---
    out.push_str("model_list:\n");
    let multi_deployment = multi_deployment_groups(&group_entries);
    // Dedupe identical deployments (e.g. the shared OpenRouter fallback
    // entries are produced per group but must appear once).
    let mut emitted: std::collections::HashSet<(String, String, String)> =
        std::collections::HashSet::new();
    for (group, entries) in &group_entries {
        for e in entries {
            let key = (
                e.model_name.clone(),
                e.litellm_params.api_base.clone(),
                e.litellm_params.model.clone(),
            );
            if !emitted.insert(key) {
                continue;
            }
            out.push_str(&format!("  - model_name: {}\n", yaml_scalar(&e.model_name)));
            out.push_str("    litellm_params:\n");
            out.push_str(&format!(
                "      model: {}\n",
                yaml_scalar(&e.litellm_params.model)
            ));
            out.push_str(&format!(
                "      api_base: {}\n",
                yaml_scalar(&e.litellm_params.api_base)
            ));
            out.push_str(&format!(
                "      api_key: {}\n",
                yaml_scalar(&e.litellm_params.api_key)
            ));
            // Record the group's intended strategy alongside the entry so the
            // app's picker and the UI can reconcile it with LiteLLM's global
            // router_settings below. The group name is operator-settable
            // config interpolated into a `#` comment: sanitize it so a
            // name containing newlines cannot inject lines into the
            // generated file.
            if e.model_name == OPENROUTER_MODEL_NAME {
                out.push_str("      # shared OpenRouter fallback deployments\n");
            } else {
                out.push_str(&format!(
                    "      # intended routing strategy for group '{}': {}\n",
                    yaml_comment_text(&group.name),
                    strategy_to_litellm(e.litellm_params.strategy)
                ));
            }
            // Dead-member handling, only where another deployment can
            // take the request: one failure (a stopped member refuses
            // the connection) cools the deployment down, so the retry
            // and the following requests go to a healthy member. Set
            // per deployment: a router-wide `allowed_fails` also cools
            // a single-member group down on LiteLLM 1.105.
            if multi_deployment.contains(e.model_name.as_str()) {
                out.push_str("      # a failing member gets no traffic for this long\n");
                out.push_str(&format!("      cooldown_time: {MEMBER_COOLDOWN_SECS}\n"));
                out.push_str("    model_info:\n      allowed_fails: 0\n");
            }
        }
    }

    // --- router_settings (global; see module docs for the resolution rule) ---
    let global_strategy = resolve_global_strategy(config);
    out.push_str("router_settings:\n");
    out.push_str(&format!(
        "  routing_strategy: {}\n",
        strategy_to_litellm(global_strategy)
    ));
    if !multi_deployment.is_empty() {
        out.push_str(&format!("  num_retries: {ROUTER_RETRIES}\n"));
    }
    let own_strategy: Vec<&ModelGroup> = group_entries
        .iter()
        .map(|(g, _)| *g)
        .filter(|g| multi_deployment.contains(g.name.as_str()) && g.strategy != global_strategy)
        .collect();
    if routing_groups_supported(config) && !own_strategy.is_empty() {
        out.push_str("  routing_groups:\n");
        for g in own_strategy {
            // Named like its model group: LiteLLM then adds no extra
            // callable model name. Its `default` is reserved.
            let name = match g.name.as_str() {
                "default" => "llmdil-default",
                n => n,
            };
            out.push_str(&format!("    - group_name: {}\n", yaml_scalar(name)));
            out.push_str(&format!("      models: [{}]\n", yaml_scalar(&g.name)));
            out.push_str(&format!(
                "      routing_strategy: {}\n",
                strategy_to_litellm(g.strategy)
            ));
        }
    }
    // OpenRouter daily spend cap (spec §7 cost guardrail), enforced
    // natively by LiteLLM's provider budget limiter (verified against
    // 1.105.1: router_strategy/budget_limiter.py). It is a deployment
    // filter keyed by provider, so it caps the AGGREGATE spend of
    // every `openrouter/...` deployment; once the window's spend
    // reaches the limit the deployments are filtered out and a
    // fallback to the cloud tier fails, leaving the request on the
    // local error path (fail closed — never silently spend). Emitted
    // only when OpenRouter deployments exist, so a config without
    // the cloud tier stays byte-identical to the pre-cap generator.
    let has_openrouter = group_entries
        .iter()
        .flat_map(|(_, es)| es.iter())
        .any(|e| e.model_name == OPENROUTER_MODEL_NAME);
    if has_openrouter {
        out.push_str("  provider_budget_config:\n");
        out.push_str("    openrouter:\n");
        out.push_str(&format!(
            "      budget_limit: {}\n",
            yaml_number(config.gateway.openrouter.daily_cap_usd)
        ));
        out.push_str(&format!("      time_period: {OPENROUTER_BUDGET_PERIOD}\n"));
    }

    // --- litellm_settings ---
    out.push_str("litellm_settings:\n");
    // Prometheus metrics for the app dashboard (Pete approved
    // 2026-10-15): the callback mounts /metrics on the gateway port
    // and maintains the per-deployment success/failure counters the
    // app polls. Under `callbacks`, not `success_callback`: LiteLLM
    // feeds its failure metrics (deployment failures, failed proxy
    // requests) only to loggers registered for every event. This REQUIRES the prometheus-client package in the
    // gateway venv — LiteLLM fails to start when the callback is set
    // but the package is missing (verified against 1.105.1); the
    // wizard's install spec carries it.
    // Context-overflow routing rule (design §6.3): with a typed rule
    // enabled, register the generated callback module (written
    // beside this file as llmdil_routing.py at gateway start; the
    // proxy resolves the dotted path config-directory-relative) and
    // emit the native post-error safety net below. With no enabled
    // typed rule, neither key is emitted — the output is
    // byte-identical to the pre-feature generator (the kill switch
    // is structural, design §6.1).
    // The same module also carries the OpenRouter overflow
    // escalation (spec §7 trigger 2) when that is enabled, so the
    // callback is registered when either feature needs it.
    let routing_table = context_overflow_table(config);
    if routing_table.is_some() || overflow_escalation_table(config).is_some() {
        out.push_str("  callbacks: [prometheus, llmdil_routing.proxy_handler_instance]\n");
    } else {
        out.push_str("  callbacks: [prometheus]\n");
    }
    out.push_str("  fallbacks:\n");
    let known_names: Vec<&str> = group_entries
        .iter()
        .flat_map(|(_, es)| es.iter().map(|e| e.model_name.as_str()))
        .collect();
    for (group, _) in &group_entries {
        let chain: Vec<&str> = group
            .fallbacks
            .iter()
            .map(String::as_str)
            .filter(|f| {
                // Drop fallbacks with no deployments (e.g. openrouter when
                // the cloud tier is disabled); never emit dangling refs.
                known_names.contains(f) || *f == group.name.as_str()
            })
            .collect();
        if chain.is_empty() {
            continue;
        }
        let rendered = chain
            .iter()
            .map(|f| yaml_scalar(f))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "    - {}: [{}]\n",
            yaml_scalar(&group.name),
            rendered
        ));
    }
    // The context-overflow safety net (design §3.6/§6.3): one entry
    // per covered group → the rule's target. Covered = every group
    // in the generated config except the target itself (no entry
    // for the target — loop safety, design §9) and except the
    // OpenRouter pseudo-group (never a rule source or target).
    // Groups omitted from the generated config get no entry: a
    // fallback row for a group with no deployments would dangle.
    if let Some(table) = &routing_table {
        let covered: Vec<&ModelGroup> = group_entries
            .iter()
            .map(|(g, _)| *g)
            .filter(|g| g.name != table.target_group && g.name != OPENROUTER_MODEL_NAME)
            .collect();
        if !covered.is_empty() {
            out.push_str("  context_window_fallbacks:\n");
            for group in covered {
                out.push_str(&format!(
                    "    - {}: [{}]\n",
                    yaml_scalar(&group.name),
                    yaml_scalar(&table.target_group)
                ));
            }
        }
    }

    // --- general_settings ---
    // Current LiteLLM refuses to boot without a master key
    // (UnsafeMasterKeyError). The key itself is never written into
    // this file: it is generated by the app, stored in Windows
    // Credential Manager, and injected as LITELLM_MASTER_KEY into the
    // gateway process's environment at spawn; LiteLLM resolves the
    // os.environ/ reference at startup.
    out.push_str("general_settings:\n");
    out.push_str("  master_key: os.environ/LITELLM_MASTER_KEY\n");
    // MCP gateway servers registered in this config (design §4.1/§5.3).
    // This function is a pure function of the config it is given: the
    // caller (the start-gateway step) passes a filtered clone holding
    // only the ARMED Gateway servers, so every Gateway-exposed entry
    // present here is emitted. Local/Disabled entries never affect
    // generation. With no Gateway servers, nothing below is emitted
    // and the output is byte-identical to the pre-feature generator.
    let gateway_mcp: Vec<&manager_config::McpServerConfig> = config
        .mcp
        .servers
        .iter()
        .filter(|s| s.exposure == McpExposure::Gateway)
        .collect();
    if !gateway_mcp.is_empty() {
        // §5.3: a virtual key with no explicit MCP grant then gets no
        // MCP servers, instead of LiteLLM's open-by-default behaviour.
        out.push_str("  require_key_mcp_access_defined: true\n");
    }

    // --- mcp_servers (after general_settings, design §4.1) ---
    if !gateway_mcp.is_empty() {
        out.push_str("mcp_servers:\n");
        for s in &gateway_mcp {
            out.push_str(&format!("  {}:\n", yaml_scalar(&s.id)));
            let transport = match s.transport {
                McpTransport::Stdio => "stdio",
                McpTransport::Http => "http",
            };
            // Transport is always explicit: LiteLLM's YAML default is
            // http and its API default is sse, so omitting it is a
            // latent bug (§4.1).
            out.push_str(&format!("    transport: {}\n", yaml_scalar(transport)));
            match s.transport {
                McpTransport::Stdio => {
                    if let Some(command) = &s.command {
                        out.push_str(&format!("    command: {}\n", yaml_scalar(command)));
                    }
                    let args = s
                        .args
                        .iter()
                        .map(|a| yaml_scalar(a))
                        .collect::<Vec<_>>()
                        .join(", ");
                    out.push_str(&format!("    args: [{args}]\n"));
                }
                McpTransport::Http => {
                    if let Some(url) = &s.url {
                        out.push_str(&format!("    url: {}\n", yaml_scalar(url)));
                    }
                }
            }
            // Env block: secret-bearing keys first (always
            // os.environ/ references, never literals — §4.1/§5.4),
            // then any curated NON-secret literals for this server
            // (see `curated_env_literals`). The block is emitted when
            // either kind is present — web_fetch has no secret keys
            // but still needs its literal.
            let literals = curated_env_literals(s);
            if !s.env_keys.is_empty() || !literals.is_empty() {
                out.push_str("    env:\n");
                for key in &s.env_keys {
                    // Secret values are always os.environ/ references,
                    // never literals (§4.1/§5.4): the real values are
                    // resolved into the gateway spawn environment.
                    out.push_str(&format!(
                        "      {}: {}\n",
                        yaml_scalar(key),
                        yaml_scalar(&format!("os.environ/{key}"))
                    ));
                }
                for (key, value) in &literals {
                    out.push_str(&format!(
                        "      {}: {}\n",
                        yaml_scalar(key),
                        yaml_scalar(value)
                    ));
                }
            }
            // Not read from config at all (§4.1): every gateway MCP
            // server is private to this deployment.
            out.push_str("    available_on_public_internet: false\n");
            if let Some(tools) = &s.allowed_tools {
                let rendered = tools
                    .iter()
                    .map(|t| yaml_scalar(t))
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&format!("    allowed_tools: [{rendered}]\n"));
            }
        }
    }

    // --- search_tools (SearXNG, design §6.3) ---
    // SearXNG is LiteLLM's built-in search provider, NOT an MCP
    // server: its entry lives in a top-level `search_tools:` block
    // alongside (never inside) `mcp_servers:`, in this fixed
    // position after it. Shape verified against LiteLLM's SearXNG
    // search docs (docs.litellm.ai/docs/search/searxng, accessed
    // 2026-10-17): a list entry with `search_tool_name` and a
    // `litellm_params` block carrying `search_provider: searxng`
    // and the instance `api_base`. Emitted only when the provider
    // is enabled with a non-empty base; otherwise nothing is
    // emitted and the output is byte-identical to a config without
    // the settings.
    let searxng = &config.mcp.searxng;
    if searxng.enabled && !searxng.api_base.trim().is_empty() {
        out.push_str("search_tools:\n");
        out.push_str("  - search_tool_name: searxng-search\n");
        out.push_str("    litellm_params:\n");
        out.push_str("      search_provider: searxng\n");
        out.push_str(&format!(
            "      api_base: {}\n",
            yaml_scalar(searxng.api_base.trim())
        ));
    }

    Ok(out)
}

/// Non-secret curated env literals for one gateway MCP server,
/// keyed by curated server id (MCP design §3.3 / §6.2). These are
/// configuration values, not secrets, so — unlike the secret-bearing
/// `env_keys`, which §4.1 mandates as `os.environ/` references —
/// they are emitted as literal values in the server's `env:` block:
///
/// - `brave_search`: `BRAVE_MCP_ENABLED_TOOLS` is the server's own
///   tool-trimming mechanism (§3.3); its value is the server's
///   `allowed_tools` joined with `,`, so an operator edit to
///   `allowed_tools` moves the literal with it. When `allowed_tools`
///   is `None`, the design default pair
///   ([`manager_config::BRAVE_DEFAULT_ALLOWED_TOOLS`]) is used.
/// - `web_fetch`: `PYTHONIOENCODING: utf-8`, the Windows stdio note
///   from the fetch server's docs (§6.2).
///
/// Any other server id gets no literals.
fn curated_env_literals(server: &manager_config::McpServerConfig) -> Vec<(String, String)> {
    match server.id.as_str() {
        "brave_search" => {
            let tools: Vec<&str> = match &server.allowed_tools {
                Some(tools) => tools.iter().map(String::as_str).collect(),
                None => manager_config::BRAVE_DEFAULT_ALLOWED_TOOLS.to_vec(),
            };
            vec![("BRAVE_MCP_ENABLED_TOOLS".to_string(), tools.join(","))]
        }
        "web_fetch" => vec![("PYTHONIOENCODING".to_string(), "utf-8".to_string())],
        _ => Vec::new(),
    }
}

/// The groups the generated config includes, each with its
/// deployments, in configuration order: groups that produce
/// deployments (see [`generate_config_yaml`]). Member references are
/// validated for every group, enabled or not.
fn included_group_entries(
    config: &AppConfig,
) -> Result<Vec<(&ModelGroup, Vec<LiteLlmModelEntry>)>, GatewayError> {
    let mut group_entries = Vec::new();
    for group in &config.gateway.groups {
        let entries = model_entries_for_group(group, config)?;
        if group_enabled(group, config) || !entries.is_empty() {
            group_entries.push((group, entries));
        }
    }
    Ok(group_entries)
}

/// Retries of a failed request (LiteLLM's own default, stated so the
/// retry is guaranteed): one for the member that just failed plus one
/// in case its cooldown was not recorded yet.
const ROUTER_RETRIES: u32 = 2;

/// How long a failed member of a multi-deployment group gets no
/// traffic. Bounds both the cost of a stopped member (one slow
/// connection attempt per period) and how long a recovered group
/// whose members were ALL down keeps refusing requests.
const MEMBER_COOLDOWN_SECS: u32 = 15;

/// The first LiteLLM release (major, minor) with
/// `router_settings.routing_groups` (per-group strategies).
pub const ROUTING_GROUPS_MIN_VERSION: (u64, u64) = (1, 84);

/// Whether the LiteLLM version the gateway runs (the pin; the start
/// step installs exactly it) supports `routing_groups`. An
/// unparseable pin counts as unsupported.
pub fn routing_groups_supported(config: &AppConfig) -> bool {
    let pin = config
        .updates
        .components
        .get(manager_config::LITELLM_COMPONENT)
        .and_then(|c| c.pinned_version.as_deref())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(manager_config::DEFAULT_LITELLM_PIN);
    let mut parts = pin.trim_start_matches(['v', 'V']).split('.');
    let mut next = || parts.next().and_then(|p| p.parse::<u64>().ok());
    match (next(), next()) {
        (Some(major), Some(minor)) => (major, minor) >= ROUTING_GROUPS_MIN_VERSION,
        _ => false,
    }
}

/// Names of the model groups that generate more than one deployment
/// (the shared OpenRouter group excluded: its deployments are
/// different models, not replicas).
fn multi_deployment_groups<'a>(
    group_entries: &'a [(&'a ModelGroup, Vec<LiteLlmModelEntry>)],
) -> std::collections::HashSet<&'a str> {
    group_entries
        .iter()
        .filter(|(g, es)| es.iter().filter(|e| e.model_name == g.name).count() > 1)
        .map(|(g, _)| g.name.as_str())
        .filter(|n| *n != OPENROUTER_MODEL_NAME)
        .collect()
}

/// The default routing strategy the generated config applies
/// (`router_settings.routing_strategy`) to every group without its
/// own `routing_groups` entry. `None` when the generator would emit
/// no config: no included group, or a member reference it rejects.
/// Clients must not re-derive it.
pub fn effective_global_strategy(config: &AppConfig) -> Option<RoutingStrategy> {
    let group_entries = included_group_entries(config).ok()?;
    (!group_entries.is_empty()).then(|| resolve_global_strategy(config))
}

/// The strategy the generated config really applies to each included
/// group, in configuration order: the group's own where the gateway
/// supports per-group strategies, else the global one. Empty when no
/// config would be generated.
pub fn applied_group_strategies(config: &AppConfig) -> Vec<(String, RoutingStrategy)> {
    let supported = routing_groups_supported(config);
    let global = resolve_global_strategy(config);
    let group_entries = included_group_entries(config).unwrap_or_default();
    let multi = multi_deployment_groups(&group_entries);
    group_entries
        .iter()
        .map(|(g, _)| {
            let own = supported && multi.contains(g.name.as_str());
            (g.name.clone(), if own { g.strategy } else { global })
        })
        .collect()
}

/// The routing part of a generated (applied) `config.yaml`: what the
/// gateway started from that file serves.
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedRouting {
    /// `router_settings.routing_strategy`.
    pub strategy: Option<RoutingStrategy>,
    /// Each served model group and the strategy applied to it, in
    /// file order (the OpenRouter group excluded).
    pub groups: Vec<(String, RoutingStrategy)>,
}

/// Read [`AppliedRouting`] back from a generated `config.yaml`; `None`
/// for anything unparseable.
pub fn applied_routing(yaml: &str) -> Option<AppliedRouting> {
    let doc: serde_yaml::Value = serde_yaml::from_str(yaml).ok()?;
    let router = &doc["router_settings"];
    let strategy = router["routing_strategy"]
        .as_str()
        .and_then(strategy_from_litellm);
    let mut own: HashMap<&str, RoutingStrategy> = HashMap::new();
    for g in router["routing_groups"].as_sequence().into_iter().flatten() {
        let Some(s) = g["routing_strategy"]
            .as_str()
            .and_then(strategy_from_litellm)
        else {
            continue;
        };
        for m in g["models"].as_sequence().into_iter().flatten() {
            if let Some(m) = m.as_str() {
                own.insert(m, s);
            }
        }
    }
    let mut groups: Vec<(String, RoutingStrategy)> = Vec::new();
    for m in doc["model_list"].as_sequence()? {
        let Some(name) = m["model_name"].as_str() else {
            continue;
        };
        if name == OPENROUTER_MODEL_NAME || groups.iter().any(|(n, _)| n == name) {
            continue;
        }
        if let Some(s) = own.get(name).copied().or(strategy) {
            groups.push((name.to_string(), s));
        }
    }
    Some(AppliedRouting { strategy, groups })
}

/// The global `routing_strategy`: the first configured group's. A
/// group added later (they append) or toggled elsewhere never changes
/// it.
fn resolve_global_strategy(config: &AppConfig) -> RoutingStrategy {
    config
        .gateway
        .groups
        .first()
        .map(|g| g.strategy)
        .unwrap_or(RoutingStrategy::SimpleShuffle)
}

/// Sanitize text interpolated into a generated `#` comment: control
/// characters (newlines above all) become spaces, so operator-settable
/// values cannot break out of the comment and inject lines into the
/// generated config.
fn yaml_comment_text(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// LiteLLM's provider budget window for the OpenRouter cap. LiteLLM
/// opens the window at the first metered spend and resets it one day
/// later (not at local midnight); the app's own ledger, which drives
/// the Dashboard banner, resets at local midnight.
pub const OPENROUTER_BUDGET_PERIOD: &str = "1d";

/// Whether a daily cap permits any OpenRouter spend: a positive,
/// finite cap. Zero (or anything validation would refuse) disables
/// the cloud tier's deployments entirely.
pub fn openrouter_cap_allows_spend(daily_cap_usd: f64) -> bool {
    daily_cap_usd.is_finite() && daily_cap_usd > 0.0
}

/// Whether the generated config carries OpenRouter deployments at
/// all: tier enabled, a spend-permitting cap, and a model allowlist.
pub fn openrouter_tier_active(config: &AppConfig) -> bool {
    let cloud = &config.gateway.openrouter;
    cloud.enabled
        && openrouter_cap_allows_spend(cloud.daily_cap_usd)
        && !cloud.model_allowlist.is_empty()
}

/// Render a finite f64 as a YAML number (always with a decimal point
/// so it parses as a float).
fn yaml_number(v: f64) -> String {
    let s = format!("{v}");
    if s.contains('.') || s.contains('e') {
        s
    } else {
        format!("{s}.0")
    }
}

/// Minimal YAML scalar quoting: bare when safe, double-quoted otherwise.
fn yaml_scalar(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '@'))
        && !s.contains("://")
        && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic());
    if safe {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::*;

    #[test]
    fn yaml_snapshot_matches_checked_in_file() {
        let cfg = four_backend_config();
        let yaml = generate_config_yaml(&cfg).expect("generate");
        // The checked-in snapshot is stored with LF endings; a Windows
        // checkout with core.autocrlf rewrites it to CRLF on disk, so
        // normalize before comparing against the generated YAML.
        let expected = std::fs::read_to_string("tests/expected-config.yaml")
            .expect("read snapshot")
            .replace("\r\n", "\n");
        assert_eq!(yaml, expected, "generated YAML drifted from snapshot");
    }

    // --- OpenRouter daily spend cap (spec §7 cost guardrail) ---

    #[test]
    fn spend_cap_emits_litellm_provider_budget() {
        let cfg = four_backend_config();
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(
            yaml.contains(
                "router_settings:\n  routing_strategy: latency-based-routing\n  provider_budget_config:\n    openrouter:\n      budget_limit: 5.0\n      time_period: 1d\n"
            ),
            "{yaml}"
        );
        let mut cfg = four_backend_config();
        cfg.gateway.openrouter.daily_cap_usd = 0.25;
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(yaml.contains("      budget_limit: 0.25\n"), "{yaml}");
        // The budget is valid YAML LiteLLM reads as a float.
        let v: serde_yaml::Value = serde_yaml::from_str(&yaml).expect("yaml");
        assert_eq!(
            v["router_settings"]["provider_budget_config"]["openrouter"]["budget_limit"].as_f64(),
            Some(0.25)
        );
        assert_eq!(
            v["router_settings"]["provider_budget_config"]["openrouter"]["time_period"].as_str(),
            Some("1d")
        );
    }

    #[test]
    fn spend_cap_zero_disables_openrouter_entirely() {
        let mut cfg = four_backend_config();
        cfg.gateway.openrouter.daily_cap_usd = 0.0;
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(
            !yaml.contains("openrouter/"),
            "no cloud deployments:\n{yaml}"
        );
        assert!(!yaml.contains("provider_budget_config"), "{yaml}");
        // No fallback row may reference the missing cloud tier.
        assert!(!yaml.contains(", openrouter]"), "{yaml}");
        assert!(!yaml.contains("[openrouter]"), "{yaml}");
        assert!(!openrouter_tier_active(&cfg));
    }

    #[test]
    fn spend_cap_absent_when_tier_disabled() {
        let mut cfg = four_backend_config();
        cfg.gateway.openrouter.enabled = false;
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(!yaml.contains("provider_budget_config"), "{yaml}");
        assert!(!yaml.contains("openrouter"), "{yaml}");
    }

    #[test]
    fn spend_cap_predicates() {
        assert!(openrouter_cap_allows_spend(0.01));
        assert!(!openrouter_cap_allows_spend(0.0));
        assert!(!openrouter_cap_allows_spend(-1.0));
        assert!(!openrouter_cap_allows_spend(f64::NAN));
        assert!(!openrouter_cap_allows_spend(f64::INFINITY));
        assert_eq!(yaml_number(5.0), "5.0");
        assert_eq!(yaml_number(12.5), "12.5");
    }

    fn mcp_stdio_server() -> manager_config::McpServerConfig {
        manager_config::McpServerConfig {
            id: "brave_search".to_string(),
            display_name: "Brave Search".to_string(),
            transport: McpTransport::Stdio,
            exposure: McpExposure::Gateway,
            command: Some("npx".to_string()),
            args: vec![
                "-y".to_string(),
                "@brave/brave-search-mcp-server@2.0.0".to_string(),
                "--transport".to_string(),
                "stdio".to_string(),
            ],
            url: None,
            env_keys: vec!["BRAVE_API_KEY".to_string()],
            cred_ref: Some("local-llm-service-manager/brave-search-mcp".to_string()),
            allowed_tools: Some(vec![
                "brave_web_search".to_string(),
                "brave_llm_context".to_string(),
            ]),
            metered: true,
            rpm_limit: Some(20),
            signup_url: None,
            credential_hint: None,
        }
    }

    fn mcp_http_server() -> manager_config::McpServerConfig {
        manager_config::McpServerConfig {
            id: "remote_tools".to_string(),
            display_name: "Remote Tools".to_string(),
            transport: McpTransport::Http,
            exposure: McpExposure::Gateway,
            command: None,
            args: vec![],
            url: Some("https://mcp.example.com/mcp".to_string()),
            env_keys: vec![],
            cred_ref: None,
            signup_url: None,
            credential_hint: None,
            allowed_tools: None,
            metered: false,
            rpm_limit: None,
        }
    }

    #[test]
    fn yaml_with_no_gateway_mcp_servers_is_unchanged() {
        // The default/four-backend fixture's registry holds only the
        // seeded Local servers: no mcp_servers block, no require-key
        // flag (the snapshot test above pins the exact bytes).
        let cfg = four_backend_config();
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(!yaml.contains("mcp_servers"), "{yaml}");
        assert!(!yaml.contains("require_key_mcp_access_defined"), "{yaml}");
        // A Disabled gateway-shaped entry likewise emits nothing.
        let mut cfg = four_backend_config();
        let mut disabled = mcp_stdio_server();
        disabled.exposure = McpExposure::Disabled;
        cfg.mcp.servers.push(disabled);
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(!yaml.contains("mcp_servers"), "{yaml}");
    }

    #[test]
    fn yaml_emits_mcp_stdio_server_with_env_refs_and_allowed_tools() {
        let mut cfg = four_backend_config();
        cfg.mcp.servers.push(mcp_stdio_server());
        let yaml = generate_config_yaml(&cfg).expect("generate");
        // require_key flag inside general_settings (§5.3).
        assert!(
            yaml.contains("  require_key_mcp_access_defined: true\n"),
            "{yaml}"
        );
        // The block comes after general_settings.
        let general_at = yaml.find("general_settings:").unwrap();
        let mcp_at = yaml.find("mcp_servers:").unwrap();
        assert!(general_at < mcp_at, "{yaml}");
        assert!(yaml.contains("  brave_search:\n"), "{yaml}");
        assert!(yaml.contains("    transport: stdio\n"), "{yaml}");
        assert!(yaml.contains("    command: npx\n"), "{yaml}");
        assert!(
            yaml.contains(
                "    args: [\"-y\", \"@brave/brave-search-mcp-server@2.0.0\", \"--transport\", stdio]\n"
            ),
            "{yaml}"
        );
        // Env values are os.environ/ references, never literals, and
        // the credential target name never appears in the YAML.
        assert!(
            yaml.contains("      BRAVE_API_KEY: os.environ/BRAVE_API_KEY\n"),
            "{yaml}"
        );
        assert!(!yaml.contains("brave-search-mcp\""), "{yaml}");
        assert!(
            yaml.contains("    available_on_public_internet: false\n"),
            "{yaml}"
        );
        assert!(
            yaml.contains("    allowed_tools: [brave_web_search, brave_llm_context]\n"),
            "{yaml}"
        );
    }

    #[test]
    fn yaml_emits_mcp_http_server_with_url_and_no_command() {
        let mut cfg = four_backend_config();
        cfg.mcp.servers.push(mcp_http_server());
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(yaml.contains("  remote_tools:\n"), "{yaml}");
        assert!(yaml.contains("    transport: http\n"), "{yaml}");
        assert!(
            yaml.contains("    url: \"https://mcp.example.com/mcp\"\n"),
            "{yaml}"
        );
        // No stdio fields, no env block, no allowed_tools for this entry.
        let section = yaml.split("  remote_tools:\n").nth(1).unwrap();
        assert!(!section.contains("command:"), "{section}");
        assert!(!section.contains("allowed_tools:"), "{section}");
        assert!(
            section.contains("available_on_public_internet: false"),
            "{section}"
        );
    }

    #[test]
    fn yaml_emits_mixed_gateway_servers_in_config_order() {
        let mut cfg = four_backend_config();
        cfg.mcp.servers.push(mcp_http_server());
        cfg.mcp.servers.push(mcp_stdio_server());
        let yaml = generate_config_yaml(&cfg).expect("generate");
        let http_at = yaml.find("  remote_tools:\n").unwrap();
        let stdio_at = yaml.find("  brave_search:\n").unwrap();
        assert!(http_at < stdio_at, "config order preserved:\n{yaml}");
        // Exactly one require_key flag for the whole set.
        assert_eq!(
            yaml.matches("require_key_mcp_access_defined").count(),
            1,
            "{yaml}"
        );
    }

    fn web_fetch_gateway_server() -> manager_config::McpServerConfig {
        manager_config::McpServerConfig {
            id: "web_fetch".to_string(),
            display_name: "Web Fetch".to_string(),
            transport: McpTransport::Stdio,
            exposure: McpExposure::Gateway,
            command: Some("uvx".to_string()),
            args: vec!["mcp-server-fetch@2026.10.10".to_string()],
            url: None,
            env_keys: vec![],
            cred_ref: None,
            signup_url: None,
            credential_hint: None,
            allowed_tools: Some(vec!["fetch".to_string()]),
            metered: false,
            rpm_limit: None,
        }
    }

    #[test]
    fn yaml_brave_env_carries_secret_ref_and_tools_literal() {
        let mut cfg = four_backend_config();
        cfg.mcp.servers.push(mcp_stdio_server());
        let yaml = generate_config_yaml(&cfg).expect("generate");
        // Secret stays an os.environ/ reference...
        assert!(
            yaml.contains("      BRAVE_API_KEY: os.environ/BRAVE_API_KEY\n"),
            "{yaml}"
        );
        // ...and the non-secret trimming literal rides the same env
        // block, derived from allowed_tools (§3.3).
        assert!(
            yaml.contains(
                "      BRAVE_MCP_ENABLED_TOOLS: \"brave_web_search,brave_llm_context\"\n"
            ),
            "{yaml}"
        );
        // rpm_limit is never emitted into the YAML (§5.5 honesty:
        // LiteLLM's mcp_rpm_limit is a key/team property).
        assert!(!yaml.contains("rpm"), "{yaml}");
    }

    #[test]
    fn yaml_brave_tools_literal_tracks_allowed_tools_edits() {
        // Edited allowed_tools: the literal follows.
        let mut cfg = four_backend_config();
        let mut server = mcp_stdio_server();
        server.allowed_tools = Some(vec!["brave_web_search".to_string()]);
        cfg.mcp.servers.push(server);
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(
            yaml.contains("      BRAVE_MCP_ENABLED_TOOLS: brave_web_search\n"),
            "{yaml}"
        );
        // allowed_tools None: the literal falls back to the design
        // default pair, and no allowed_tools line is emitted.
        let mut cfg = four_backend_config();
        let mut server = mcp_stdio_server();
        server.allowed_tools = None;
        cfg.mcp.servers.push(server);
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(
            yaml.contains(
                "      BRAVE_MCP_ENABLED_TOOLS: \"brave_web_search,brave_llm_context\"\n"
            ),
            "{yaml}"
        );
        assert!(!yaml.contains("allowed_tools:"), "{yaml}");
    }

    #[test]
    fn yaml_web_fetch_env_carries_pythonioencoding_literal() {
        let mut cfg = four_backend_config();
        cfg.mcp.servers.push(web_fetch_gateway_server());
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(yaml.contains("  web_fetch:\n"), "{yaml}");
        // The env block exists despite web_fetch having no secret
        // env keys, and carries only the §6.2 literal.
        assert!(yaml.contains("      PYTHONIOENCODING: utf-8\n"), "{yaml}");
        assert!(!yaml.contains("os.environ/PYTHONIOENCODING"), "{yaml}");
        // The seeded Disabled form is absent entirely (zero-Gateway
        // output stays byte-identical — the snapshot test's domain).
        let mut cfg = four_backend_config();
        let mut disabled = web_fetch_gateway_server();
        disabled.exposure = McpExposure::Disabled;
        cfg.mcp.servers.push(disabled);
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(!yaml.contains("web_fetch"), "{yaml}");
        assert!(!yaml.contains("PYTHONIOENCODING"), "{yaml}");
    }

    #[test]
    fn yaml_non_curated_servers_get_no_literals() {
        let mut cfg = four_backend_config();
        let mut custom = mcp_stdio_server();
        custom.id = "custom_search".to_string();
        cfg.mcp.servers.push(custom);
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(yaml.contains("  custom_search:\n"), "{yaml}");
        assert!(!yaml.contains("BRAVE_MCP_ENABLED_TOOLS"), "{yaml}");
        assert!(!yaml.contains("PYTHONIOENCODING"), "{yaml}");
    }

    #[test]
    fn yaml_searxng_search_tools_emitted_only_when_enabled() {
        // Disabled (the default): no block, output unchanged.
        let cfg = four_backend_config();
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(!yaml.contains("search_tools"), "{yaml}");
        // Enabled but with an empty base: still nothing (validation
        // rejects this state at save; generation stays total).
        let mut cfg = four_backend_config();
        cfg.mcp.searxng.enabled = true;
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(!yaml.contains("search_tools"), "{yaml}");
        // Enabled with a base: the documented shape, top-level,
        // after mcp_servers when both are present.
        let mut cfg = four_backend_config();
        cfg.mcp.searxng = manager_config::SearxngSettings {
            enabled: true,
            api_base: "http://127.0.0.1:8080".to_string(),
        };
        cfg.mcp.servers.push(mcp_stdio_server());
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(
            yaml.contains(
                "search_tools:\n  - search_tool_name: searxng-search\n    litellm_params:\n      search_provider: searxng\n      api_base: \"http://127.0.0.1:8080\"\n"
            ),
            "{yaml}"
        );
        let mcp_at = yaml.find("mcp_servers:").unwrap();
        let search_at = yaml.find("search_tools:").unwrap();
        assert!(mcp_at < search_at, "{yaml}");
    }

    #[test]
    fn yaml_group_name_cannot_inject_comment_lines() {
        // Group names are operator-settable and land in a `#` comment;
        // an embedded newline must not inject lines into the file.
        let mut cfg = four_backend_config();
        cfg.gateway.groups[0].name = "planner\nmalicious: true".to_string();
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(
            !yaml.lines().any(|l| l == "malicious: true"),
            "injected line present:\n{yaml}"
        );
        assert!(yaml.contains("group 'planner malicious: true'"));
    }

    #[test]
    fn yaml_contains_all_backends_groups_and_fallbacks() {
        let cfg = four_backend_config();
        let yaml = generate_config_yaml(&cfg).expect("generate");
        for port in [8081, 8082, 8083, 8084] {
            assert!(
                yaml.contains(&format!("http://127.0.0.1:{port}/v1")),
                "missing backend :{port}"
            );
        }
        for name in [
            "planner",
            "coder-fast",
            "coder",
            "tool-runner",
            "openrouter",
        ] {
            assert!(
                yaml.contains(&format!("model_name: {name}")),
                "missing {name}"
            );
        }
        assert!(yaml.contains("routing_strategy: latency-based-routing"));
        assert!(yaml.contains("- planner: [coder, openrouter]"));
        assert!(yaml.contains("- coder-fast: [coder, openrouter]"));
        assert!(yaml.contains("- coder: [openrouter]"));
        assert!(yaml.contains("- tool-runner: [openrouter]"));
        assert!(yaml.contains("os.environ/OPENROUTER_API_KEY"));
        assert!(!yaml.contains("sk-"), "no real secrets in generated YAML");
    }

    #[test]
    fn yaml_drops_openrouter_when_tier_disabled() {
        let mut cfg = four_backend_config();
        cfg.gateway.openrouter.enabled = false;
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(!yaml.contains("openrouter.ai"));
        assert!(!yaml.contains("model_name: openrouter"));
        // Fallback chains must not dangle.
        assert!(!yaml.contains("openrouter]"));
        assert!(!yaml.contains(", openrouter"));
    }

    #[test]
    fn yaml_omits_group_with_all_members_disabled() {
        let mut cfg = four_backend_config();
        cfg.backends
            .iter_mut()
            .find(|b| b.id == "coder")
            .unwrap()
            .enabled = false;
        // The coder group must have no deployments at all to be
        // omitted: strip its OpenRouter fallback (a group whose
        // fallback deployments survive is kept — see
        // yaml_keeps_openrouter_only_group_when_members_disabled).
        cfg.gateway
            .groups
            .iter_mut()
            .find(|g| g.name == "coder")
            .unwrap()
            .fallbacks = vec![];
        let yaml = generate_config_yaml(&cfg).expect("generate");
        // No deployment and no fallback row for the disabled group.
        assert!(
            !yaml.lines().any(|l| l.trim() == "- model_name: coder"),
            "yaml:\n{yaml}"
        );
        assert!(!yaml.contains("127.0.0.1:8082"), "yaml:\n{yaml}");
        assert!(
            !yaml.lines().any(|l| l.trim().starts_with("- coder:")),
            "yaml:\n{yaml}"
        );
        // Fallback chains naming it drop the reference instead of
        // leaving it dangling.
        assert!(yaml.contains("- planner: [openrouter]"), "yaml:\n{yaml}");
        assert!(yaml.contains("- coder-fast: [openrouter]"), "yaml:\n{yaml}");
        // Remaining groups are generated as before.
        assert!(yaml.contains("model_name: planner"));
        assert!(yaml.contains("http://127.0.0.1:8081/v1"));
    }

    #[test]
    fn yaml_mixed_group_includes_only_enabled_members() {
        let mut cfg = four_backend_config();
        let mut coder_b = backend("coder-b", 8086);
        coder_b.enabled = false;
        cfg.backends.push(coder_b);
        cfg.gateway.groups.push(ModelGroup {
            name: "pair".to_string(),
            members: vec!["coder".to_string(), "coder-b".to_string()],
            strategy: RoutingStrategy::LeastBusy,
            fallbacks: vec![],
            purpose: String::new(),
        });
        let pair = cfg
            .gateway
            .groups
            .iter()
            .find(|g| g.name == "pair")
            .unwrap();
        assert!(group_enabled(pair, &cfg));
        assert_eq!(group_enabled_member_count(pair, &cfg), 1);
        let yaml = generate_config_yaml(&cfg).expect("generate");
        // The group is enabled through its one enabled member: present,
        // with only that member's deployment.
        assert!(yaml.contains("model_name: pair"), "yaml:\n{yaml}");
        assert!(!yaml.contains("8086"), "disabled member leaked:\n{yaml}");
    }

    #[test]
    fn yaml_errors_when_every_group_is_disabled() {
        let mut cfg = four_backend_config();
        for b in &mut cfg.backends {
            b.enabled = false;
        }
        // The cloud tier off as well: with it on, the groups'
        // OpenRouter fallback deployments would still generate
        // (yaml_keeps_openrouter_only_group_when_members_disabled).
        cfg.gateway.openrouter.enabled = false;
        let err = generate_config_yaml(&cfg).unwrap_err();
        assert!(matches!(err, GatewayError::ConfigInvalid(_)));
        assert!(err.to_string().contains("no enabled model groups"), "{err}");
    }

    #[test]
    fn yaml_rejects_unknown_backend_member() {
        let mut cfg = four_backend_config();
        cfg.gateway.groups[0].members = vec!["nope".to_string()];
        let err = generate_config_yaml(&cfg).unwrap_err();
        assert!(matches!(err, GatewayError::ConfigInvalid(_)));
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn yaml_rejects_empty_groups() {
        let mut cfg = four_backend_config();
        cfg.gateway.groups.clear();
        assert!(matches!(
            generate_config_yaml(&cfg).unwrap_err(),
            GatewayError::ConfigInvalid(_)
        ));
    }

    #[test]
    fn yaml_rejects_openrouter_enabled_with_empty_allowlist() {
        let mut cfg = four_backend_config();
        cfg.gateway.openrouter.model_allowlist.clear();
        assert!(matches!(
            generate_config_yaml(&cfg).unwrap_err(),
            GatewayError::ConfigInvalid(_)
        ));
    }

    #[test]
    fn yaml_accepts_host_port_member_refs() {
        let mut cfg = four_backend_config();
        cfg.gateway.groups.push(ModelGroup {
            name: "aux".to_string(),
            members: vec!["127.0.0.1:8099".to_string()],
            strategy: RoutingStrategy::SimpleShuffle,
            fallbacks: vec![],
            purpose: String::new(),
        });
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(yaml.contains("http://127.0.0.1:8099/v1"));
    }

    #[test]
    fn yaml_keeps_winml_group_when_its_member_backend_is_disabled() {
        // WinML registers under tool-runner whenever WinML is
        // enabled; the deployment does not come from the group's
        // member backend, so disabling that backend must not drop
        // the group (and WinML with it) from the generated config.
        let mut cfg = four_backend_config();
        for b in cfg.backends.iter_mut() {
            if b.id == "tool-runner" {
                b.enabled = false;
            }
        }
        cfg.winml.enabled = true;
        cfg.winml.port = 8090;
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(
            yaml.contains("http://127.0.0.1:8090/v1"),
            "winml deployment survives its group having no enabled member:\n{yaml}"
        );
    }

    #[test]
    fn yaml_keeps_openrouter_only_group_when_members_disabled() {
        // A group whose only deployments are its OpenRouter
        // fallback entries is the only group: generation must not
        // fail ConfigInvalid just because no member is enabled.
        let mut cfg = four_backend_config();
        for b in cfg.backends.iter_mut() {
            b.enabled = false;
        }
        cfg.gateway.groups = vec![ModelGroup {
            name: "cloud".to_string(),
            members: vec!["coder".to_string()],
            strategy: RoutingStrategy::SimpleShuffle,
            fallbacks: vec!["openrouter".to_string()],
            purpose: String::new(),
        }];
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(yaml.contains("openrouter.ai"), "{yaml}");
        // With the cloud tier off too, the group has no deployments
        // at all and generation fails closed, as before.
        cfg.gateway.openrouter.enabled = false;
        assert!(matches!(
            generate_config_yaml(&cfg),
            Err(GatewayError::ConfigInvalid(_))
        ));
    }

    /// Two groups with two live members each and different strategies.
    fn two_pair_config() -> AppConfig {
        let mut cfg = four_backend_config();
        cfg.gateway.groups = vec![
            group("a", RoutingStrategy::SimpleShuffle, &["planner", "coder"]),
            group(
                "b",
                RoutingStrategy::LeastBusy,
                &["coder-fast", "tool-runner"],
            ),
        ];
        cfg
    }

    /// A14: each multi-member group is routed under its own strategy.
    #[test]
    fn per_group_strategies_use_routing_groups() {
        let mut cfg = two_pair_config();
        cfg.gateway.groups[1].name = "default".to_string();
        let yaml = generate_config_yaml(&cfg).expect("generate");
        let v: serde_yaml::Value = serde_yaml::from_str(&yaml).expect("yaml");
        assert_eq!(v["router_settings"]["routing_strategy"], "simple-shuffle");
        let rg = &v["router_settings"]["routing_groups"];
        assert_eq!(rg.as_sequence().unwrap().len(), 1, "{yaml}");
        // LiteLLM reserves the group name `default`.
        assert_eq!(rg[0]["group_name"], "llmdil-default");
        assert_eq!(rg[0]["models"][0], "default");
        assert_eq!(rg[0]["routing_strategy"], "least-busy");
        let expect = vec![
            ("a".to_string(), RoutingStrategy::SimpleShuffle),
            ("default".to_string(), RoutingStrategy::LeastBusy),
        ];
        assert_eq!(applied_group_strategies(&cfg), expect);
        let applied = applied_routing(&yaml).expect("parse back");
        assert_eq!(applied.groups, expect);
        assert_eq!(applied.strategy, Some(RoutingStrategy::SimpleShuffle));
    }

    /// A14: adding an unrelated group changes no other group's
    /// strategy — with per-group support and on an older pin.
    #[test]
    fn adding_a_group_keeps_the_other_groups_strategies() {
        for pin in ["1.84.0", "1.83.7"] {
            let mut cfg = four_backend_config();
            manager_config::backfill_litellm_pin(&mut cfg);
            cfg.updates
                .components
                .get_mut(manager_config::LITELLM_COMPONENT)
                .unwrap()
                .pinned_version = Some(pin.to_string());
            cfg.gateway.groups = vec![
                group("solo", RoutingStrategy::LatencyBased, &["planner"]),
                group("late", RoutingStrategy::SimpleShuffle, &["coder"]),
            ];
            let before = applied_group_strategies(&cfg);
            cfg.gateway.groups.push(group(
                "new",
                RoutingStrategy::LeastBusy,
                &["coder-fast", "tool-runner"],
            ));
            let after = applied_group_strategies(&cfg);
            assert_eq!(&after[..2], &before[..], "{pin}");
            let yaml = generate_config_yaml(&cfg).expect("generate");
            assert_eq!(yaml.contains("routing_groups:"), pin == "1.84.0", "{yaml}");
            assert_eq!(applied_routing(&yaml).unwrap().groups, after);
        }
    }

    /// B1: moving a member out and back is no change.
    #[test]
    fn member_order_does_not_change_the_generated_config() {
        let cfg = two_pair_config();
        let mut moved = cfg.clone();
        moved.gateway.groups[0].members.reverse();
        assert_eq!(
            generate_config_yaml(&cfg).unwrap(),
            generate_config_yaml(&moved).unwrap()
        );
    }

    /// A2: a multi-member group leaves a failing member out and
    /// retries on another; a single-member group is unchanged.
    #[test]
    fn dead_member_settings_only_for_multi_deployment_groups() {
        let mut cfg = two_pair_config();
        cfg.gateway.groups[1].members.pop();
        let yaml = generate_config_yaml(&cfg).expect("generate");
        let v: serde_yaml::Value = serde_yaml::from_str(&yaml).expect("yaml");
        assert_eq!(v["router_settings"]["num_retries"], 2);
        for m in v["model_list"].as_sequence().unwrap() {
            let multi = m["model_name"] == "a";
            assert_eq!(
                m["litellm_params"]["cooldown_time"].as_u64(),
                multi.then_some(15),
                "{yaml}"
            );
            let fails = m.get("model_info").map(|i| i["allowed_fails"].as_u64());
            assert_eq!(fails, multi.then_some(Some(0)), "{yaml}");
        }
        // Single-member groups only: nothing router-wide either.
        let yaml = generate_config_yaml(&four_backend_config()).expect("generate");
        assert!(!yaml.contains("num_retries"), "{yaml}");
        assert!(!yaml.contains("cooldown_time"), "{yaml}");
    }

    /// B4: LiteLLM feeds its failure metrics only to a logger
    /// registered under `callbacks`.
    #[test]
    fn prometheus_is_registered_for_every_event() {
        let yaml = generate_config_yaml(&four_backend_config()).expect("generate");
        assert!(yaml.contains("  callbacks: [prometheus]\n"), "{yaml}");
        assert!(!yaml.contains("success_callback"), "{yaml}");
    }

    /// `effective_global_strategy` is the generator's own rule: it
    /// names exactly the strategy written to `router_settings`.
    #[test]
    fn effective_global_strategy_matches_generated_router_settings() {
        let cfg = four_backend_config();
        let strategy = effective_global_strategy(&cfg).expect("groups included");
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(
            yaml.contains(&format!(
                "routing_strategy: {}",
                strategy_to_litellm(strategy)
            )),
            "{yaml}"
        );
    }

    /// A group whose only deployments are OpenRouter fallbacks is
    /// included by the generator (its members are all disabled), so it
    /// takes part in the resolution.
    #[test]
    fn effective_global_strategy_counts_fallback_only_groups() {
        let mut cfg = four_backend_config();
        cfg.gateway.openrouter.enabled = true;
        cfg.gateway.groups = vec![ModelGroup {
            name: "solo".to_string(),
            members: vec!["coder-fast".to_string()],
            strategy: RoutingStrategy::LeastBusy,
            fallbacks: vec!["openrouter".to_string()],
            purpose: String::new(),
        }];
        for b in cfg.backends.iter_mut() {
            b.enabled = false;
        }
        assert!(generate_config_yaml(&cfg).is_ok());
        assert_eq!(
            effective_global_strategy(&cfg),
            Some(RoutingStrategy::LeastBusy)
        );
    }

    /// No config would be generated: no strategy is reported.
    #[test]
    fn effective_global_strategy_none_when_nothing_generates() {
        let mut cfg = four_backend_config();
        cfg.gateway.groups.clear();
        assert_eq!(effective_global_strategy(&cfg), None);

        let mut cfg = four_backend_config();
        cfg.gateway.groups = vec![ModelGroup {
            name: "ghost".to_string(),
            members: vec!["no-such-backend".to_string()],
            strategy: RoutingStrategy::LeastBusy,
            fallbacks: vec![],
            purpose: String::new(),
        }];
        assert!(generate_config_yaml(&cfg).is_err());
        assert_eq!(effective_global_strategy(&cfg), None);
    }

    #[test]
    fn yaml_includes_winml_under_tool_runner_when_registered() {
        let mut cfg = four_backend_config();
        cfg.winml.enabled = true;
        cfg.winml.port = 8090;
        let yaml = generate_config_yaml(&cfg).expect("generate");
        assert!(yaml.contains("http://127.0.0.1:8090/v1"));
        // Only under tool-runner, not planner.
        let planner_section = yaml
            .split("model_name: planner")
            .nth(1)
            .unwrap()
            .split("model_name: ")
            .next()
            .unwrap();
        assert!(!planner_section.contains(":8090"));
    }

    #[test]
    fn openrouter_target_type_round_trips() {
        let t = OpenRouterTarget {
            enabled: true,
            model_allowlist: vec!["qwen/qwen3-235b-a22b".to_string()],
            daily_cap_usd: 5.0,
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: OpenRouterTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
    }
}
