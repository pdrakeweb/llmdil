//! Whole-config validation (spec §10).

use std::collections::{HashMap, HashSet};

use crate::{
    validate_display_name, validate_mcp_servers, validate_searxng, AppConfig, ConfigError,
    RoutingRuleKind, ServerFlags, SplitMode, CURRENT_VERSION,
};

/// Tolerance for tensor_split sum-to-1.0 checks.
const SPLIT_SUM_EPSILON: f32 = 1e-4;

/// Upper bound for the gateway request ring buffer length.
const MAX_REQUEST_LOG_LEN: usize = 100_000;

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

    // --- admin API bind choice (auth-plan §5) ---
    // A specific-interface bind names one concrete address. A
    // wildcard there would silently mean "all interfaces", so it is
    // refused rather than accepted as a narrower-looking setting.
    if let Some(ip) = &config.admin_api.bind_ip {
        match ip.parse::<std::net::IpAddr>() {
            Err(_) => errors.push(format!("admin_api bind_ip {ip:?} is not an IP address")),
            Ok(addr) if addr.is_unspecified() || addr.is_multicast() => errors.push(format!(
                "admin_api bind_ip {ip:?} is not a single interface address"
            )),
            Ok(_) => {}
        }
    }
    if config.admin_api.bind_mode == crate::AdminApiBindMode::Interface
        && config.admin_api.bind_ip.is_none()
    {
        errors.push("admin_api bind_mode \"interface\" requires bind_ip".to_string());
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
        // A declared offload default is meaningless without the
        // MoE layer count it counts against, and can never exceed
        // it (the write paths enforce the pair atomically; this is
        // the backstop for hand-edited files).
        match (m.moe_layers, m.default_offload_layers) {
            (Some(0), _) => {
                errors.push(format!(
                    "model {:?}: moe_layers must be > 0 when declared",
                    m.id
                ));
            }
            (None, Some(_)) => {
                errors.push(format!(
                    "model {:?}: default_offload_layers requires moe_layers to be declared",
                    m.id
                ));
            }
            (Some(layers), Some(default)) if default > layers => {
                errors.push(format!(
                    "model {:?}: default_offload_layers {default} exceeds moe_layers {layers}",
                    m.id
                ));
            }
            _ => {}
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
        errors.push("at most one enabled context-overflow routing rule is supported".to_string());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::*;
    use crate::*;

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
        if let Some(RoutingRuleKind::ContextOverflow { optimal_tokens, .. }) = &mut rule.kind {
            *optimal_tokens = Some(0);
        }
        cfg.gateway.routing_rules.push(rule);
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("optimal_tokens"), "unexpected: {err}");

        // At/above the target's enabled members' configured context
        // (tool-runner: 32768) the threshold can never fire.
        let mut cfg = working_config();
        let mut rule = overflow_rule("context-overflow", "tool-runner");
        if let Some(RoutingRuleKind::ContextOverflow { optimal_tokens, .. }) = &mut rule.kind {
            *optimal_tokens = Some(32768);
        }
        cfg.gateway.routing_rules.push(rule);
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("hard window"), "unexpected: {err}");

        // Below it validates.
        let mut cfg = working_config();
        let mut rule = overflow_rule("context-overflow", "tool-runner");
        if let Some(RoutingRuleKind::ContextOverflow { optimal_tokens, .. }) = &mut rule.kind {
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

    /// Admin API bind choice (auth-plan §5): a config written before
    /// the setting existed keeps binding all interfaces, exactly as
    /// before — the field is additive and defaulted.
    #[test]
    fn admin_api_bind_defaults_preserve_existing_configs() {
        let legacy: AppConfig = serde_json::from_value({
            let mut v = serde_json::to_value(AppConfig::default_config()).unwrap();
            v["admin_api"] = serde_json::json!({ "enabled": true, "port": 47821 });
            v
        })
        .unwrap();
        assert_eq!(legacy.admin_api.bind_mode, crate::AdminApiBindMode::All);
        assert_eq!(legacy.admin_api.bind_ip, None);
        validate(&legacy).expect("legacy admin_api validates");
        assert_eq!(
            AppConfig::default_config().admin_api.bind_mode,
            crate::AdminApiBindMode::All
        );
        // Wire spelling.
        let v = serde_json::to_value(crate::AdminApiBindMode::Interface).unwrap();
        assert_eq!(v, "interface");
    }

    #[test]
    fn admin_api_bind_ip_must_be_one_concrete_address() {
        let mut cfg = working_config();
        cfg.admin_api.bind_mode = crate::AdminApiBindMode::Interface;
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("requires bind_ip"), "unexpected: {err}");
        for bad in ["localhost", "0.0.0.0", "::", "224.0.0.1"] {
            cfg.admin_api.bind_ip = Some(bad.to_string());
            let err = validate(&cfg).unwrap_err().to_string();
            assert!(err.contains("bind_ip"), "{bad}: unexpected: {err}");
        }
        cfg.admin_api.bind_ip = Some("192.168.1.20".to_string());
        validate(&cfg).expect("a concrete address validates");
        // A remembered address is kept (and still checked) in the
        // other modes.
        cfg.admin_api.bind_mode = crate::AdminApiBindMode::Loopback;
        validate(&cfg).expect("loopback with a remembered bind_ip validates");
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
