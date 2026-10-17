//! The salvage parser (design §4).
//!
//! Local models produce "mostly right with one broken entry", so
//! parsing salvages per item: extract the first balanced JSON
//! object, deserialize item by item, drop schema failures (counted,
//! fragments kept), demote mapping failures to advice (the
//! reasoning survives; the mutation does not), and cap the plan at
//! 25 items. Only a response with no extractable object — or no
//! items array — fails the consultation as a whole.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use manager_config::AppConfig;
use serde_json::{json, Map, Value};

use crate::plan::{fingerprint, Confidence, Decision, Plan, PlanItem, PlanKind};

/// Hard cap on plan items after parsing (design §4).
pub const MAX_ITEMS: usize = 25;

/// The prompt asks for at most this many items (design §6); the
/// parser's cap above is the enforcement, this is the request.
pub const REQUESTED_MAX_ITEMS: usize = 12;

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("no JSON object could be extracted from the response")]
    NoJsonObject,
    #[error("the response JSON has no items array")]
    NoItems,
}

/// Extract the first balanced `{...}` object from response text.
/// Code fences and surrounding prose are expected. String literals
/// and escapes are tracked so braces inside strings do not count.
pub fn extract_json_object(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for i in start..bytes.len() {
        let b = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

enum ChangeVerdict {
    /// Normalized, executable change payload.
    Ok(Value),
    /// Known kind, unexecutable payload: render as advice.
    Demote,
    /// Schema failure: drop the item.
    Drop,
}

fn as_u32(v: &Value) -> Option<u32> {
    if let Some(n) = v.as_u64() {
        return u32::try_from(n).ok();
    }
    if let Some(f) = v.as_f64() {
        if f.fract() == 0.0 && f >= 0.0 && f <= u32::MAX as f64 {
            return Some(f as u32);
        }
    }
    None
}

fn req_u32(obj: &Map<String, Value>, key: &str) -> Option<u32> {
    obj.get(key).and_then(as_u32)
}

fn opt_u32(obj: &Map<String, Value>, key: &str, verdict: &mut bool) -> Option<u32> {
    match obj.get(key) {
        None => None,
        Some(v) => match as_u32(v) {
            Some(n) => Some(n),
            None => {
                *verdict = false;
                None
            }
        },
    }
}

fn req_str(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty())
}

fn opt_str(obj: &Map<String, Value>, key: &str, ok: &mut bool) -> Option<String> {
    match obj.get(key) {
        None => None,
        Some(v) => match v.as_str() {
            Some(s) => Some(s.to_string()),
            None => {
                *ok = false;
                None
            }
        },
    }
}

fn req_str_vec(obj: &Map<String, Value>, key: &str) -> Option<Vec<Value>> {
    let arr = obj.get(key)?.as_array()?;
    let mut out = Vec::new();
    for v in arr {
        out.push(Value::String(v.as_str()?.to_string()));
    }
    Some(out)
}

fn req_f32_vec(obj: &Map<String, Value>, key: &str) -> Option<Vec<Value>> {
    let arr = obj.get(key)?.as_array()?;
    let mut out = Vec::new();
    for v in arr {
        let f = v.as_f64()?;
        out.push(json!(f));
    }
    Some(out)
}

/// Validate and normalize one item's change payload for its kind.
/// Only the fields the kind's mapping admits survive, so what
/// validation and apply see is exactly what the app can execute.
fn normalize_change(kind: PlanKind, change: &Value) -> ChangeVerdict {
    // Advice is handled by the caller (its text is the payload).
    let empty = Map::new();
    let obj = match change {
        Value::Object(o) => o,
        Value::Null => &empty,
        _ => return ChangeVerdict::Drop,
    };
    // A raw-flags rewrite anywhere in the change is the largest
    // hallucination surface in the app: advice-only, always (§4).
    if obj.contains_key("raw_flags") {
        return ChangeVerdict::Demote;
    }
    let allowed: &[&str] = match kind {
        PlanKind::BackendCreate => &[
            "model_id",
            "group",
            "port",
            "n_ctx",
            "n_batch",
            "tensor_split",
            "offload_layers",
        ],
        PlanKind::BackendDelete | PlanKind::GroupDelete => {
            return ChangeVerdict::Ok(json!({}));
        }
        PlanKind::BackendSetContext => &["n_ctx"],
        PlanKind::BackendSetFlags => &["n_ctx", "n_batch"],
        PlanKind::BackendSetOffload => &["offload_layers"],
        PlanKind::BackendSetPlacement => &["tensor_split", "split_mode"],
        PlanKind::BackendSetModel => &["model_id"],
        PlanKind::BackendSetEnabled => &["enabled"],
        PlanKind::GroupSetMembers => &["members"],
        PlanKind::GroupSetStrategy => &["strategy"],
        PlanKind::GroupSetFallbacks => &["fallbacks"],
        PlanKind::GroupCreate => &["members", "strategy", "fallbacks"],
        PlanKind::ModelAdd => &["source_url", "quant", "sha256"],
        PlanKind::Advice => &["text"],
    };
    // A field the mapping does not admit: the reasoning may be
    // sound but the mutation is not one the app can validate.
    if obj.keys().any(|k| !allowed.contains(&k.as_str())) {
        return ChangeVerdict::Demote;
    }
    let mut out = Map::new();
    let mut ok = true;
    match kind {
        PlanKind::BackendCreate => {
            match req_str(obj, "model_id") {
                Some(m) => {
                    out.insert("model_id".into(), json!(m));
                }
                None => return ChangeVerdict::Drop,
            }
            if let Some(g) = opt_str(obj, "group", &mut ok) {
                out.insert("group".into(), json!(g));
            }
            if let Some(p) = opt_u32(obj, "port", &mut ok) {
                out.insert("port".into(), json!(p));
            }
            if let Some(n) = opt_u32(obj, "n_ctx", &mut ok) {
                out.insert("n_ctx".into(), json!(n));
            }
            if let Some(n) = opt_u32(obj, "n_batch", &mut ok) {
                out.insert("n_batch".into(), json!(n));
            }
            if obj.contains_key("tensor_split") {
                match req_f32_vec(obj, "tensor_split") {
                    Some(v) => {
                        out.insert("tensor_split".into(), Value::Array(v));
                    }
                    None => return ChangeVerdict::Drop,
                }
            }
            if let Some(n) = opt_u32(obj, "offload_layers", &mut ok) {
                out.insert("offload_layers".into(), json!(n));
            }
        }
        PlanKind::BackendSetContext => match req_u32(obj, "n_ctx") {
            Some(n) => {
                out.insert("n_ctx".into(), json!(n));
            }
            None => return ChangeVerdict::Drop,
        },
        PlanKind::BackendSetFlags => {
            let mut any = false;
            if let Some(n) = opt_u32(obj, "n_ctx", &mut ok) {
                out.insert("n_ctx".into(), json!(n));
                any = true;
            }
            if let Some(n) = opt_u32(obj, "n_batch", &mut ok) {
                out.insert("n_batch".into(), json!(n));
                any = true;
            }
            if !any {
                return ChangeVerdict::Drop;
            }
        }
        PlanKind::BackendSetOffload => match req_u32(obj, "offload_layers") {
            Some(n) => {
                out.insert("offload_layers".into(), json!(n));
            }
            None => return ChangeVerdict::Drop,
        },
        PlanKind::BackendSetPlacement => {
            match req_f32_vec(obj, "tensor_split") {
                Some(v) => {
                    out.insert("tensor_split".into(), Value::Array(v));
                }
                None => return ChangeVerdict::Drop,
            }
            if let Some(m) = opt_str(obj, "split_mode", &mut ok) {
                if !["none", "layer", "row"].contains(&m.as_str()) {
                    return ChangeVerdict::Drop;
                }
                out.insert("split_mode".into(), json!(m));
            }
        }
        PlanKind::BackendSetModel => match req_str(obj, "model_id") {
            Some(m) => {
                out.insert("model_id".into(), json!(m));
            }
            None => return ChangeVerdict::Drop,
        },
        PlanKind::BackendSetEnabled => match obj.get("enabled").and_then(|v| v.as_bool()) {
            Some(b) => {
                out.insert("enabled".into(), json!(b));
            }
            None => return ChangeVerdict::Drop,
        },
        PlanKind::GroupSetMembers => match req_str_vec(obj, "members") {
            Some(v) => {
                out.insert("members".into(), Value::Array(v));
            }
            None => return ChangeVerdict::Drop,
        },
        PlanKind::GroupSetStrategy => match req_str(obj, "strategy") {
            Some(s) if ["simple-shuffle", "least-busy", "latency-based"].contains(&s.as_str()) => {
                out.insert("strategy".into(), json!(s));
            }
            _ => return ChangeVerdict::Drop,
        },
        PlanKind::GroupSetFallbacks => match req_str_vec(obj, "fallbacks") {
            Some(v) => {
                out.insert("fallbacks".into(), Value::Array(v));
            }
            None => return ChangeVerdict::Drop,
        },
        PlanKind::GroupCreate => {
            match req_str_vec(obj, "members") {
                Some(v) => {
                    out.insert("members".into(), Value::Array(v));
                }
                None => return ChangeVerdict::Drop,
            }
            match opt_str(obj, "strategy", &mut ok) {
                Some(s) => {
                    if !["simple-shuffle", "least-busy", "latency-based"].contains(&s.as_str()) {
                        return ChangeVerdict::Drop;
                    }
                    out.insert("strategy".into(), json!(s));
                }
                None => {
                    out.insert("strategy".into(), json!("simple-shuffle"));
                }
            }
            if obj.contains_key("fallbacks") {
                match req_str_vec(obj, "fallbacks") {
                    Some(v) => {
                        out.insert("fallbacks".into(), Value::Array(v));
                    }
                    None => return ChangeVerdict::Drop,
                }
            } else {
                out.insert("fallbacks".into(), json!([]));
            }
        }
        PlanKind::ModelAdd => {
            match req_str(obj, "source_url") {
                Some(u) => {
                    out.insert("source_url".into(), json!(u));
                }
                None => return ChangeVerdict::Drop,
            }
            if let Some(q) = opt_str(obj, "quant", &mut ok) {
                out.insert("quant".into(), json!(q));
            }
            if let Some(s) = opt_str(obj, "sha256", &mut ok) {
                out.insert("sha256".into(), json!(s));
            }
        }
        PlanKind::BackendDelete | PlanKind::GroupDelete | PlanKind::Advice => unreachable!(),
    }
    if !ok {
        return ChangeVerdict::Drop;
    }
    ChangeVerdict::Ok(Value::Object(out))
}

/// The preserved text of an item demoted to advice: its reasoning
/// and the proposed change, so nothing the model said is lost when
/// the mutation is refused.
fn demote_text(rationale: &str, expected_effect: &str, change: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !rationale.trim().is_empty() {
        parts.push(rationale.trim().to_string());
    }
    if !expected_effect.trim().is_empty() {
        parts.push(format!("Expected effect: {}", expected_effect.trim()));
    }
    if let Value::Object(o) = change {
        if !o.is_empty() {
            parts.push(format!(
                "Proposed change: {}",
                serde_json::to_string_pretty(change).unwrap_or_default()
            ));
        }
    }
    if parts.is_empty() {
        "(no detail provided)".to_string()
    } else {
        parts.join("\n\n")
    }
}

/// Whether an unrecognized kind belongs to a known-but-unmapped
/// family (gateway settings, downloads, wizard/install actions,
/// operations) and so demotes to advice instead of dropping
/// (design §4: "items proposing them parse as advice").
fn known_unmapped(kind: &str) -> bool {
    let Some((family, _)) = kind.split_once('.') else {
        return false;
    };
    matches!(
        family,
        "backend" | "group" | "model" | "gateway" | "wizard" | "update" | "mxc" | "winml"
    )
}

/// Assign a port to a backend.create item that did not name one:
/// the lowest free port from 8081 up, skipping the gateway and
/// Postgres ports and every configured or already-assigned port.
/// The assigned port becomes part of the item's change, so the
/// plan row can state it and apply uses exactly it.
fn assign_create_port(cfg: &AppConfig, assigned: &HashSet<u16>) -> u16 {
    let mut used: HashSet<u16> = cfg.backends.iter().map(|b| b.port).collect();
    used.insert(cfg.gateway.port);
    used.insert(cfg.postgres.port);
    used.extend(assigned.iter().copied());
    (8081..=8999)
        .find(|p| !used.contains(p))
        .unwrap_or(8999)
}

/// Parse a consultation response into a plan (design §4).
///
/// `cfg` is the configuration the consultation ran against (used
/// for create-port assignment); `denied_fingerprints` carries the
/// denial memory from previous plans (§6); `directive` is the
/// operator directive that scoped this consultation (addendum
/// A.2), recorded verbatim on the produced plan.
#[allow(clippy::too_many_arguments)]
pub fn parse_plan(
    response: &str,
    cfg: &AppConfig,
    consulted_backend: &str,
    config_hash: &str,
    context: Value,
    denied_fingerprints: &HashSet<String>,
    directive: &str,
    now: DateTime<Utc>,
) -> Result<Plan, ParseError> {
    let object_text = extract_json_object(response).ok_or(ParseError::NoJsonObject)?;
    let root: Value =
        serde_json::from_str(&object_text).map_err(|_| ParseError::NoJsonObject)?;
    let summary = root
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let raw_items = root
        .get("items")
        .and_then(|v| v.as_array())
        .ok_or(ParseError::NoItems)?;

    let mut items: Vec<PlanItem> = Vec::new();
    let mut dropped_invalid = 0usize;
    let mut dropped_fragments: Vec<String> = Vec::new();
    let mut assigned_ports: HashSet<u16> = HashSet::new();

    for raw in raw_items {
        let fragment = serde_json::to_string(raw).unwrap_or_default();
        let Some(obj) = raw.as_object() else {
            dropped_invalid += 1;
            dropped_fragments.push(fragment);
            continue;
        };
        let Some(kind_str) = obj.get("kind").and_then(|v| v.as_str()) else {
            dropped_invalid += 1;
            dropped_fragments.push(fragment);
            continue;
        };
        let target = obj
            .get("target")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let rationale = obj
            .get("rationale")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let expected_effect = obj
            .get("expected_effect")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let confidence = obj
            .get("confidence")
            .and_then(|v| v.as_str())
            .map(Confidence::parse)
            .unwrap_or(Confidence::Low);
        let change = obj.get("change").cloned().unwrap_or(Value::Null);

        let kind = match PlanKind::parse(kind_str) {
            Some(k) => k,
            None => {
                if known_unmapped(kind_str) {
                    items.push(PlanItem {
                        id: 0,
                        fingerprint: String::new(),
                        kind: PlanKind::Advice,
                        target,
                        change: json!({}),
                        advice_text: Some(demote_text(&rationale, &expected_effect, &change)),
                        rationale,
                        expected_effect,
                        confidence,
                        decision: Decision::Undecided,
                        previously_denied: false,
                        apply_outcome: None,
                        apply_error: None,
                    });
                } else {
                    dropped_invalid += 1;
                    dropped_fragments.push(fragment);
                }
                continue;
            }
        };

        if kind == PlanKind::Advice {
            let text = change
                .get("text")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            match text {
                Some(text) if !text.trim().is_empty() => {
                    items.push(PlanItem {
                        id: 0,
                        fingerprint: String::new(),
                        kind,
                        target,
                        change: json!({}),
                        advice_text: Some(text),
                        rationale,
                        expected_effect,
                        confidence,
                        decision: Decision::Undecided,
                        previously_denied: false,
                        apply_outcome: None,
                        apply_error: None,
                    });
                }
                _ => {
                    dropped_invalid += 1;
                    dropped_fragments.push(fragment);
                }
            }
            continue;
        }

        // Every executable kind names its target (for creates, the
        // new entity's id).
        let Some(target) = target else {
            dropped_invalid += 1;
            dropped_fragments.push(fragment);
            continue;
        };

        match normalize_change(kind, &change) {
            ChangeVerdict::Drop => {
                dropped_invalid += 1;
                dropped_fragments.push(fragment);
            }
            ChangeVerdict::Demote => {
                items.push(PlanItem {
                    id: 0,
                    fingerprint: String::new(),
                    kind: PlanKind::Advice,
                    target: Some(target),
                    change: json!({}),
                    advice_text: Some(demote_text(&rationale, &expected_effect, &change)),
                    rationale,
                    expected_effect,
                    confidence,
                    decision: Decision::Undecided,
                    previously_denied: false,
                    apply_outcome: None,
                    apply_error: None,
                });
            }
            ChangeVerdict::Ok(mut normalized) => {
                if kind == PlanKind::BackendCreate && normalized.get("port").is_none() {
                    let port = assign_create_port(cfg, &assigned_ports);
                    assigned_ports.insert(port);
                    normalized
                        .as_object_mut()
                        .expect("normalized change is an object")
                        .insert("port".into(), json!(port));
                }
                if kind == PlanKind::BackendCreate {
                    if let Some(p) = normalized.get("port").and_then(|v| v.as_u64()) {
                        assigned_ports.insert(p as u16);
                    }
                }
                let fp = fingerprint(kind, Some(&target), &normalized);
                let previously_denied = denied_fingerprints.contains(&fp);
                let decision = if kind.is_destructive() || previously_denied {
                    Decision::Denied
                } else {
                    Decision::Undecided
                };
                items.push(PlanItem {
                    id: 0,
                    fingerprint: fp,
                    kind,
                    target: Some(target),
                    change: normalized,
                    advice_text: None,
                    rationale,
                    expected_effect,
                    confidence,
                    decision,
                    previously_denied,
                    apply_outcome: None,
                    apply_error: None,
                });
            }
        }
    }

    // Advice items carry a fingerprint too (stable identity for
    // the UI), computed over their text.
    for item in items.iter_mut() {
        if item.fingerprint.is_empty() {
            item.fingerprint = fingerprint(
                PlanKind::Advice,
                item.target.as_deref(),
                &json!({ "text": item.advice_text.clone().unwrap_or_default() }),
            );
        }
    }

    let truncated = items.len().saturating_sub(MAX_ITEMS);
    items.truncate(MAX_ITEMS);
    for (i, item) in items.iter_mut().enumerate() {
        item.id = i + 1;
    }

    Ok(Plan {
        summary,
        directive: directive.to_string(),
        items,
        consulted_backend: consulted_backend.to_string(),
        created_at: now.to_rfc3339(),
        config_hash: config_hash.to_string(),
        dropped_invalid,
        truncated,
        dropped_fragments,
        raw_response: response.to_string(),
        context,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use manager_config::AppConfig;

    fn parse(response: &str) -> Result<Plan, ParseError> {
        parse_plan(
            response,
            &AppConfig::default_config(),
            "planner",
            "hash",
            json!({}),
            &HashSet::new(),
            "",
            Utc::now(),
        )
    }

    #[test]
    fn extracts_object_from_fences_and_prose() {
        let r = parse(
            "Here is the plan:\n```json\n{\"summary\":\"s\",\"items\":[{\"kind\":\"backend.set_context\",\"target\":\"coder\",\"change\":{\"n_ctx\":65536},\"rationale\":\"r\",\"confidence\":\"high\"}]}\n```\ndone",
        )
        .unwrap();
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].kind, PlanKind::BackendSetContext);
        assert_eq!(r.items[0].confidence, Confidence::High);
        assert_eq!(r.items[0].decision, Decision::Undecided);
    }

    #[test]
    fn no_object_fails_the_consultation() {
        assert!(matches!(
            parse("no json here"),
            Err(ParseError::NoJsonObject)
        ));
    }

    #[test]
    fn broken_item_is_dropped_and_counted() {
        let r = parse(
            "{\"items\":[{\"kind\":\"backend.set_context\",\"target\":\"coder\",\"change\":{\"n_ctx\":\"wide\"}},{\"kind\":\"backend.set_enabled\",\"target\":\"coder\",\"change\":{\"enabled\":true}}]}",
        )
        .unwrap();
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.dropped_invalid, 1);
        assert_eq!(r.dropped_fragments.len(), 1);
    }

    #[test]
    fn raw_flags_rewrite_demotes_to_advice() {
        let r = parse(
            "{\"items\":[{\"kind\":\"backend.set_flags\",\"target\":\"coder\",\"change\":{\"raw_flags\":\"--threads 4\"},\"rationale\":\"tune it\"}]}",
        )
        .unwrap();
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].kind, PlanKind::Advice);
        assert!(r.items[0]
            .advice_text
            .as_ref()
            .unwrap()
            .contains("--threads 4"));
    }

    #[test]
    fn unmapped_gateway_kind_demotes_unknown_kind_drops() {
        let r = parse(
            "{\"items\":[{\"kind\":\"gateway.set_port\",\"target\":\"gateway\",\"change\":{\"port\":5000},\"rationale\":\"move it\"},{\"kind\":\"banana.split\",\"change\":{}}]}",
        )
        .unwrap();
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].kind, PlanKind::Advice);
        assert_eq!(r.dropped_invalid, 1);
    }

    #[test]
    fn deletes_start_denied() {
        let r = parse(
            "{\"items\":[{\"kind\":\"backend.delete\",\"target\":\"coder\",\"change\":{},\"confidence\":\"high\"}]}",
        )
        .unwrap();
        assert_eq!(r.items[0].decision, Decision::Denied);
    }

    #[test]
    fn create_gets_a_free_port_assigned() {
        // Default config backends hold 8081..8084, gateway 4000.
        let r = parse(
            "{\"items\":[{\"kind\":\"backend.create\",\"target\":\"coder-2\",\"change\":{\"model_id\":\"qwen3-8b\"}}]}",
        )
        .unwrap();
        assert_eq!(r.items[0].change["port"], json!(8085));
    }

    #[test]
    fn previously_denied_fingerprint_arrives_denied() {
        let first = parse(
            "{\"items\":[{\"kind\":\"backend.set_context\",\"target\":\"coder\",\"change\":{\"n_ctx\":65536}}]}",
        )
        .unwrap();
        let fp = first.items[0].fingerprint.clone();
        let mut denied = HashSet::new();
        denied.insert(fp);
        let second = parse_plan(
            "{\"items\":[{\"kind\":\"backend.set_context\",\"target\":\"coder\",\"change\":{\"n_ctx\":65536}}]}",
            &AppConfig::default_config(),
            "planner",
            "hash",
            json!({}),
            &denied,
            "",
            Utc::now(),
        )
        .unwrap();
        assert_eq!(second.items[0].decision, Decision::Denied);
        assert!(second.items[0].previously_denied);
    }

    #[test]
    fn parse_plan_carries_the_directive_onto_the_plan() {
        // Addendum A.2: the directive that produced the plan is
        // stored verbatim on it.
        let r = parse_plan(
            "{\"items\":[{\"kind\":\"backend.set_enabled\",\"target\":\"coder\",\"change\":{\"enabled\":true}}]}",
            &AppConfig::default_config(),
            "planner",
            "hash",
            json!({}),
            &HashSet::new(),
            "the tool-runner feels slow",
            Utc::now(),
        )
        .unwrap();
        assert_eq!(r.directive, "the tool-runner feels slow");
        // The no-directive path stays empty, not absent.
        let plain = parse("{\"items\":[]}").unwrap();
        assert_eq!(plain.directive, "");
    }

    #[test]
    fn cap_truncates_at_25() {
        let mut items = String::new();
        for i in 0..30 {
            if i > 0 {
                items.push(',');
            }
            items.push_str(&format!(
                "{{\"kind\":\"backend.set_enabled\",\"target\":\"b{i}\",\"change\":{{\"enabled\":true}}}}"
            ));
        }
        let r = parse(&format!("{{\"items\":[{items}]}}")).unwrap();
        assert_eq!(r.items.len(), 25);
        assert_eq!(r.truncated, 5);
        assert_eq!(r.items[24].id, 25);
    }
}
