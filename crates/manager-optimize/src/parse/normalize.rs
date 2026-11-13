//! Per-kind change normalization: validate and normalize one item's
//! `change` payload, or decide it demotes to advice or drops.

use serde_json::{json, Map, Value};

use crate::plan::PlanKind;

pub(super) enum ChangeVerdict {
    /// Normalized, executable change payload.
    Ok(Value),
    /// Known kind, unexecutable payload: render as advice.
    Demote,
    /// Schema failure: drop the item.
    Drop,
}

pub(super) fn as_u32(v: &Value) -> Option<u32> {
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

pub(super) fn req_u32(obj: &Map<String, Value>, key: &str) -> Option<u32> {
    obj.get(key).and_then(as_u32)
}

pub(super) fn opt_u32(obj: &Map<String, Value>, key: &str, verdict: &mut bool) -> Option<u32> {
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

pub(super) fn req_str(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty())
}

pub(super) fn opt_str(obj: &Map<String, Value>, key: &str, ok: &mut bool) -> Option<String> {
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

pub(super) fn req_f32_vec(obj: &Map<String, Value>, key: &str) -> Option<Vec<Value>> {
    let arr = obj.get(key)?.as_array()?;
    let mut out = Vec::new();
    for v in arr {
        let f = v.as_f64()?;
        out.push(json!(f));
    }
    Some(out)
}

/// The `change` fields a kind's mapping admits (design §4): the
/// single source for both [`normalize_change`] (any other field
/// demotes the item to advice) and the structured-output schema
/// (`crate::schema`, Phase 3), so the schema the consulted model
/// is constrained to and the parser reading its answer cannot
/// drift apart. Kinds whose change is empty admit nothing.
pub(crate) fn change_fields(kind: PlanKind) -> &'static [&'static str] {
    match kind {
        PlanKind::BackendCreate => &[
            "model_id",
            "group",
            "port",
            "n_ctx",
            "n_batch",
            "tensor_split",
            "offload_layers",
            "moe_cache_mib",
        ],
        PlanKind::BackendDelete | PlanKind::GroupDelete | PlanKind::ModelDownload => &[],
        PlanKind::BackendSetContext => &["n_ctx"],
        PlanKind::BackendSetFlags => &["n_ctx", "n_batch"],
        PlanKind::BackendSetOffload => &["offload_layers", "moe_cache_mib"],
        PlanKind::BackendSetPlacement => &["tensor_split", "split_mode"],
        PlanKind::BackendSetModel => &["model_id"],
        PlanKind::BackendSetEnabled => &["enabled"],
        PlanKind::GroupSetMembers => &["members"],
        PlanKind::GroupSetStrategy => &["strategy"],
        PlanKind::GroupSetFallbacks => &["fallbacks"],
        PlanKind::GroupCreate => &["members", "strategy", "fallbacks"],
        PlanKind::ModelAdd => &["source_url", "quant", "sha256", "params_b"],
        PlanKind::Advice => &["text"],
    }
}

/// Validate and normalize one item's change payload for its kind.
/// Only the fields the kind's mapping admits survive, so what
/// validation and apply see is exactly what the app can execute.
pub(super) fn normalize_change(kind: PlanKind, change: &Value) -> ChangeVerdict {
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
    if matches!(
        kind,
        PlanKind::BackendDelete | PlanKind::GroupDelete | PlanKind::ModelDownload
    ) {
        return ChangeVerdict::Ok(json!({}));
    }
    let allowed = change_fields(kind);
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
                // A TCP port is 1..=65535. opt_u32 admits up to u32::MAX,
                // and later `as u16` casts silently truncated 73617 to
                // 8081 (the plan showed one port, apply used another)
                // and 65536 to 0 (finding P-7).
                if p == 0 || u16::try_from(p).is_err() {
                    return ChangeVerdict::Drop;
                }
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
            if let Some(n) = opt_u32(obj, "moe_cache_mib", &mut ok) {
                out.insert("moe_cache_mib".into(), json!(n));
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
        PlanKind::BackendSetOffload => {
            match req_u32(obj, "offload_layers") {
                Some(n) => {
                    out.insert("offload_layers".into(), json!(n));
                }
                None => return ChangeVerdict::Drop,
            }
            if let Some(n) = opt_u32(obj, "moe_cache_mib", &mut ok) {
                out.insert("moe_cache_mib".into(), json!(n));
            }
        }
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
            // A positive parameter count, when the proposal knows
            // it (a discovered candidate's real figure): the add's
            // executor passes it through, since an unregistered
            // non-catalog model has no other params_b source and
            // model_add refuses to guess. A malformed or
            // non-positive value is a schema failure.
            if let Some(v) = obj.get("params_b") {
                match v.as_f64() {
                    Some(p) if p > 0.0 => {
                        out.insert("params_b".into(), json!(p));
                    }
                    _ => return ChangeVerdict::Drop,
                }
            }
        }
        PlanKind::BackendDelete
        | PlanKind::GroupDelete
        | PlanKind::ModelDownload
        | PlanKind::Advice => unreachable!(),
    }
    if !ok {
        return ChangeVerdict::Drop;
    }
    ChangeVerdict::Ok(Value::Object(out))
}

/// The preserved text of an item demoted to advice: its reasoning
/// and the proposed change, so nothing the model said is lost when
/// the mutation is refused.
pub(super) fn demote_text(rationale: &str, expected_effect: &str, change: &Value) -> String {
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
pub(super) fn known_unmapped(kind: &str) -> bool {
    let Some((family, _)) = kind.split_once('.') else {
        return false;
    };
    matches!(
        family,
        "backend" | "group" | "model" | "gateway" | "wizard" | "update" | "mxc" | "winml"
    )
}
