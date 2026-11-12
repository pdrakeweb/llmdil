//! Context-overflow routing rule and the generated LiteLLM routing callback.

use manager_config::{AppConfig, ModelGroup};
use serde::Serialize;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Context-overflow routing rule (docs/routing-rules-design.md §4, §6.3)
// ---------------------------------------------------------------------------

/// One group's baked windows for the context-overflow rule: the hard
/// window (largest prompt the group accepts), the optimal window
/// (advisory threshold), and the loopback URL of the representative
/// backend's exact token-counting endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoutingGroupWindow {
    pub name: String,
    pub hard_tokens: u32,
    pub optimal_tokens: u32,
    /// `http://127.0.0.1:<port>/v1/chat/completions/input_tokens` of
    /// the first enabled member backend. `None` when the group has
    /// enabled members but none is a local backend (host:port-only
    /// groups have no baked counting endpoint — the hook then uses
    /// its degraded estimate path for the group).
    pub count_url: Option<String>,
}

/// The full rule table baked into the generated routing callback:
/// computed from the config at generation time (design §4.1), so a
/// backend context change flows through the ordinary activation
/// path exactly like a `config.yaml` change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextOverflowTable {
    pub target_group: String,
    pub output_reserve_tokens: u32,
    pub advisory: bool,
    /// Every configured group with a computable window, including
    /// the target itself (the advisory is judged against the
    /// SERVING group's window, design §4.1).
    pub groups: Vec<RoutingGroupWindow>,
}

/// The config's enabled context-overflow rule, if any. Validation
/// guarantees at most one enabled typed rule (manager-config §6.2),
/// so the first enabled one is the rule.
fn enabled_overflow_rule(
    config: &AppConfig,
) -> Option<(&manager_config::RoutingRule, ContextOverflowFields)> {
    config.gateway.routing_rules.iter().find_map(|r| {
        if !r.enabled {
            return None;
        }
        // RoutingRuleKind has a single variant in v1, so this
        // destructure is irrefutable.
        let fields = r.kind.as_ref().map(|kind| {
            let manager_config::RoutingRuleKind::ContextOverflow {
                target_group,
                optimal_fraction,
                optimal_tokens,
                output_reserve_tokens,
                advisory,
            } = kind;
            ContextOverflowFields {
                target_group: target_group.clone(),
                optimal_fraction: *optimal_fraction,
                optimal_tokens: *optimal_tokens,
                output_reserve_tokens: *output_reserve_tokens,
                advisory: *advisory,
            }
        })?;
        Some((r, fields))
    })
}

#[derive(Debug, Clone)]
struct ContextOverflowFields {
    target_group: String,
    optimal_fraction: f64,
    optimal_tokens: Option<u32>,
    output_reserve_tokens: u32,
    advisory: bool,
}

/// Separator- and case-insensitive path comparison, matching
/// manager-config's model-registry lookup semantics (Windows paths
/// are case-insensitive).
fn normalize_model_path(p: &std::path::Path) -> String {
    p.to_string_lossy().replace('\\', "/").to_lowercase()
}

/// A backend's hard window (design §4.1): its configured context,
/// capped by its model's rated context from the curated catalog
/// (0/unknown rating = no cap). The backend's model is the
/// registered model whose file the backend serves; a backend whose
/// model is not in the catalog is capped by `n_ctx` alone.
fn backend_hard_window(
    backend: &manager_config::BackendConfig,
    config: &AppConfig,
    catalog_ctx_max: &HashMap<String, u32>,
) -> u32 {
    let ctx_max = config
        .models
        .iter()
        .find(|m| normalize_model_path(&m.gguf_path) == normalize_model_path(&backend.model_file))
        .and_then(|m| catalog_ctx_max.get(&m.id).copied())
        .unwrap_or(0);
    if ctx_max > 0 {
        backend.flags.n_ctx.min(ctx_max)
    } else {
        backend.flags.n_ctx
    }
}

/// A group's hard window: the minimum over its enabled member
/// backends (routing may land on any member, so the group is only
/// as large as its smallest enabled member — design §4.1). `None`
/// when the group has no enabled backend member (host:port members
/// carry no configured context, so they cannot bound the window).
fn group_hard_window(
    group: &ModelGroup,
    config: &AppConfig,
    catalog_ctx_max: &HashMap<String, u32>,
) -> Option<(u32, Option<String>)> {
    let mut hard: Option<u32> = None;
    let mut count_url: Option<String> = None;
    for member in &group.members {
        let Some(backend) = config.backends.iter().find(|b| b.id == *member) else {
            continue;
        };
        if !backend.enabled {
            continue;
        }
        let w = backend_hard_window(backend, config, catalog_ctx_max);
        hard = Some(hard.map_or(w, |h: u32| h.min(w)));
        if count_url.is_none() {
            count_url = Some(format!(
                "http://127.0.0.1:{}/v1/chat/completions/input_tokens",
                backend.port
            ));
        }
    }
    hard.map(|h| (h, count_url))
}

/// Compute the context-overflow rule table for a config, or `None`
/// when no typed rule is enabled (the kill-switch state: generation
/// must then be indistinguishable from a rule-free config —
/// design §6.1/§6.3).
pub fn context_overflow_table(config: &AppConfig) -> Option<ContextOverflowTable> {
    let (_, fields) = enabled_overflow_rule(config)?;
    let catalog_ctx_max: HashMap<String, u32> = manager_models::curated_models()
        .into_iter()
        .map(|s| (s.id, s.ctx_max))
        .collect();
    let mut groups = Vec::new();
    for group in &config.gateway.groups {
        let Some((hard, count_url)) = group_hard_window(group, config, &catalog_ctx_max) else {
            continue;
        };
        // The rule's absolute override wins for every group; else
        // the fraction of the group's own hard window (§4.1).
        let optimal = fields
            .optimal_tokens
            .unwrap_or_else(|| (fields.optimal_fraction * f64::from(hard)).floor() as u32);
        groups.push(RoutingGroupWindow {
            name: group.name.clone(),
            hard_tokens: hard,
            optimal_tokens: optimal,
            count_url,
        });
    }
    Some(ContextOverflowTable {
        target_group: fields.target_group,
        output_reserve_tokens: fields.output_reserve_tokens,
        advisory: fields.advisory,
        groups,
    })
}

/// The fixed callback template (design §6.4), with the rule table
/// substituted as data at `__LLMDIL_RULE_TABLE__`. The table values
/// are serialized JSON embedded as a Python string literal — never
/// string-interpolated code — so config content cannot inject
/// Python into the generated file.
const ROUTING_CALLBACK_TEMPLATE: &str = r##"# Generated by Local LLM Service Manager — DO NOT HAND-EDIT.
# Regenerated from the app configuration whenever the gateway is (re)started from the Setup Wizard.
#
# Context-overflow routing callback (docs/routing-rules-design.md):
# counts each chat request exactly against the requested group's own
# backend, reroutes requests past the group's hard window to the
# rule's target group, and — past the serving group's optimal
# window — injects the compaction advisory. Fail-open: any error
# leaves the request untouched.
import json
import urllib.request

from litellm.integrations.custom_logger import CustomLogger

ADVISORY_SENTENCE = "One note: this conversation is now past the length where my answers are sharpest — compacting it, or starting a fresh chat with a summary, will get you higher-quality responses."

ADVISORY_TEMPLATE = (
    "[Routing note from the local gateway — not from the user, and do not quote it: "
    "this conversation is now about {need} tokens, beyond the roughly {optimal}-token range "
    "where your answers are most reliable. Answer the user's current request normally and in full. "
    "Then, as the final sentence of your reply, add this sentence exactly: \"{sentence}\" "
    "Give this advice only once per conversation: if you have already given it earlier in this "
    "conversation, do not repeat it.]"
)

COUNT_TIMEOUT_SECS = 2.0
ESTIMATE_MARGIN = 1.15

RULE_TABLE = json.loads(__LLMDIL_RULE_TABLE__)


def _metadata(data):
    metadata = data.get("metadata")
    if not isinstance(metadata, dict):
        metadata = {}
        data["metadata"] = metadata
    return metadata


def _count_exact(url, payload):
    body = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(
        url, data=body, headers={"Content-Type": "application/json"}, method="POST"
    )
    with urllib.request.urlopen(req, timeout=COUNT_TIMEOUT_SECS) as resp:
        parsed = json.loads(resp.read().decode("utf-8"))
    return int(parsed["input_tokens"])


def _count_estimate(messages):
    from litellm import token_counter

    return int(token_counter(messages=messages) * ESTIMATE_MARGIN)


def _already_advised(messages):
    for message in messages:
        if not isinstance(message, dict) or message.get("role") != "assistant":
            continue
        content = message.get("content")
        if isinstance(content, str) and ADVISORY_SENTENCE in content:
            return True
    return False


def _route(data):
    if not isinstance(data, dict):
        return None
    messages = data.get("messages")
    if not isinstance(messages, list) or not messages:
        return None
    group = data.get("model")
    if not isinstance(group, str):
        return None
    entry = RULE_TABLE["groups"].get(group)
    if entry is None:
        return None
    target = RULE_TABLE["target_group"]

    # Count exactly first (the serving backend's own tokenizer and
    # chat template, design section 3.5); on any failure degrade to
    # LiteLLM's counter with a safety margin, marked in the request
    # metadata; with neither available, pass through (fail-open).
    prompt_tokens = None
    estimated = False
    count_url = entry.get("count_url")
    if count_url:
        try:
            prompt_tokens = _count_exact(count_url, data)
        except Exception:
            prompt_tokens = None
    if prompt_tokens is None:
        try:
            prompt_tokens = _count_estimate(messages)
            estimated = True
        except Exception:
            return None

    max_tokens = data.get("max_tokens")
    if isinstance(max_tokens, bool) or not isinstance(max_tokens, int) or max_tokens <= 0:
        max_tokens = RULE_TABLE["output_reserve_tokens"]
    need = prompt_tokens + max_tokens

    if estimated:
        _metadata(data)["llmdil_count_estimated"] = True

    serving = group
    if need > entry["hard"] and group != target:
        serving = target
        data["model"] = target
        _metadata(data)["llmdil_rerouted_from"] = group

    if RULE_TABLE["advisory"]:
        serving_entry = RULE_TABLE["groups"].get(serving)
        if (
            serving_entry is not None
            and need > serving_entry["optimal"]
            and not _already_advised(messages)
        ):
            note = ADVISORY_TEMPLATE.format(
                need=f"{need:,}",
                optimal=f"{serving_entry['optimal']:,}",
                sentence=ADVISORY_SENTENCE,
            )
            index = 0
            while index < len(messages):
                head = messages[index]
                if not isinstance(head, dict) or head.get("role") != "system":
                    break
                index += 1
            messages.insert(index, {"role": "system", "content": note})
    return data


class LlmdilRoutingHandler(CustomLogger):
    # Defined directly on this class: the proxy detects the hook by
    # looking for the method on the handler class itself.
    async def async_pre_call_hook(self, user_api_key_dict, cache, data, call_type):
        try:
            return _route(data)
        except Exception:
            return None


proxy_handler_instance = LlmdilRoutingHandler()
"##;

/// Render a string as a Python double-quoted string literal
/// (JSON-compatible escaping, which Python accepts verbatim).
fn py_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Generate the `llmdil_routing.py` callback file for a config, or
/// `None` when no typed routing rule is enabled (the caller then
/// writes nothing and removes any stale copy — design §6.3).
pub fn generate_routing_callback(config: &AppConfig) -> Option<String> {
    let table = context_overflow_table(config)?;
    // The table serializes to a JSON object the template loads with
    // json.loads; groups become a name-keyed object for the hook.
    let groups: serde_json::Map<String, serde_json::Value> = table
        .groups
        .iter()
        .map(|g| {
            (
                g.name.clone(),
                serde_json::json!({
                    "hard": g.hard_tokens,
                    "optimal": g.optimal_tokens,
                    "count_url": g.count_url,
                }),
            )
        })
        .collect();
    let table_json = serde_json::json!({
        "target_group": table.target_group,
        "output_reserve_tokens": table.output_reserve_tokens,
        "advisory": table.advisory,
        "groups": groups,
    });
    let rendered = ROUTING_CALLBACK_TEMPLATE.replacen(
        "__LLMDIL_RULE_TABLE__",
        &py_string_literal(&table_json.to_string()),
        1,
    );
    Some(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate_config_yaml;
    use crate::test_fixtures::*;
    use manager_config::RoutingStrategy;
    use std::path::PathBuf;

    // --- context-overflow routing rule (design §4/§6.3) ---

    fn model(id: &str, path: &str) -> manager_config::ModelConfig {
        manager_config::ModelConfig {
            id: id.to_string(),
            gguf_path: PathBuf::from(path),
            quant: "Q4_K_M".to_string(),
            params_b: 30.0,
            sha256: String::new(),
            verified_at: None,
            verified_sha256: None,
            source_url: String::new(),
            assigned_backends: Vec::new(),
            enabled: true,
            display_name: None,
            moe_layers: None,
            default_offload_layers: None,
            mtp: None,
        }
    }

    fn overflow_rule(target: &str) -> manager_config::RoutingRule {
        manager_config::RoutingRule {
            id: "context-overflow".to_string(),
            description: "Oversized requests go to the large-context group".to_string(),
            enabled: true,
            kind: Some(manager_config::RoutingRuleKind::ContextOverflow {
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

    /// The design §4.4 fleet shape: coder at a configured 65,536
    /// context, a large-context group whose member is configured
    /// at the model's full 262,144 rating.
    fn routing_fixture() -> AppConfig {
        let mut cfg = AppConfig::default_config();
        cfg.mcp
            .servers
            .retain(|s| s.id == "github" || s.id == "gdrive");
        let mut planner = backend("planner", 8081);
        planner.model_file = PathBuf::from("models/planner.gguf");
        planner.flags.n_ctx = 262144;
        let mut coder = backend("coder", 8082);
        coder.model_file = PathBuf::from("models/coder.gguf");
        coder.flags.n_ctx = 65536;
        cfg.backends = vec![planner, coder];
        cfg.models = vec![
            model("qwen3-27b-planner", "models/planner.gguf"),
            model("qwen3-coder-30b", "models/coder.gguf"),
        ];
        cfg.gateway.groups = vec![
            group("coder", RoutingStrategy::SimpleShuffle, &["coder"]),
            group("large-context", RoutingStrategy::LatencyBased, &["planner"]),
        ];
        cfg.gateway.routing_rules = vec![overflow_rule("large-context")];
        cfg
    }

    #[test]
    fn routing_table_bakes_windows_from_config() {
        let table = context_overflow_table(&routing_fixture()).expect("table");
        assert_eq!(table.target_group, "large-context");
        assert_eq!(table.output_reserve_tokens, 1024);
        assert!(table.advisory);
        let coder = table.groups.iter().find(|g| g.name == "coder").unwrap();
        assert_eq!(coder.hard_tokens, 65536);
        assert_eq!(coder.optimal_tokens, 32768);
        assert_eq!(
            coder.count_url.as_deref(),
            Some("http://127.0.0.1:8082/v1/chat/completions/input_tokens")
        );
        let target = table
            .groups
            .iter()
            .find(|g| g.name == "large-context")
            .unwrap();
        assert_eq!(target.hard_tokens, 262144);
        assert_eq!(target.optimal_tokens, 131072);
        assert_eq!(
            target.count_url.as_deref(),
            Some("http://127.0.0.1:8081/v1/chat/completions/input_tokens")
        );
    }

    #[test]
    fn routing_table_catalog_cap_and_absolute_optimal() {
        let mut cfg = routing_fixture();
        // Configured context above the model's rating: the catalog
        // cap (262,144 for both fixture models) is the hard window.
        cfg.backends
            .iter_mut()
            .find(|b| b.id == "coder")
            .unwrap()
            .flags
            .n_ctx = 300_000;
        let table = context_overflow_table(&cfg).expect("table");
        let coder = table.groups.iter().find(|g| g.name == "coder").unwrap();
        assert_eq!(coder.hard_tokens, 262144);
        // An absolute optimal_tokens override wins for every group.
        if let Some(manager_config::RoutingRuleKind::ContextOverflow { optimal_tokens, .. }) =
            &mut cfg.gateway.routing_rules[0].kind
        {
            *optimal_tokens = Some(40960);
        }
        let table = context_overflow_table(&cfg).expect("table");
        assert!(table.groups.iter().all(|g| g.optimal_tokens == 40960));
    }

    #[test]
    fn routing_disabled_backend_members_do_not_bound_the_window() {
        let mut cfg = routing_fixture();
        // A disabled small-context member must not shrink the
        // group's window: routing can never land on it.
        let mut fast = backend("coder-fast", 8083);
        fast.enabled = false;
        fast.flags.n_ctx = 8192;
        cfg.backends.push(fast);
        cfg.gateway.groups[0].members.push("coder-fast".to_string());
        let table = context_overflow_table(&cfg).expect("table");
        let coder = table.groups.iter().find(|g| g.name == "coder").unwrap();
        assert_eq!(coder.hard_tokens, 65536);
    }

    #[test]
    fn routing_no_enabled_rule_generates_nothing() {
        // Golden: with no enabled typed rule the YAML is
        // byte-identical to the same config with no rules at all,
        // and no callback file is generated (design §6.1/§6.3).
        let base = four_backend_config();
        let base_yaml = generate_config_yaml(&base).expect("generate");
        assert!(generate_routing_callback(&base).is_none());
        assert!(context_overflow_table(&base).is_none());

        let mut disabled = base.clone();
        let mut rule = overflow_rule("planner");
        rule.enabled = false;
        disabled.gateway.routing_rules.push(rule);
        assert_eq!(
            generate_config_yaml(&disabled).expect("generate"),
            base_yaml
        );
        assert!(generate_routing_callback(&disabled).is_none());

        let mut legacy = base.clone();
        legacy
            .gateway
            .routing_rules
            .push(manager_config::RoutingRule {
                id: "legacy".to_string(),
                description: "old fragment rule".to_string(),
                enabled: true,
                kind: None,
                litellm_fragment: Some(serde_json::json!({"x": 1})),
                source: "user".to_string(),
            });
        assert_eq!(generate_config_yaml(&legacy).expect("generate"), base_yaml);
        assert!(generate_routing_callback(&legacy).is_none());
    }

    #[test]
    fn routing_yaml_gains_callback_and_safety_net() {
        let yaml = generate_config_yaml(&routing_fixture()).expect("generate");
        assert!(
            yaml.contains("  callbacks: llmdil_routing.proxy_handler_instance\n"),
            "callbacks key missing:\n{yaml}"
        );
        assert!(yaml.contains("  context_window_fallbacks:\n"));
        assert!(yaml.contains("    - coder: [large-context]\n"));
        // Loop safety: the target group gets NO fallback entry.
        assert!(
            !yaml.contains("- large-context: ["),
            "target must not have a context fallback entry:\n{yaml}"
        );
        let py = generate_routing_callback(&routing_fixture()).expect("callback");
        assert!(py.contains("class LlmdilRoutingHandler(CustomLogger):"));
        assert!(py.contains("proxy_handler_instance = LlmdilRoutingHandler()"));
        assert!(py.contains("async def async_pre_call_hook"));
    }

    /// The generated callback, executed under python3 with the
    /// LiteLLM surface stubbed (design §6.4 test plan): exact-count
    /// reroute, advisory placement and once-only behavior, the
    /// degraded estimate path, fail-open, and streaming parity.
    /// Skips when no python3 is available.
    #[test]
    fn routing_callback_hook_behavior() {
        let probe = std::process::Command::new("python3")
            .arg("--version")
            .output();
        let Ok(probe) = probe else {
            eprintln!("python3 unavailable; skipping hook behavior test");
            return;
        };
        if !probe.status.success() {
            eprintln!("python3 probe failed; skipping hook behavior test");
            return;
        }
        let py = generate_routing_callback(&routing_fixture()).expect("callback");
        let dir =
            std::env::temp_dir().join(format!("llmdil-routing-hook-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let result = run_hook_driver(&dir, &py);
        let _ = std::fs::remove_dir_all(&dir);
        let results = result.expect("hook driver run");
        assert_hook_results(&results);
    }

    fn run_hook_driver(dir: &std::path::Path, py: &str) -> Result<serde_json::Value, String> {
        std::fs::write(dir.join("llmdil_routing.py"), py).map_err(|e| e.to_string())?;
        std::fs::write(dir.join("driver.py"), HOOK_DRIVER_PY).map_err(|e| e.to_string())?;
        // py_compile gate (design §6.4): the generated file must be
        // valid Python before its behavior is even exercised.
        let compile = std::process::Command::new("python3")
            .args(["-m", "py_compile"])
            .arg(dir.join("llmdil_routing.py"))
            .output()
            .map_err(|e| e.to_string())?;
        if !compile.status.success() {
            return Err(format!(
                "py_compile failed: {}",
                String::from_utf8_lossy(&compile.stderr)
            ));
        }
        let out = std::process::Command::new("python3")
            .arg(dir.join("driver.py"))
            .arg(dir)
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!(
                "driver failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let last = stdout.lines().last().unwrap_or("");
        serde_json::from_str(last).map_err(|e| format!("driver output parse: {e}: {stdout}"))
    }

    fn assert_hook_results(r: &serde_json::Value) {
        // The baked table reached the module intact.
        assert_eq!(r["table"]["target_group"], "large-context");
        assert_eq!(r["table"]["output_reserve_tokens"], 1024);
        assert_eq!(r["table"]["advisory"], true);
        assert_eq!(r["table"]["groups"]["coder"]["hard"], 65536);
        assert_eq!(r["table"]["groups"]["coder"]["optimal"], 32768);
        assert_eq!(r["table"]["groups"]["large-context"]["hard"], 262144);
        assert_eq!(r["table"]["groups"]["large-context"]["optimal"], 131072);

        // Over-hard: rerouted + stamped, no advisory (need is far
        // below the target's optimal window — design §4.4 ex. 2).
        assert_eq!(r["over_hard"]["model"], "large-context");
        assert_eq!(r["over_hard"]["meta"]["llmdil_rerouted_from"], "coder");
        assert_eq!(r["over_hard"]["n_messages"], 2);
        assert!(
            r["over_hard"]["urls"][0]
                .as_str()
                .unwrap()
                .contains("127.0.0.1:8082"),
            "counting must hit the requested group's own backend"
        );

        // Under hard, over optimal: advisory inserted after the
        // leading system message, model untouched (§4.4 ex. 1).
        assert_eq!(r["advisory"]["model"], "coder");
        assert_eq!(
            r["advisory"]["roles"],
            serde_json::json!(["system", "system", "user"])
        );
        let note = r["advisory"]["note"].as_str().unwrap();
        assert!(note.contains("41,024"), "note carries need: {note}");
        assert!(note.contains("32,768"), "note carries optimal: {note}");
        assert!(note.contains(
            "One note: this conversation is now past the length where my answers are sharpest"
        ));

        // Once per conversation: the sentence already in history
        // suppresses a second injection.
        assert_eq!(r["advisory_once"]["n_messages"], 4);

        // Direct-to-target past its optimal window: advisory fires,
        // no reroute metadata (D7).
        assert_eq!(r["direct_target"]["model"], "large-context");
        assert!(r["direct_target"]["meta"].is_null());
        assert_eq!(
            r["direct_target"]["roles"],
            serde_json::json!(["system", "system", "user"])
        );

        // Degraded path: counting down → margined estimate reroutes
        // and is marked as estimated.
        assert_eq!(r["estimate"]["model"], "large-context");
        assert_eq!(r["estimate"]["meta"]["llmdil_count_estimated"], true);
        assert_eq!(r["estimate"]["meta"]["llmdil_rerouted_from"], "coder");

        // Both counters down: fail-open — the hook returns None.
        assert_eq!(r["fail_open"]["result_is_none"], true);

        // Streaming requests take the identical path.
        assert_eq!(r["streaming"]["model"], "large-context");
        assert_eq!(r["streaming"]["meta"]["llmdil_rerouted_from"], "coder");

        // max_tokens counts toward need (§4.2 step 3): the same
        // prompt passes without it and reroutes with it.
        assert_eq!(r["max_tokens"]["with"], "large-context");
        assert_eq!(r["max_tokens"]["without"], "coder");

        // Groups outside the table pass through untouched.
        assert_eq!(r["uncovered"]["result_is_none"], true);

        // A small request is returned with no mutation at all.
        assert_eq!(r["small"]["model"], "coder");
        assert!(r["small"]["meta"].is_null());
        assert_eq!(r["small"]["n_messages"], 2);
    }

    const HOOK_DRIVER_PY: &str = r##"
import asyncio, importlib.util, json, sys, types, urllib.request

workdir = sys.argv[1]

# Stub the LiteLLM surface the generated module imports: the
# CustomLogger base class and litellm.token_counter.
litellm = types.ModuleType("litellm")
integrations = types.ModuleType("litellm.integrations")
custom_logger = types.ModuleType("litellm.integrations.custom_logger")

class CustomLogger:
    pass

custom_logger.CustomLogger = CustomLogger
integrations.custom_logger = custom_logger
litellm.integrations = integrations

ESTIMATE = {"value": 0, "fail": False}

def token_counter(messages=None, **kwargs):
    if ESTIMATE["fail"]:
        raise RuntimeError("counter unavailable")
    return ESTIMATE["value"]

litellm.token_counter = token_counter
sys.modules["litellm"] = litellm
sys.modules["litellm.integrations"] = integrations
sys.modules["litellm.integrations.custom_logger"] = custom_logger

spec = importlib.util.spec_from_file_location("llmdil_routing", workdir + "/llmdil_routing.py")
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)

COUNT = {"tokens": 0, "fail": False, "urls": []}

class FakeResp:
    def __enter__(self):
        return self
    def __exit__(self, *args):
        return False
    def read(self):
        return json.dumps({"input_tokens": COUNT["tokens"]}).encode()

def fake_urlopen(req, timeout=None):
    COUNT["urls"].append(req.full_url)
    if COUNT["fail"]:
        raise OSError("connection refused")
    return FakeResp()

urllib.request.urlopen = fake_urlopen
handler = mod.proxy_handler_instance

def run(data):
    return asyncio.run(handler.async_pre_call_hook(None, None, data, "completion"))

def chat(model, count, extra=None, history=None):
    COUNT["tokens"] = count
    msgs = [{"role": "system", "content": "You are helpful."},
            {"role": "user", "content": "hello"}]
    if history:
        msgs = msgs + history
    data = {"model": model, "messages": msgs}
    if extra:
        data.update(extra)
    return run(data)

results = {}
results["table"] = mod.RULE_TABLE

r = chat("coder", 70000)
results["over_hard"] = {"model": r["model"], "meta": r.get("metadata"),
                        "n_messages": len(r["messages"]), "urls": list(COUNT["urls"])}
COUNT["urls"].clear()

r = chat("coder", 40000)
results["advisory"] = {"model": r["model"],
                       "roles": [m["role"] for m in r["messages"]],
                       "note": r["messages"][1]["content"] if len(r["messages"]) > 2 else None}

r = chat("coder", 40000, history=[
    {"role": "assistant", "content": "Earlier answer. " + mod.ADVISORY_SENTENCE},
    {"role": "user", "content": "more"}])
results["advisory_once"] = {"n_messages": len(r["messages"])}

r = chat("large-context", 150000)
results["direct_target"] = {"model": r["model"], "meta": r.get("metadata"),
                            "roles": [m["role"] for m in r["messages"]]}

COUNT["fail"] = True
ESTIMATE["value"] = 60000
r = chat("coder", 0)
results["estimate"] = {"model": r["model"], "meta": r.get("metadata")}
COUNT["fail"] = False

COUNT["fail"] = True
ESTIMATE["fail"] = True
r = run({"model": "coder", "messages": [{"role": "user", "content": "x"}]})
results["fail_open"] = {"result_is_none": r is None}
COUNT["fail"] = False
ESTIMATE["fail"] = False

r = chat("coder", 70000, extra={"stream": True})
results["streaming"] = {"model": r["model"], "meta": r.get("metadata")}

results["max_tokens"] = {
    "with": chat("coder", 64000, extra={"max_tokens": 2000})["model"],
    "without": chat("coder", 64000)["model"],
}

r = run({"model": "openrouter", "messages": [{"role": "user", "content": "x"}]})
results["uncovered"] = {"result_is_none": r is None}

r = chat("coder", 1000)
results["small"] = {"model": r["model"], "meta": r.get("metadata"),
                    "n_messages": len(r["messages"])}

print(json.dumps(results))
"##;
}
