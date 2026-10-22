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

use crate::experiment::{
    experiment_fingerprint, is_hf_form, ExperimentFlags, ExperimentPredicted, ExperimentProposal,
    ExperimentProvisioning, MAX_EXPERIMENTS,
};
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
        PlanKind::BackendDelete | PlanKind::GroupDelete | PlanKind::ModelDownload => {
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
        PlanKind::ModelAdd => &["source_url", "quant", "sha256", "params_b"],
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

/// One discovered candidate from the consultation's context
/// document (the `candidates` block `build_context` assembled
/// from the discovery pre-pass): a grounding source for
/// new-model proposals that are neither registered nor catalog
/// models (tracker #51).
struct CandidateGrounding {
    repo_id: String,
    file: String,
    params_b: f64,
}

/// The context document's discovered candidates, in the shape
/// the grounding passes consume.
fn candidates_from_context(context: &Value) -> Vec<CandidateGrounding> {
    context
        .get("candidates")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    let repo_id = c.get("repo_id")?.as_str()?.to_string();
                    let file = c.get("file")?.as_str()?.to_string();
                    let params_b = c.get("params_b").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    Some(CandidateGrounding {
                        repo_id,
                        file,
                        params_b,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The registration id derived for a discovered candidate: the
/// quant file's stem, lowered to the catalog's id conventions
/// (letters, digits, '.', '_', '-' kept; anything else becomes
/// '-'). Deterministic, so the same candidate always derives
/// the same id — in the context, in a proposal, and here.
fn candidate_model_id(file: &str) -> String {
    let stem = file.strip_suffix(".gguf").unwrap_or(file);
    stem.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// A synthesized `model.add` item, built exactly as an authored
/// one is normalized: fingerprint over kind + target + change,
/// the denial-memory decision rule, the create's own confidence
/// (the add exists to serve the create's proposal).
fn chained_model_add(
    target: &str,
    change: Value,
    rationale: String,
    confidence: Confidence,
    denied_fingerprints: &HashSet<String>,
) -> PlanItem {
    let fp = fingerprint(PlanKind::ModelAdd, Some(target), &change);
    let previously_denied = denied_fingerprints.contains(&fp);
    PlanItem {
        id: 0,
        fingerprint: fp,
        kind: PlanKind::ModelAdd,
        target: Some(target.to_string()),
        change,
        advice_text: None,
        rationale,
        expected_effect: "Model registered".to_string(),
        confidence,
        decision: if previously_denied {
            Decision::Denied
        } else {
            Decision::Undecided
        },
        previously_denied,
        provision_for_experiment: None,
        apply_outcome: None,
        apply_error: None,
    }
}

/// Ground every `backend.create`'s model reference (tracker
/// #51). Plan construction is the reliable layer for this rule —
/// the prompt states it, but a local model can still invent a
/// plausible model id (the field case: a create on
/// `qwen3.8-27b-moe`, offered with an Accept button, invalid at
/// validation, silently skipped at apply). For each create, in
/// plan order:
///
/// - model registered → the create stands;
/// - a `model.add` for the model already emitted (authored
///   earlier, or chained for an earlier create) → stands;
/// - an authored `model.add` for the model sits LATER in the
///   plan → it is moved ahead of the create (validation and
///   apply both run in plan order, so order is the defect);
/// - model in the curated catalog → a `model.add` carrying the
///   catalog entry's URL, quant, and hash is chained ahead;
/// - model naming a discovered candidate (its repo id, its
///   `repo/file`, or the id derived from its file) → a
///   `model.add` carrying the candidate's concrete HF resolve
///   URL and real `params_b` is chained ahead, and the create's
///   `model_id` is rewritten to the derived registration id;
/// - anything else → the create is EXCLUDED: recorded in the
///   plan's `excluded_items` with its reason, and stated in the
///   plan summary. Never silently dropped, never offered.
///
/// New-model proposals therefore only ever come from the
/// catalog or the discovery results — never free invention.
fn ground_create_models(
    items: Vec<PlanItem>,
    cfg: &AppConfig,
    context: &Value,
    denied_fingerprints: &HashSet<String>,
    summary: &mut String,
) -> (Vec<PlanItem>, Vec<crate::plan::ExcludedItem>) {
    let registered: HashSet<&str> = cfg.models.iter().map(|m| m.id.as_str()).collect();
    let catalog = manager_models::curated_models();
    let candidates = candidates_from_context(context);

    let mut out: Vec<PlanItem> = Vec::new();
    let mut excluded: Vec<crate::plan::ExcludedItem> = Vec::new();
    // Model ids an already-emitted model.add registers.
    let mut adds_available: HashSet<String> = HashSet::new();
    let mut pending = items;
    while !pending.is_empty() {
        let item = pending.remove(0);
        if item.kind == PlanKind::ModelAdd {
            if let Some(t) = &item.target {
                adds_available.insert(t.clone());
            }
            out.push(item);
            continue;
        }
        if item.kind != PlanKind::BackendCreate {
            out.push(item);
            continue;
        }
        let target = item.target.clone().unwrap_or_default();
        let model_id = item
            .change
            .get("model_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // Registered, or an add already emitted registers it.
        if registered.contains(model_id.as_str()) || adds_available.contains(&model_id) {
            out.push(item);
            continue;
        }
        // An authored add for this model sits later in the plan:
        // move it ahead of the create.
        if let Some(pos) = pending
            .iter()
            .position(|p| p.kind == PlanKind::ModelAdd && p.target.as_deref() == Some(model_id.as_str()))
        {
            let add = pending.remove(pos);
            adds_available.insert(model_id.clone());
            out.push(add);
            out.push(item);
            continue;
        }
        // A catalog model: chain its registration from the
        // catalog entry itself (URL, quant, pinned hash).
        if let Some(spec) = catalog.iter().find(|s| s.id == model_id) {
            let mut change = json!({
                "source_url": spec.url,
                "quant": spec.quant,
            });
            if !spec.sha256.is_empty() {
                change["sha256"] = json!(spec.sha256);
            }
            let add = chained_model_add(
                &model_id,
                change,
                format!(
                    "Registers the catalog model '{model_id}' ({}), chained by the app at plan build: the backend.create for '{target}' names it, and it is in the curated catalog but not yet registered.",
                    spec.name
                ),
                item.confidence,
                denied_fingerprints,
            );
            adds_available.insert(model_id.clone());
            out.push(add);
            out.push(item);
            continue;
        }
        // A discovered candidate, named by repo, repo/file, or
        // the derived registration id.
        let candidate = candidates.iter().find(|c| {
            model_id == c.repo_id
                || model_id == format!("{}/{}", c.repo_id, c.file)
                || model_id == candidate_model_id(&c.file)
        });
        if let Some(c) = candidate {
            let derived = candidate_model_id(&c.file);
            let mut item = item;
            if !adds_available.contains(&derived) && !registered.contains(derived.as_str()) {
                // An authored add for the derived id later in
                // the plan serves the create: move it ahead.
                if let Some(pos) = pending.iter().position(|p| {
                    p.kind == PlanKind::ModelAdd && p.target.as_deref() == Some(derived.as_str())
                }) {
                    let add = pending.remove(pos);
                    adds_available.insert(derived.clone());
                    out.push(add);
                } else {
                    let mut change = json!({
                        "source_url": format!(
                            "{}/{}/resolve/main/{}",
                            crate::discover::HF_DEFAULT_BASE_URL, c.repo_id, c.file
                        ),
                    });
                    if c.params_b > 0.0 {
                        change["params_b"] = json!(c.params_b);
                    }
                    let add = chained_model_add(
                        &derived,
                        change,
                        format!(
                            "Registers the discovered model '{}/{}' (Hugging Face discovery pre-pass for this consultation), chained by the app at plan build: the backend.create for '{target}' is grounded in it.",
                            c.repo_id, c.file
                        ),
                        item.confidence,
                        denied_fingerprints,
                    );
                    adds_available.insert(derived.clone());
                    out.push(add);
                }
            }
            // The create must name the id the add registers (or
            // the already-registered model with that id): rewrite
            // and re-fingerprint, as port assignment rewrites a
            // create's port at parse.
            if item
                .change
                .get("model_id")
                .and_then(|v| v.as_str())
                != Some(derived.as_str())
            {
                item.change["model_id"] = json!(derived);
                item.fingerprint = fingerprint(
                    PlanKind::BackendCreate,
                    item.target.as_deref(),
                    &item.change,
                );
                item.previously_denied = denied_fingerprints.contains(&item.fingerprint);
                if item.previously_denied {
                    item.decision = Decision::Denied;
                }
            }
            out.push(item);
            continue;
        }
        // Ungroundable: exclude, record, and state it in the
        // summary — the operator sees what was refused and why.
        let reason = format!(
            "model '{model_id}' is not registered, is not in the catalog, and was not among the models discovered for this consultation, so a backend on it can never be created"
        );
        if !summary.is_empty() {
            summary.push(' ');
        }
        summary.push_str(&format!(
            "Excluded at plan build: backend.create '{target}' names {reason}."
        ));
        excluded.push(crate::plan::ExcludedItem {
            kind: PlanKind::BackendCreate,
            target: item.target.clone(),
            reason,
        });
    }
    (out, excluded)
}

/// Ground every authored `model.add`'s parameter count in the
/// consultation's discovered candidates (tracker #38). An
/// authored add for a discovered candidate often names only the
/// candidate's resolve URL — the consultant was not asked to
/// repeat a figure the app already holds — but an unknown-size,
/// non-catalog add without a count is invalid at validation and
/// unappliable (model_add refuses to guess, tracker #32). When
/// the add's source URL names a candidate from this
/// consultation's context (its repo/file, or its file name), the
/// candidate's real `params_b` is written onto the item, so the
/// value the consultation grounded is the value validation and
/// apply carry. An item that already carries a positive count
/// keeps it — the consultant's (or a later operator edit's)
/// value is never overwritten. Re-fingerprinted after the
/// rewrite, as the create grounding's model_id rewrite is.
fn ground_model_add_params(
    items: Vec<PlanItem>,
    context: &Value,
    denied_fingerprints: &HashSet<String>,
) -> Vec<PlanItem> {
    let candidates = candidates_from_context(context);
    if candidates.is_empty() {
        return items;
    }
    items
        .into_iter()
        .map(|mut item| {
            if item.kind != PlanKind::ModelAdd {
                return item;
            }
            let carried = item
                .change
                .get("params_b")
                .and_then(|v| v.as_f64())
                .filter(|p| *p > 0.0);
            if carried.is_some() {
                return item;
            }
            let url = item
                .change
                .get("source_url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let file_name = url.rsplit('/').next().unwrap_or("");
            let candidate = candidates.iter().find(|c| {
                c.params_b > 0.0
                    && (c.file == file_name
                        || url.contains(&format!("{}/{}", c.repo_id, c.file)))
            });
            if let Some(c) = candidate {
                item.change["params_b"] = json!(c.params_b);
                item.fingerprint = fingerprint(
                    PlanKind::ModelAdd,
                    item.target.as_deref(),
                    &item.change,
                );
                item.previously_denied = denied_fingerprints.contains(&item.fingerprint);
                if item.previously_denied {
                    item.decision = Decision::Denied;
                }
            }
            item
        })
        .collect()
}

/// A synthesized `model.download` item for experiment
/// provisioning, built like [`chained_model_add`]: fingerprint
/// over kind + target + the empty change, the denial-memory
/// decision rule, and the experiment's own initial decision (a
/// proposal that starts denied — it names backends to stop, or
/// was denied before — starts with its provisioning denied
/// too; accepting the experiment accepts the steps).
fn chained_model_download(
    model_id: &str,
    experiment: &ExperimentProposal,
    denied_fingerprints: &HashSet<String>,
) -> PlanItem {
    let change = json!({});
    let fp = fingerprint(PlanKind::ModelDownload, Some(model_id), &change);
    let previously_denied = denied_fingerprints.contains(&fp);
    PlanItem {
        id: 0,
        fingerprint: fp,
        kind: PlanKind::ModelDownload,
        target: Some(model_id.to_string()),
        change,
        advice_text: None,
        rationale: format!(
            "Downloads the GGUF file for '{model_id}' through the existing model download machinery, chained by the app at plan build: experiment E{} runs its candidate from disk and experiments never download at run time, so without this step the experiment cannot run. The model's source URL and any hash are model-supplied and unverified — confirm the pinned hash in the Models view before downloading.",
            experiment.id
        ),
        expected_effect: "Model file downloaded".to_string(),
        confidence: Confidence::Medium,
        decision: if previously_denied || experiment.decision == Decision::Denied {
            Decision::Denied
        } else {
            Decision::Undecided
        },
        previously_denied,
        provision_for_experiment: Some(experiment.id),
        apply_outcome: None,
        apply_error: None,
    }
}

/// Ground every experiment's candidate model (experiment
/// provisioning). An experiment whose candidate is not
/// registered can never run — experiments never download, so
/// preflight is a dead end — unless the plan itself provisions
/// the model. For each unregistered candidate, in experiment
/// order:
///
/// - a `model.add` already in the plan registers it → link
///   that item, and chain only the download;
/// - the candidate names a discovered candidate (its repo id,
///   its `repo/file`, or the id derived from its file) → chain
///   a `model.add` carrying the candidate's concrete HF
///   resolve URL and real `params_b` (the tracker #38 rule:
///   an unregistered non-catalog model has no other params_b
///   source), plus the download; the proposal's candidate is
///   rewritten to the derived registration id, as a grounded
///   `backend.create`'s `model_id` is;
/// - a catalog id → chain the registration from the catalog
///   entry (URL, quant, pinned hash), plus the download;
/// - an HF repo/file form → chain the registration from the
///   candidate's own resolve URL, plus the download;
/// - anything else → no steps can be offered: the summary
///   states it, and plan-aware validation states precisely
///   what is missing (not registered / not in the catalog /
///   not discovered / not an HF repo/file).
///
/// Chained items are marked `provision_for_experiment` and
/// start with the experiment's own initial decision; a linked
/// pre-existing item is only referenced from the proposal's
/// `provisioning` record (denying the experiment must not deny
/// a registration other items rely on). Returns, per
/// experiment, the provisioning items' fingerprints in plan
/// order — the caller resolves them to item ids once final
/// ids are assigned.
fn ground_experiment_provisioning(
    items: &mut Vec<PlanItem>,
    experiments: &mut [ExperimentProposal],
    cfg: &AppConfig,
    context: &Value,
    denied_fingerprints: &HashSet<String>,
    summary: &mut String,
) -> Vec<(usize, Vec<String>)> {
    let registered: HashSet<&str> = cfg.models.iter().map(|m| m.id.as_str()).collect();
    let catalog = manager_models::curated_models();
    let candidates = candidates_from_context(context);
    let mut links: Vec<(usize, Vec<String>)> = Vec::new();

    for exp in experiments.iter_mut() {
        let candidate = exp.candidate_model_id.trim().to_string();
        if candidate.is_empty() || registered.contains(candidate.as_str()) {
            continue;
        }
        // Resolve the candidate to (registration id, add change
        // to chain) — a None change means a plan item already
        // registers the id.
        let mut resolved: Option<(String, Option<Value>)> = None;
        let mut already_registered_as: Option<String> = None;
        if items
            .iter()
            .any(|i| i.kind == PlanKind::ModelAdd && i.target.as_deref() == Some(candidate.as_str()))
        {
            resolved = Some((candidate.clone(), None));
        } else if let Some(c) = candidates.iter().find(|c| {
            candidate == c.repo_id
                || candidate == format!("{}/{}", c.repo_id, c.file)
                || candidate == candidate_model_id(&c.file)
        }) {
            let derived = candidate_model_id(&c.file);
            if registered.contains(derived.as_str()) {
                already_registered_as = Some(derived);
            } else if items
                .iter()
                .any(|i| i.kind == PlanKind::ModelAdd && i.target.as_deref() == Some(derived.as_str()))
            {
                resolved = Some((derived, None));
            } else {
                let mut change = json!({
                    "source_url": format!(
                        "{}/{}/resolve/main/{}",
                        crate::discover::HF_DEFAULT_BASE_URL, c.repo_id, c.file
                    ),
                });
                if c.params_b > 0.0 {
                    change["params_b"] = json!(c.params_b);
                }
                resolved = Some((derived, Some(change)));
            }
        } else if let Some(spec) = catalog.iter().find(|s| s.id == candidate) {
            let mut change = json!({
                "source_url": spec.url,
                "quant": spec.quant,
            });
            if !spec.sha256.is_empty() {
                change["sha256"] = json!(spec.sha256);
            }
            resolved = Some((candidate.clone(), Some(change)));
        } else if is_hf_form(&candidate) {
            let (prefix, file) = candidate
                .rsplit_once('/')
                .expect("HF form always contains a slash");
            let derived = candidate_model_id(file);
            if registered.contains(derived.as_str()) {
                already_registered_as = Some(derived);
            } else if items
                .iter()
                .any(|i| i.kind == PlanKind::ModelAdd && i.target.as_deref() == Some(derived.as_str()))
            {
                resolved = Some((derived, None));
            } else {
                let change = json!({
                    "source_url": format!(
                        "{}/{}/resolve/main/{}",
                        crate::discover::HF_DEFAULT_BASE_URL, prefix, file
                    ),
                });
                resolved = Some((derived, Some(change)));
            }
        }
        // The candidate resolves to a model that IS registered,
        // under the derived id: point the proposal at it; no
        // provisioning is needed.
        if let Some(derived) = already_registered_as {
            exp.candidate_model_id = derived;
            exp.fingerprint =
                experiment_fingerprint(&exp.candidate_model_id, &exp.flags, &exp.serves);
            exp.previously_denied = denied_fingerprints.contains(&exp.fingerprint);
            if exp.previously_denied {
                exp.decision = Decision::Denied;
            }
            continue;
        }
        let Some((model_id, add_change)) = resolved else {
            if !summary.is_empty() {
                summary.push(' ');
            }
            summary.push_str(&format!(
                "Experiment E{} candidate '{candidate}' cannot be provisioned: it is not a registered model, is not in the catalog, and was not among the models discovered for this consultation, so the plan offers no registration or download step for it.",
                exp.id
            ));
            continue;
        };
        // Chain the registration when the plan does not already
        // carry it, then the download (or link the one an
        // earlier experiment chained for the same model).
        let mut link_fps: Vec<String> = Vec::new();
        match add_change {
            Some(change) => {
                let mut add = chained_model_add(
                    &model_id,
                    change,
                    format!(
                        "Registers the model '{model_id}', chained by the app at plan build: experiment E{} proposes it as its candidate, and an unregistered candidate can never run.",
                        exp.id
                    ),
                    Confidence::Medium,
                    denied_fingerprints,
                );
                add.provision_for_experiment = Some(exp.id);
                if exp.decision == Decision::Denied {
                    add.decision = Decision::Denied;
                }
                link_fps.push(add.fingerprint.clone());
                items.push(add);
            }
            None => {
                if let Some(add) = items.iter().find(|i| {
                    i.kind == PlanKind::ModelAdd && i.target.as_deref() == Some(model_id.as_str())
                }) {
                    link_fps.push(add.fingerprint.clone());
                }
            }
        }
        if let Some(dl) = items.iter().find(|i| {
            i.kind == PlanKind::ModelDownload && i.target.as_deref() == Some(model_id.as_str())
        }) {
            link_fps.push(dl.fingerprint.clone());
        } else {
            let dl = chained_model_download(&model_id, exp, denied_fingerprints);
            link_fps.push(dl.fingerprint.clone());
            items.push(dl);
        }
        // The proposal names the registration id from here on,
        // so validation and preflight resolve the same model
        // the plan provisions.
        if exp.candidate_model_id != model_id {
            exp.candidate_model_id = model_id.clone();
            exp.fingerprint =
                experiment_fingerprint(&exp.candidate_model_id, &exp.flags, &exp.serves);
            exp.previously_denied = denied_fingerprints.contains(&exp.fingerprint);
            if exp.previously_denied {
                exp.decision = Decision::Denied;
            }
        }
        exp.provisioning = Some(ExperimentProvisioning {
            model_id,
            item_ids: Vec::new(),
        });
        links.push((exp.id, link_fps));
    }
    links
}

/// Re-ground experiment provisioning on a plan being LOADED.
///
/// Plan building grounds every experiment's provisioning (see
/// [`ground_experiment_provisioning`]), but a plan file written
/// before that grounding existed — or by any build that lacked
/// it — stores `provisioning: None`, and every later surface
/// then dead-ends the experiment: plan-aware validation calls
/// the candidate unresolvable even when the plan itself carries
/// the `model.add` that registers it, and the runner fails it
/// as undownloadable (Pete's Oct 18 plan, run on Oct 19, 2026).
/// This pass applies the build-time resolution rules to a
/// stored plan: a `model.add` already in the plan links (and
/// only the download is chained); a catalog id or an HF
/// repo/file candidate chains its registration and download;
/// discovery resolves through the candidates recorded in the
/// plan's own stored context. Chained items are appended after
/// the plan's own items with fresh ids, mirroring how build
/// appends them after the item cap. Returns whether the plan
/// changed; the caller persists a changed plan so the repair
/// is durable. Idempotent: an experiment that already carries
/// provisioning is never touched.
pub fn reground_experiment_provisioning(plan: &mut Plan, cfg: &AppConfig) -> bool {
    let registered: HashSet<&str> = cfg.models.iter().map(|m| m.id.as_str()).collect();
    let catalog = manager_models::curated_models();
    let denied: HashSet<String> = HashSet::new();
    let mut changed = false;
    let mut next_id = plan.items.iter().map(|i| i.id).max().unwrap_or(0);

    for exp_idx in 0..plan.experiments.len() {
        if plan.experiments[exp_idx].provisioning.is_some() {
            continue;
        }
        let candidate = plan.experiments[exp_idx]
            .candidate_model_id
            .trim()
            .to_string();
        if candidate.is_empty() || registered.contains(candidate.as_str()) {
            continue;
        }
        let exp_id = plan.experiments[exp_idx].id;
        // Resolve exactly as build time does: (registration
        // id, add change to chain) — a None change means a plan
        // item already registers the id. Discovery is consulted
        // through the candidates the plan's own stored context
        // carried (the same set the payload validates against).
        let mut resolved: Option<(String, Option<Value>)> = None;
        if plan.items.iter().any(|i| {
            i.kind == PlanKind::ModelAdd && i.target.as_deref() == Some(candidate.as_str())
        }) {
            resolved = Some((candidate.clone(), None));
        } else if let Some(c) = candidates_from_context(&plan.context).iter().find(|c| {
            candidate == c.repo_id
                || candidate == format!("{}/{}", c.repo_id, c.file)
                || candidate == candidate_model_id(&c.file)
        }) {
            let derived = candidate_model_id(&c.file);
            if registered.contains(derived.as_str()) {
                plan.experiments[exp_idx].candidate_model_id = derived;
                let fp = {
                    let e = &plan.experiments[exp_idx];
                    experiment_fingerprint(&e.candidate_model_id, &e.flags, &e.serves)
                };
                plan.experiments[exp_idx].fingerprint = fp;
                changed = true;
                continue;
            } else if plan.items.iter().any(|i| {
                i.kind == PlanKind::ModelAdd && i.target.as_deref() == Some(derived.as_str())
            }) {
                resolved = Some((derived, None));
            } else {
                let mut change = json!({
                    "source_url": format!(
                        "{}/{}/resolve/main/{}",
                        crate::discover::HF_DEFAULT_BASE_URL, c.repo_id, c.file
                    ),
                });
                if c.params_b > 0.0 {
                    change["params_b"] = json!(c.params_b);
                }
                resolved = Some((derived, Some(change)));
            }
        } else if let Some(spec) = catalog.iter().find(|s| s.id == candidate) {
            let mut change = json!({
                "source_url": spec.url,
                "quant": spec.quant,
            });
            if !spec.sha256.is_empty() {
                change["sha256"] = json!(spec.sha256);
            }
            resolved = Some((candidate.clone(), Some(change)));
        } else if is_hf_form(&candidate) {
            let (prefix, file) = candidate
                .rsplit_once('/')
                .expect("HF form always contains a slash");
            let derived = candidate_model_id(file);
            if registered.contains(derived.as_str()) {
                // The proposal names the model under its derived
                // registration id, as build time would have left
                // it; no provisioning is needed.
                plan.experiments[exp_idx].candidate_model_id = derived;
                let fp = {
                    let e = &plan.experiments[exp_idx];
                    experiment_fingerprint(&e.candidate_model_id, &e.flags, &e.serves)
                };
                plan.experiments[exp_idx].fingerprint = fp;
                changed = true;
                continue;
            } else if plan.items.iter().any(|i| {
                i.kind == PlanKind::ModelAdd && i.target.as_deref() == Some(derived.as_str())
            }) {
                resolved = Some((derived, None));
            } else {
                let change = json!({
                    "source_url": format!(
                        "{}/{}/resolve/main/{}",
                        crate::discover::HF_DEFAULT_BASE_URL, prefix, file
                    ),
                });
                resolved = Some((derived, Some(change)));
            }
        }
        let Some((model_id, add_change)) = resolved else {
            continue;
        };
        let mut item_ids: Vec<usize> = Vec::new();
        match add_change {
            Some(change) => {
                let mut add = chained_model_add(
                    &model_id,
                    change,
                    format!(
                        "Registers the model '{model_id}', chained when a stored plan was re-grounded at load: experiment E{exp_id} proposes it as its candidate, and an unregistered candidate can never run."
                    ),
                    Confidence::Medium,
                    &denied,
                );
                add.provision_for_experiment = Some(exp_id);
                if plan.experiments[exp_idx].decision == Decision::Denied {
                    add.decision = Decision::Denied;
                }
                next_id += 1;
                add.id = next_id;
                item_ids.push(add.id);
                plan.items.push(add);
            }
            None => {
                if let Some(add) = plan.items.iter().find(|i| {
                    i.kind == PlanKind::ModelAdd && i.target.as_deref() == Some(model_id.as_str())
                }) {
                    item_ids.push(add.id);
                }
            }
        }
        if let Some(dl) = plan.items.iter().find(|i| {
            i.kind == PlanKind::ModelDownload && i.target.as_deref() == Some(model_id.as_str())
        }) {
            item_ids.push(dl.id);
        } else {
            let mut dl = chained_model_download(&model_id, &plan.experiments[exp_idx], &denied);
            next_id += 1;
            dl.id = next_id;
            item_ids.push(dl.id);
            plan.items.push(dl);
        }
        if plan.experiments[exp_idx].candidate_model_id != model_id {
            plan.experiments[exp_idx].candidate_model_id = model_id.clone();
            let fp = {
                let e = &plan.experiments[exp_idx];
                experiment_fingerprint(&e.candidate_model_id, &e.flags, &e.serves)
            };
            plan.experiments[exp_idx].fingerprint = fp;
        }
        plan.experiments[exp_idx].provisioning =
            Some(ExperimentProvisioning { model_id, item_ids });
        changed = true;
    }
    changed
}

/// The entities one item creates, requires, and removes, for
/// the dependency-ordering pass. Entities are keyed like the
/// validation simulation's write scopes: `m:<model id>`,
/// `b:<backend id>`, `g:<group name>`.
fn item_entities(item: &PlanItem) -> (Vec<String>, Vec<String>, Vec<String>) {
    let target = item.target.clone().unwrap_or_default();
    let mut creates: Vec<String> = Vec::new();
    let mut requires: Vec<String> = Vec::new();
    let mut removes: Vec<String> = Vec::new();
    // Group member/fallback entries that name backends (a
    // `host:port` member is an external endpoint, and
    // "openrouter" is the cloud fallback — neither is a backend
    // this plan can create or delete).
    let backend_refs = |change: &Value, key: &str| -> Vec<String> {
        change
            .get(key)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| !s.contains(':') && *s != "openrouter")
                    .map(|s| format!("b:{s}"))
                    .collect()
            })
            .unwrap_or_default()
    };
    match item.kind {
        PlanKind::ModelAdd => creates.push(format!("m:{target}")),
        PlanKind::ModelDownload => requires.push(format!("m:{target}")),
        PlanKind::BackendCreate => {
            creates.push(format!("b:{target}"));
            if let Some(m) = item.change.get("model_id").and_then(|v| v.as_str()) {
                requires.push(format!("m:{m}"));
            }
        }
        PlanKind::BackendDelete => {
            requires.push(format!("b:{target}"));
            removes.push(format!("b:{target}"));
        }
        PlanKind::BackendSetModel => {
            requires.push(format!("b:{target}"));
            if let Some(m) = item.change.get("model_id").and_then(|v| v.as_str()) {
                requires.push(format!("m:{m}"));
            }
        }
        PlanKind::BackendSetContext
        | PlanKind::BackendSetFlags
        | PlanKind::BackendSetOffload
        | PlanKind::BackendSetPlacement
        | PlanKind::BackendSetEnabled => {
            requires.push(format!("b:{target}"));
        }
        PlanKind::GroupCreate => {
            creates.push(format!("g:{target}"));
            requires.extend(backend_refs(&item.change, "members"));
            requires.extend(backend_refs(&item.change, "fallbacks"));
        }
        PlanKind::GroupSetMembers => {
            requires.push(format!("g:{target}"));
            requires.extend(backend_refs(&item.change, "members"));
        }
        PlanKind::GroupSetFallbacks => {
            requires.push(format!("g:{target}"));
            requires.extend(backend_refs(&item.change, "fallbacks"));
        }
        PlanKind::GroupSetStrategy => {
            requires.push(format!("g:{target}"));
        }
        PlanKind::GroupDelete => {
            requires.push(format!("g:{target}"));
            removes.push(format!("g:{target}"));
        }
        PlanKind::Advice => {}
    }
    (creates, requires, removes)
}

/// Dependency ordering (plan-build pass): reorder items so a
/// prerequisite always precedes the item that needs it — a
/// model registration before any backend or download naming
/// the model, a backend creation before routing items naming
/// it, routing updates before a delete that removes what they
/// reference, deletes last among their dependents.
///
/// Edges derive from [`item_entities`]: creator → referencer,
/// and referencer → remover (an item that uses an entity runs
/// before the item that removes it). Placement is a depth-first
/// walk in emitted order: an item is emitted only after its
/// in-plan predecessors, and a predecessor is pulled earlier
/// only when an edge forces it — so where no edge applies, the
/// model's emitted order (its intent) is preserved exactly.
/// A dependency cycle cannot be ordered; it is broken at the
/// point of detection, keeping every item (validation reports
/// the genuine problem on the affected rows).
fn order_items_by_dependency(items: Vec<PlanItem>) -> Vec<PlanItem> {
    let n = items.len();
    let mut creators: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut removers: std::collections::HashMap<String, Vec<usize>> = std::collections::HashMap::new();
    let mut requires: Vec<Vec<String>> = Vec::with_capacity(n);
    for (i, item) in items.iter().enumerate() {
        let (creates, reqs, removes) = item_entities(item);
        for e in creates {
            creators.entry(e).or_insert(i);
        }
        for e in removes {
            removers.entry(e).or_default().push(i);
        }
        requires.push(reqs);
    }
    // predecessors[i]: in-plan items that must be emitted
    // before item i.
    let mut predecessors: Vec<Vec<usize>> = vec![Vec::new(); n];
    for i in 0..n {
        for e in &requires[i] {
            if let Some(&c) = creators.get(e) {
                if c != i && !predecessors[i].contains(&c) {
                    predecessors[i].push(c);
                }
            }
            if let Some(rs) = removers.get(e) {
                for &r in rs {
                    // Item i uses the entity; the remover runs
                    // after it.
                    if r != i && !predecessors[r].contains(&i) {
                        predecessors[r].push(i);
                    }
                }
            }
        }
    }
    for preds in predecessors.iter_mut() {
        preds.sort_unstable();
    }
    fn visit(
        i: usize,
        predecessors: &[Vec<usize>],
        state: &mut [u8],
        out: &mut Vec<usize>,
    ) {
        // state: 0 = unvisited, 1 = on the current path (cycle
        // guard), 2 = emitted.
        if state[i] != 0 {
            return;
        }
        state[i] = 1;
        for &p in &predecessors[i] {
            visit(p, predecessors, state, out);
        }
        state[i] = 2;
        out.push(i);
    }
    let mut state = vec![0u8; n];
    let mut order: Vec<usize> = Vec::with_capacity(n);
    for i in 0..n {
        visit(i, &predecessors, &mut state, &mut order);
    }
    let mut slots: Vec<Option<PlanItem>> = items.into_iter().map(Some).collect();
    order
        .into_iter()
        .map(|i| slots[i].take().expect("each item is emitted exactly once"))
        .collect()
}

/// Parse the sibling `experiments` array (addendum A.3) with
/// the same salvage discipline as items: per-entry drop
/// (counted, fragments kept in the plan's dropped fragments),
/// cap [`MAX_EXPERIMENTS`]. An entry whose `serves` is empty
/// or missing is dropped under the same counting — a proposal
/// serving neither the directive nor a purpose need is advice,
/// not an experiment, and an experiment-shaped entry carries
/// no advice text worth demoting it into. Proposals naming
/// backends in `requires_stopping` start denied (the deletes'
/// precedent); a proposal whose fingerprint was denied before
/// also starts denied.
fn parse_experiments(
    root: &Value,
    denied_fingerprints: &HashSet<String>,
    dropped_fragments: &mut Vec<String>,
) -> (Vec<ExperimentProposal>, usize, usize) {
    let Some(raw_items) = root.get("experiments").and_then(|v| v.as_array()) else {
        return (Vec::new(), 0, 0);
    };
    let mut out: Vec<ExperimentProposal> = Vec::new();
    let mut dropped = 0usize;
    for raw in raw_items {
        let fragment = serde_json::to_string(raw).unwrap_or_default();
        let mut drop_entry = |dropped: &mut usize| {
            *dropped += 1;
            dropped_fragments.push(fragment.clone());
        };
        let Some(obj) = raw.as_object() else {
            drop_entry(&mut dropped);
            continue;
        };
        let Some(candidate) = req_str(obj, "candidate_model_id") else {
            drop_entry(&mut dropped);
            continue;
        };
        // `serves` is required (§A.2/A.3): empty or missing
        // drops the entry under salvage counting.
        let Some(serves) = req_str(obj, "serves") else {
            drop_entry(&mut dropped);
            continue;
        };
        // Flags: an absent block is all-defaults; a present
        // block with a malformed field drops the entry.
        let mut flags = ExperimentFlags::default();
        let mut flags_ok = true;
        if let Some(fv) = obj.get("flags") {
            let Some(fo) = fv.as_object() else {
                drop_entry(&mut dropped);
                continue;
            };
            if fo.contains_key("n_ctx") {
                match fo.get("n_ctx").and_then(as_u32) {
                    Some(n) => flags.n_ctx = Some(n),
                    None => flags_ok = false,
                }
            }
            if fo.contains_key("offload_layers") {
                match fo.get("offload_layers").and_then(as_u32) {
                    Some(n) => flags.offload_layers = Some(n),
                    None => flags_ok = false,
                }
            }
            if fo.contains_key("tensor_split") {
                match req_f32_vec(fo, "tensor_split") {
                    Some(v) => {
                        flags.tensor_split = Some(
                            v.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect(),
                        );
                    }
                    None => flags_ok = false,
                }
            }
        }
        if !flags_ok {
            drop_entry(&mut dropped);
            continue;
        }
        // requires_stopping: absent = []; a present non-array
        // (or non-string entries) drops the entry — the stop
        // claim must be exact.
        let mut requires_stopping: Vec<String> = Vec::new();
        if let Some(rs) = obj.get("requires_stopping") {
            match rs.as_array() {
                Some(arr) => {
                    let mut ok = true;
                    for v in arr {
                        match v.as_str() {
                            Some(s) if !s.trim().is_empty() => {
                                requires_stopping.push(s.trim().to_string())
                            }
                            _ => ok = false,
                        }
                    }
                    if !ok {
                        drop_entry(&mut dropped);
                        continue;
                    }
                }
                None => {
                    drop_entry(&mut dropped);
                    continue;
                }
            }
        }
        let predicted = ExperimentPredicted {
            predicted_per_second: obj
                .get("predicted")
                .and_then(|p| p.get("predicted_per_second"))
                .and_then(|v| v.as_f64()),
            fit: obj
                .get("predicted")
                .and_then(|p| p.get("fit"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        };
        let fp = experiment_fingerprint(&candidate, &flags, &serves);
        let previously_denied = denied_fingerprints.contains(&fp);
        let decision = if !requires_stopping.is_empty() || previously_denied {
            Decision::Denied
        } else {
            Decision::Undecided
        };
        out.push(ExperimentProposal {
            id: 0,
            fingerprint: fp,
            candidate_model_id: candidate,
            flags,
            serves,
            hypothesis: obj
                .get("hypothesis")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            predicted,
            requires_stopping,
            run_eval: obj
                .get("run_eval")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            decision,
            previously_denied,
            provisioning: None,
            result: None,
            estimate: None,
        });
    }
    let truncated = out.len().saturating_sub(MAX_EXPERIMENTS);
    out.truncate(MAX_EXPERIMENTS);
    for (i, e) in out.iter_mut().enumerate() {
        e.id = i + 1;
    }
    (out, dropped, truncated)
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
                        provision_for_experiment: None,
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
                        provision_for_experiment: None,
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
                    provision_for_experiment: None,
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
                    provision_for_experiment: None,
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

    // Ground every backend.create's model reference (tracker
    // #51): chain the model.add that registers an unregistered
    // catalog or discovered model ahead of its create, and
    // exclude a create whose model resolves to neither —
    // recorded on the plan and stated in the summary.
    let mut summary = summary;
    let (items, excluded_items) =
        ground_create_models(items, cfg, &context, denied_fingerprints, &mut summary);

    // Ground authored model.add counts in the consultation's
    // discovered candidates (tracker #38): an add naming a
    // candidate's file carries that candidate's real params_b,
    // so validation and apply see the value instead of the add
    // failing for a count the app already knew.
    let items = ground_model_add_params(items, &context, denied_fingerprints);

    // Dependency ordering: prerequisites before the items
    // that need them (registration before creation, creation
    // before routing, routing before deletes). Validation and
    // apply both run in plan order, so this order is the one
    // every later stage judges. Runs before truncation so the
    // cap cuts dependents (which sit late) before the
    // prerequisites they need.
    let mut items = order_items_by_dependency(items);

    let truncated = items.len().saturating_sub(MAX_ITEMS);
    items.truncate(MAX_ITEMS);

    // Experiments (§A.3): the sibling array, parsed after the
    // items so its dropped fragments append to the same list.
    let (mut experiments, experiments_dropped_invalid, experiments_truncated) =
        parse_experiments(&root, denied_fingerprints, &mut dropped_fragments);

    // Experiment provisioning: chain the registration +
    // download items an unregistered experiment candidate
    // needs in order to run, linked from the proposal. The
    // chained items are app-built (like the create grounding's
    // adds), so they are appended after the 25-item cap, which
    // bounds the consultant's own output; the ordering pass
    // then runs once more over the whole plan so provisioning
    // items land at their dependency positions (registration
    // ahead of the download and of anything naming the model).
    let provision_links = ground_experiment_provisioning(
        &mut items,
        &mut experiments,
        cfg,
        &context,
        denied_fingerprints,
        &mut summary,
    );
    let mut items = order_items_by_dependency(items);
    for (i, item) in items.iter_mut().enumerate() {
        item.id = i + 1;
    }
    // Provisioning links were recorded by fingerprint (stable
    // across ordering); resolve them to final item ids.
    if !provision_links.is_empty() {
        let id_by_fingerprint: std::collections::HashMap<&str, usize> = items
            .iter()
            .map(|i| (i.fingerprint.as_str(), i.id))
            .collect();
        for (exp_id, fps) in &provision_links {
            if let Some(exp) = experiments.iter_mut().find(|e| e.id == *exp_id) {
                if let Some(prov) = exp.provisioning.as_mut() {
                    prov.item_ids = fps
                        .iter()
                        .filter_map(|fp| id_by_fingerprint.get(fp.as_str()).copied())
                        .collect();
                }
            }
        }
    }

    Ok(Plan {
        summary,
        directive: directive.to_string(),
        experiments,
        experiments_dropped_invalid,
        experiments_truncated,
        items,
        excluded_items,
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
        // qwen3-8b is a catalog model but unregistered in the
        // default config, so plan build chains its model.add
        // ahead of the create (tracker #51); the create is the
        // second item and still gets the first free port.
        let r = parse(
            "{\"items\":[{\"kind\":\"backend.create\",\"target\":\"coder-2\",\"change\":{\"model_id\":\"qwen3-8b\"}}]}",
        )
        .unwrap();
        assert_eq!(r.items.len(), 2);
        assert_eq!(r.items[0].kind, PlanKind::ModelAdd);
        assert_eq!(r.items[1].change["port"], json!(8085));
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
    fn experiments_parse_with_flags_and_undecided_default() {
        let r = parse(
            "{\"items\":[],\"experiments\":[{\"candidate_model_id\":\"qwen3-coder-30b\",\"flags\":{\"n_ctx\":32768,\"offload_layers\":40,\"tensor_split\":[0.5,0.5]},\"serves\":\"the directive\",\"hypothesis\":\"faster\",\"predicted\":{\"predicted_per_second\":35.0,\"fit\":\"alongside\"},\"requires_stopping\":[],\"run_eval\":true}]}",
        )
        .unwrap();
        assert_eq!(r.experiments.len(), 1);
        let e = &r.experiments[0];
        assert_eq!(e.id, 1);
        assert_eq!(e.candidate_model_id, "qwen3-coder-30b");
        assert_eq!(e.flags.n_ctx, Some(32768));
        assert_eq!(e.flags.offload_layers, Some(40));
        assert_eq!(e.flags.tensor_split, Some(vec![0.5, 0.5]));
        assert_eq!(e.predicted.predicted_per_second, Some(35.0));
        assert!(e.run_eval);
        assert_eq!(e.decision, Decision::Undecided);
        assert!(!e.fingerprint.is_empty());
        assert_eq!(r.experiments_dropped_invalid, 0);
    }

    #[test]
    fn experiments_with_requires_stopping_start_denied() {
        let r = parse(
            "{\"items\":[],\"experiments\":[{\"candidate_model_id\":\"m1\",\"serves\":\"need\",\"requires_stopping\":[\"coder\"]}]}",
        )
        .unwrap();
        assert_eq!(r.experiments[0].decision, Decision::Denied);
        assert_eq!(r.experiments[0].requires_stopping, vec!["coder"]);
    }

    #[test]
    fn experiments_empty_serves_and_malformed_entries_drop_and_count() {
        let r = parse(
            "{\"items\":[],\"experiments\":[{\"candidate_model_id\":\"m1\",\"serves\":\"\"},{\"candidate_model_id\":\"m2\"},{\"serves\":\"x\"},{\"candidate_model_id\":\"m3\",\"serves\":\"ok\",\"flags\":{\"n_ctx\":\"wide\"}},{\"candidate_model_id\":\"m4\",\"serves\":\"ok\"}]}",
        )
        .unwrap();
        // Empty serves, missing serves, missing candidate, and a
        // malformed flag each drop; only the well-formed entry
        // survives (§A.3 salvage; empty-serves drops, it does
        // not demote).
        assert_eq!(r.experiments.len(), 1);
        assert_eq!(r.experiments[0].candidate_model_id, "m4");
        assert_eq!(r.experiments_dropped_invalid, 4);
        assert_eq!(r.dropped_fragments.len(), 4);
        // Item salvage counting is untouched by experiment drops.
        assert_eq!(r.dropped_invalid, 0);
    }

    #[test]
    fn experiments_cap_at_three() {
        let mut exps = String::new();
        for i in 0..5 {
            if i > 0 {
                exps.push(',');
            }
            exps.push_str(&format!(
                "{{\"candidate_model_id\":\"m{i}\",\"serves\":\"need {i}\"}}"
            ));
        }
        let r = parse(&format!("{{\"items\":[],\"experiments\":[{exps}]}}")).unwrap();
        assert_eq!(r.experiments.len(), 3);
        assert_eq!(r.experiments_truncated, 2);
        assert_eq!(r.experiments[2].id, 3);
    }

    #[test]
    fn experiments_absent_array_and_previously_denied() {
        let plain = parse("{\"items\":[]}").unwrap();
        assert!(plain.experiments.is_empty());
        let first = parse(
            "{\"items\":[],\"experiments\":[{\"candidate_model_id\":\"m1\",\"serves\":\"need\"}]}",
        )
        .unwrap();
        let fp = first.experiments[0].fingerprint.clone();
        let mut denied = HashSet::new();
        denied.insert(fp);
        let second = parse_plan(
            "{\"items\":[],\"experiments\":[{\"candidate_model_id\":\"m1\",\"serves\":\"need\"}]}",
            &AppConfig::default_config(),
            "planner",
            "hash",
            json!({}),
            &denied,
            "",
            Utc::now(),
        )
        .unwrap();
        assert_eq!(second.experiments[0].decision, Decision::Denied);
        assert!(second.experiments[0].previously_denied);
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

    // ---- Tracker #51: grounding backend.create model references.

    /// A config that validates cleanly with exactly one registered
    /// model — the same trim validate.rs's tests use, since
    /// default_config() alone does not pass whole-config
    /// validation (its backends reference unregistered files).
    fn cfg_one_model() -> AppConfig {
        let mut cfg = AppConfig::default_config();
        cfg.backends.retain(|b| b.id == "tool-runner");
        cfg.gateway.groups = vec![manager_config::ModelGroup {
            name: "tool-runner".to_string(),
            members: vec!["tool-runner".to_string()],
            strategy: manager_config::RoutingStrategy::SimpleShuffle,
            fallbacks: Vec::new(),
            purpose: String::new(),
        }];
        cfg.models.push(manager_config::ModelConfig {
            id: "qwen3-8b".to_string(),
            gguf_path: std::path::PathBuf::from("models/qwen3-8b.gguf"),
            quant: "Q4_K_M".to_string(),
            params_b: 8.0,
            sha256: String::new(),
            verified_at: None,
            verified_sha256: None,
            source_url: String::new(),
            assigned_backends: vec!["tool-runner".to_string()],
            enabled: true,
            display_name: None,
            moe_layers: None,
            default_offload_layers: None,
        });
        cfg.backends[0].model_file = std::path::PathBuf::from("models/qwen3-8b.gguf");
        cfg
    }

    fn parse_with(
        response: &str,
        cfg: &AppConfig,
        context: Value,
    ) -> Result<Plan, ParseError> {
        parse_plan(
            response,
            cfg,
            "planner",
            "hash",
            context,
            &HashSet::new(),
            "",
            Utc::now(),
        )
    }

    #[test]
    fn create_on_unregistered_catalog_model_chains_model_add() {
        // (a) A backend proposed on a catalog model that is not
        // registered: the plan chains the catalog's model.add
        // AHEAD of the create, and both validate as actionable.
        let cfg = cfg_one_model();
        let r = parse_with(
            "{\"summary\":\"s\",\"items\":[{\"kind\":\"backend.create\",\"target\":\"planner-alt\",\"change\":{\"model_id\":\"qwen3-27b-planner\",\"port\":8090,\"n_ctx\":32768},\"rationale\":\"r\",\"confidence\":\"medium\"}]}",
            &cfg,
            json!({}),
        )
        .unwrap();
        assert_eq!(r.items.len(), 2);
        assert_eq!(r.items[0].kind, PlanKind::ModelAdd);
        assert_eq!(r.items[0].target.as_deref(), Some("qwen3-27b-planner"));
        assert_eq!(r.items[1].kind, PlanKind::BackendCreate);
        assert_eq!(r.items[1].target.as_deref(), Some("planner-alt"));
        assert!(r.excluded_items.is_empty());
        // The chained add carries the catalog entry's own facts.
        let spec = manager_models::curated_models()
            .into_iter()
            .find(|s| s.id == "qwen3-27b-planner")
            .expect("catalog carries the planner model");
        assert_eq!(
            r.items[0].change["source_url"].as_str(),
            Some(spec.url.as_str())
        );
        assert_eq!(r.items[0].change["quant"].as_str(), Some(spec.quant.as_str()));
        if !spec.sha256.is_empty() {
            assert_eq!(
                r.items[0].change["sha256"].as_str(),
                Some(spec.sha256.as_str())
            );
        }
        // Both items validate actionable against the live
        // snapshot: the add carries the standing model.add
        // warning, the create is valid — in plan order.
        let live = crate::validate::LiveSnapshot {
            layouts: std::collections::HashMap::new(),
            free_vram_mib_per_gpu: vec![16384, 16384],
            backend_running: std::collections::HashMap::new(),
        };
        let verdicts = crate::validate::validate_plan(&r, &cfg, &live);
        assert_eq!(verdicts.len(), 2);
        assert_eq!(
            verdicts[0].status,
            crate::validate::ValidationStatus::Warning,
            "chained model.add verdict: {:?}",
            verdicts[0].reason
        );
        assert_eq!(
            verdicts[1].status,
            crate::validate::ValidationStatus::Valid,
            "chained backend.create verdict: {:?}",
            verdicts[1].reason
        );
    }

    #[test]
    fn authored_model_add_carries_consultation_params_b() {
        // Tracker #38: a consultation-supplied params_b survives
        // parsing on the item's change — the same field apply
        // forwards to model_add — and validation carries it, so
        // an unknown-size, non-catalog add is actionable.
        let cfg = cfg_one_model();
        let r = parse_with(
            "{\"summary\":\"s\",\"items\":[{\"kind\":\"model.add\",\"target\":\"obscure-model\",\"change\":{\"source_url\":\"https://example.com/obscure.gguf\",\"params_b\":13.5},\"rationale\":\"r\",\"confidence\":\"medium\"}]}",
            &cfg,
            json!({}),
        )
        .unwrap();
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].change["params_b"].as_f64(), Some(13.5));
        let live = crate::validate::LiveSnapshot {
            layouts: std::collections::HashMap::new(),
            free_vram_mib_per_gpu: vec![16384, 16384],
            backend_running: std::collections::HashMap::new(),
        };
        let verdicts = crate::validate::validate_plan(&r, &cfg, &live);
        assert_eq!(
            verdicts[0].status,
            crate::validate::ValidationStatus::Warning,
            "verdict: {:?}",
            verdicts[0].reason
        );
    }

    #[test]
    fn authored_model_add_for_a_candidate_gains_its_params_b() {
        // Tracker #38: an authored add naming a discovered
        // candidate's file but carrying no count gains the
        // candidate's real params_b at plan build — the
        // consultation's grounded figure, carried on the item
        // through validation and apply.
        let cfg = cfg_one_model();
        let context = json!({
            "candidates": [{
                "repo_id": "Qwen/Qwen3.8-27B-MoE-GGUF",
                "file": "Qwen3.8-27B-MoE-Q4_K_M.gguf",
                "params_b": 27.0
            }]
        });
        let r = parse_with(
            "{\"summary\":\"s\",\"items\":[{\"kind\":\"model.add\",\"target\":\"qwen3.8-27b-moe\",\"change\":{\"source_url\":\"https://huggingface.co/Qwen/Qwen3.8-27B-MoE-GGUF/resolve/main/Qwen3.8-27B-MoE-Q4_K_M.gguf\"},\"rationale\":\"r\",\"confidence\":\"medium\"}]}",
            &cfg,
            context,
        )
        .unwrap();
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].change["params_b"].as_f64(), Some(27.0));
        let live = crate::validate::LiveSnapshot {
            layouts: std::collections::HashMap::new(),
            free_vram_mib_per_gpu: vec![16384, 16384],
            backend_running: std::collections::HashMap::new(),
        };
        let verdicts = crate::validate::validate_plan(&r, &cfg, &live);
        assert_eq!(
            verdicts[0].status,
            crate::validate::ValidationStatus::Warning,
            "verdict: {:?}",
            verdicts[0].reason
        );
    }

    #[test]
    fn create_on_invented_model_is_excluded_with_reason() {
        // (b) Pete's field case: a backend proposed on a model
        // that is registered nowhere, in no catalog, and was not
        // discovered. The create is excluded at plan build with
        // the reason recorded and stated in the summary; the
        // remaining valid item survives.
        let r = parse(
            "{\"summary\":\"Plan.\",\"items\":[{\"kind\":\"backend.create\",\"target\":\"planner-moe\",\"change\":{\"model_id\":\"qwen3.8-27b-moe\",\"port\":8087,\"n_ctx\":32768,\"offload_layers\":24,\"tensor_split\":[1.0,0.0]},\"rationale\":\"r\",\"confidence\":\"low\"},{\"kind\":\"backend.set_context\",\"target\":\"planner\",\"change\":{\"n_ctx\":131072},\"rationale\":\"r\"}]}",
        )
        .unwrap();
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].kind, PlanKind::BackendSetContext);
        assert_eq!(r.excluded_items.len(), 1);
        assert_eq!(r.excluded_items[0].kind, PlanKind::BackendCreate);
        assert_eq!(r.excluded_items[0].target.as_deref(), Some("planner-moe"));
        assert!(
            r.excluded_items[0].reason.contains("qwen3.8-27b-moe"),
            "reason names the model: {}",
            r.excluded_items[0].reason
        );
        assert!(
            r.summary.contains("planner-moe") && r.summary.contains("qwen3.8-27b-moe"),
            "summary states the exclusion: {}",
            r.summary
        );
        // The exclusion survives plan persistence (the field is
        // part of the plan document, not a parse-time side note).
        let round = serde_json::to_string(&r).unwrap();
        let back: Plan = serde_json::from_str(&round).unwrap();
        assert_eq!(back.excluded_items.len(), 1);
    }

    #[test]
    fn create_on_discovered_candidate_chains_concrete_add() {
        // (c) The consultation grounded the model in the HF
        // discovery pre-pass: the chained add carries the
        // candidate's concrete repo/file and real params_b, and
        // the create is rewritten to the derived registration id.
        let context = json!({
            "candidates": [{
                "repo_id": "someorg/Qwen3.8-27B-MoE-GGUF",
                "file": "Qwen3.8-27B-MoE-Q4_K_M.gguf",
                "params_b": 27.0,
                "quant": "Q4_K_M"
            }]
        });
        let r = parse_with(
            "{\"items\":[{\"kind\":\"backend.create\",\"target\":\"planner-moe\",\"change\":{\"model_id\":\"someorg/Qwen3.8-27B-MoE-GGUF/Qwen3.8-27B-MoE-Q4_K_M.gguf\",\"port\":8087,\"n_ctx\":32768},\"rationale\":\"r\"}]}",
            &AppConfig::default_config(),
            context,
        )
        .unwrap();
        assert_eq!(r.items.len(), 2);
        assert_eq!(r.items[0].kind, PlanKind::ModelAdd);
        assert_eq!(
            r.items[0].target.as_deref(),
            Some("qwen3.8-27b-moe-q4_k_m")
        );
        assert_eq!(
            r.items[0].change["source_url"].as_str(),
            Some("https://huggingface.co/someorg/Qwen3.8-27B-MoE-GGUF/resolve/main/Qwen3.8-27B-MoE-Q4_K_M.gguf")
        );
        assert_eq!(r.items[0].change["params_b"].as_f64(), Some(27.0));
        assert_eq!(r.items[1].kind, PlanKind::BackendCreate);
        assert_eq!(
            r.items[1].change["model_id"].as_str(),
            Some("qwen3.8-27b-moe-q4_k_m")
        );
        assert!(r.excluded_items.is_empty());
    }

    // ---- Dependency ordering (plan-build pass).

    #[test]
    fn dependency_ordering_puts_registration_and_creation_first_deletes_last() {
        // Emitted in the wrong order throughout: routing names
        // a backend that is only created later, the create
        // names a catalog model registered by an add emitted
        // last, and a delete removes the backend the routing
        // items still reference.
        let cfg = cfg_one_model();
        let r = parse_with(
            "{\"items\":[
              {\"kind\":\"group.set_members\",\"target\":\"tool-runner\",\"change\":{\"members\":[\"extra\"]},\"rationale\":\"r\"},
              {\"kind\":\"group.set_fallbacks\",\"target\":\"tool-runner\",\"change\":{\"fallbacks\":[\"extra\",\"openrouter\"]},\"rationale\":\"r\"},
              {\"kind\":\"backend.create\",\"target\":\"extra\",\"change\":{\"model_id\":\"qwen3-27b-planner\",\"port\":8090},\"rationale\":\"r\"},
              {\"kind\":\"backend.delete\",\"target\":\"tool-runner\",\"change\":{},\"rationale\":\"r\"},
              {\"kind\":\"model.add\",\"target\":\"qwen3-27b-planner\",\"change\":{\"source_url\":\"https://example.com/Qwen3-27B-Planner.gguf\"},\"rationale\":\"r\"}
            ]}",
            &cfg,
            json!({}),
        )
        .unwrap();
        let kinds: Vec<(PlanKind, &str)> = r
            .items
            .iter()
            .map(|i| (i.kind, i.target.as_deref().unwrap_or("")))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (PlanKind::ModelAdd, "qwen3-27b-planner"),
                (PlanKind::BackendCreate, "extra"),
                (PlanKind::GroupSetMembers, "tool-runner"),
                (PlanKind::GroupSetFallbacks, "tool-runner"),
                (PlanKind::BackendDelete, "tool-runner"),
            ]
        );
        // Ids follow the new order (the UI aligns validations
        // and decisions by id).
        assert!(r.items.iter().enumerate().all(|(i, item)| item.id == i + 1));
    }

    #[test]
    fn dependency_ordering_preserves_emitted_order_without_edges() {
        // Independent items keep the model's emitted order
        // exactly — the pass moves an item only when an edge
        // forces it.
        let r = parse(
            "{\"items\":[
              {\"kind\":\"backend.set_enabled\",\"target\":\"coder\",\"change\":{\"enabled\":true}},
              {\"kind\":\"backend.set_context\",\"target\":\"planner\",\"change\":{\"n_ctx\":65536}},
              {\"kind\":\"advice\",\"change\":{\"text\":\"note\"}}
            ]}",
        )
        .unwrap();
        let kinds: Vec<PlanKind> = r.items.iter().map(|i| i.kind).collect();
        assert_eq!(
            kinds,
            vec![
                PlanKind::BackendSetEnabled,
                PlanKind::BackendSetContext,
                PlanKind::Advice
            ]
        );
    }

    // ---- The Oct 18 field plan (regression fixture for the
    // plan-aware validation / dependency ordering / experiment
    // provisioning fixes).

    /// The field case's starting configuration: backends
    /// `planner` and `planner-mtp` on registered models, group
    /// `planner` routed to `planner`. Valid as a whole config.
    fn cfg_planner_pair() -> AppConfig {
        let mut cfg = AppConfig::default_config();
        let backend = |id: &str, model_file: &str, port: u16| manager_config::BackendConfig {
            id: id.to_string(),
            enabled: true,
            model_file: std::path::PathBuf::from(model_file),
            port,
            flags: manager_config::ServerFlags {
                n_ctx: 32768,
                n_batch: 512,
                tensor_split: Vec::new(),
                split_mode: manager_config::SplitMode::None,
            },
            raw_flags: None,
            flags_advanced: false,
            restart_policy: manager_config::RestartPolicy {
                max_retries: 5,
                backoff_base_secs: 2,
            },
            overrides_global: false,
            purpose: String::new(),
            tool_call_probe: None,
        };
        let model = |id: &str, file: &str| manager_config::ModelConfig {
            id: id.to_string(),
            gguf_path: std::path::PathBuf::from(file),
            quant: "Q4_K_M".to_string(),
            params_b: 27.0,
            sha256: String::new(),
            verified_at: None,
            verified_sha256: None,
            source_url: String::new(),
            assigned_backends: Vec::new(),
            enabled: true,
            display_name: None,
            moe_layers: None,
            default_offload_layers: None,
        };
        cfg.backends = vec![
            backend("planner", "models/planner.gguf", 8081),
            backend("planner-mtp", "models/planner-mtp.gguf", 8082),
        ];
        cfg.models = vec![
            model("planner-model", "models/planner.gguf"),
            model("planner-mtp-model", "models/planner-mtp.gguf"),
        ];
        cfg.gateway.groups = vec![
            manager_config::ModelGroup {
                name: "planner".to_string(),
                members: vec!["planner".to_string()],
                strategy: manager_config::RoutingStrategy::SimpleShuffle,
                fallbacks: Vec::new(),
                purpose: String::new(),
            },
            manager_config::ModelGroup {
                name: "planner-mtp".to_string(),
                members: vec!["planner-mtp".to_string()],
                strategy: manager_config::RoutingStrategy::SimpleShuffle,
                fallbacks: Vec::new(),
                purpose: String::new(),
            },
        ];
        assert!(
            manager_config::validate(&cfg).is_ok(),
            "fixture config must validate: {:?}",
            manager_config::validate(&cfg)
        );
        cfg
    }

    /// The reported plan, in the order the coder backend
    /// emitted it: create before its model registration,
    /// routing before the creates it names, deletes that the
    /// routing still references, and an experiment whose
    /// candidate nothing registered.
    const FIELD_PLAN_RESPONSE: &str = r#"{"summary":"Replace the planner with a MoE model.","items":[
      {"kind":"backend.create","target":"planner-moe","change":{"model_id":"qwen3.8-27b-moe","port":8087,"n_ctx":32768,"offload_layers":24},"rationale":"MoE planner","confidence":"medium"},
      {"kind":"group.set_members","target":"planner","change":{"members":["planner-moe"]},"rationale":"route to the new backend"},
      {"kind":"group.set_fallbacks","target":"planner","change":{"fallbacks":["planner","openrouter"]},"rationale":"fall back to the old planner, then cloud"},
      {"kind":"backend.delete","target":"planner","change":{},"rationale":"replaced"},
      {"kind":"backend.delete","target":"planner-mtp","change":{},"rationale":"replaced"},
      {"kind":"model.add","target":"qwen3.8-27b-moe","change":{"source_url":"https://huggingface.co/Qwen/Qwen3.8-27B-MoE-GGUF/resolve/main/Qwen3.8-27B-MoE-Q4_K_M.gguf"},"rationale":"register the MoE model"}
    ],"experiments":[
      {"candidate_model_id":"qwen3.8-27b-moe","flags":{"n_ctx":32768,"offload_layers":24},"serves":"the directive: find a more effective MoE planning model","hypothesis":"the MoE candidate plans faster","requires_stopping":["planner"],"run_eval":false}
    ]}"#;

    fn field_plan() -> (AppConfig, Plan) {
        let cfg = cfg_planner_pair();
        let context = json!({
            "candidates": [{
                "repo_id": "Qwen/Qwen3.8-27B-MoE-GGUF",
                "file": "Qwen3.8-27B-MoE-Q4_K_M.gguf",
                "params_b": 27.0
            }]
        });
        let plan = parse_with(FIELD_PLAN_RESPONSE, &cfg, context).unwrap();
        (cfg, plan)
    }

    #[test]
    fn field_plan_orders_validates_and_provisions() {
        let (cfg, plan) = field_plan();

        // (a) Final order 6,1,2,3,4,5 by kind/identity, with
        // the experiment's chained download after them.
        let kinds: Vec<(PlanKind, &str)> = plan
            .items
            .iter()
            .map(|i| (i.kind, i.target.as_deref().unwrap_or("")))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (PlanKind::ModelAdd, "qwen3.8-27b-moe"),
                (PlanKind::BackendCreate, "planner-moe"),
                (PlanKind::GroupSetMembers, "planner"),
                (PlanKind::GroupSetFallbacks, "planner"),
                (PlanKind::BackendDelete, "planner"),
                (PlanKind::BackendDelete, "planner-mtp"),
                (PlanKind::ModelDownload, "qwen3.8-27b-moe"),
            ]
        );
        assert!(plan.items.iter().enumerate().all(|(i, item)| item.id == i + 1));

        // (b) No Invalid flags on items 1, 2, 4 (the create,
        // the members change, the planner delete) once the
        // plan is simulated in its final order.
        let mut layouts = std::collections::HashMap::new();
        layouts.insert(
            "qwen3.8-27b-moe".to_string(),
            crate::fit::LayoutFacts {
                total_bytes: 15 * 1024 * manager_models::MIB,
                expert_bytes: Some(10 * 1024 * manager_models::MIB),
                expert_bytes_by_block: None,
                moe_layers: Some(48),
                kv_per_token: Some(8192),
                kv_geometry: None,
                rs_bytes_per_seq: None,
                basis: manager_models::MemoryBasis::Estimate,
                ctx_max: 131072,
                params_b: 27.0,
            },
        );
        let mut backend_running = std::collections::HashMap::new();
        backend_running.insert("planner-mtp".to_string(), true);
        let live = crate::validate::LiveSnapshot {
            layouts,
            free_vram_mib_per_gpu: vec![16384, 16384],
            backend_running,
        };
        let verdicts = crate::validate::validate_plan(&plan, &cfg, &live);
        use crate::validate::ValidationStatus;
        for (idx, label) in [(1, "backend.create"), (2, "group.set_members"), (4, "backend.delete planner")] {
            assert_ne!(
                verdicts[idx].status,
                ValidationStatus::Invalid,
                "{label} must not be Invalid: {:?}",
                verdicts[idx].reason
            );
        }
        // The planner delete carries the fallback-scrub
        // warning, not an invalid flag.
        assert_eq!(verdicts[4].status, ValidationStatus::Warning);
        assert!(
            verdicts[4]
                .reason
                .as_deref()
                .unwrap_or("")
                .contains("fallback chain of: planner"),
            "delete warning: {:?}",
            verdicts[4].reason
        );
        assert_ne!(verdicts[6].status, ValidationStatus::Invalid);

        // (c) E1 gains provisioning steps and is runnable once
        // they are accepted.
        let exp = &plan.experiments[0];
        assert_eq!(exp.decision, Decision::Denied, "requires_stopping starts denied");
        let prov = exp
            .provisioning
            .as_ref()
            .expect("the experiment must carry provisioning");
        assert_eq!(prov.model_id, "qwen3.8-27b-moe");
        assert_eq!(prov.item_ids, vec![1, 7], "the authored add and the chained download");
        let download = &plan.items[6];
        assert_eq!(download.provision_for_experiment, Some(1));
        assert_eq!(download.decision, Decision::Denied, "provisioning starts with the experiment's decision");
        // Plan-aware validation: not a dead-end Invalid.
        let extra = vec![
            "Qwen/Qwen3.8-27B-MoE-GGUF/Qwen3.8-27B-MoE-Q4_K_M.gguf".to_string(),
        ];
        let ev = crate::experiment::validate_experiment_for_plan(exp, &cfg, &extra, &plan.items);
        assert_eq!(ev.status, ValidationStatus::Valid, "E1 verdict: {:?}", ev.reason);

        // Accepting the experiment accepts its provisioning
        // items; after they apply (model registered, file on
        // disk) preflight resolution succeeds — the experiment
        // is runnable.
        let mut accepted_plan = plan.clone();
        let newly_denied =
            crate::experiment::apply_experiment_decision(&mut accepted_plan, 1, Decision::Accepted)
                .expect("experiment exists");
        assert!(newly_denied.is_empty());
        assert_eq!(accepted_plan.experiments[0].decision, Decision::Accepted);
        assert_eq!(accepted_plan.items[0].decision, Decision::Accepted, "linked authored add accepted");
        assert_eq!(accepted_plan.items[6].decision, Decision::Accepted, "chained download accepted");
        let mut cfg_after = cfg.clone();
        cfg_after.models.push(manager_config::ModelConfig {
            id: "qwen3.8-27b-moe".to_string(),
            gguf_path: std::path::PathBuf::from("models/Qwen3.8-27B-MoE-Q4_K_M.gguf"),
            quant: "Q4_K_M".to_string(),
            params_b: 27.0,
            sha256: String::new(),
            verified_at: None,
            verified_sha256: None,
            source_url: String::new(),
            assigned_backends: Vec::new(),
            enabled: true,
            display_name: None,
            moe_layers: None,
            default_offload_layers: None,
        });
        let registered = vec![crate::experiment::RegisteredModelFile {
            id: "qwen3.8-27b-moe".to_string(),
            path: std::path::PathBuf::from("/data/models/Qwen3.8-27B-MoE-Q4_K_M.gguf"),
        }];
        let resolution = crate::experiment::resolve_candidate(
            "qwen3.8-27b-moe",
            &registered,
            &[],
            std::path::Path::new("/data/models"),
            |_| true,
        );
        assert!(
            matches!(resolution, crate::experiment::CandidateResolution::OnDisk { .. }),
            "post-provisioning resolution: {resolution:?}"
        );

        // Denying the experiment denies only the item chained
        // for it — the authored registration (which the
        // backend.create also relies on) is untouched.
        let mut denied_plan = plan.clone();
        crate::experiment::apply_experiment_decision(&mut denied_plan, 1, Decision::Denied)
            .expect("experiment exists");
        assert_eq!(denied_plan.items[6].decision, Decision::Denied, "chained download denied");
        assert_eq!(
            denied_plan.items[0].decision,
            Decision::Undecided,
            "authored add is shared, never cascade-denied"
        );
    }

    #[test]
    fn reground_repairs_a_prefix_stored_plan() {
        use crate::validate::ValidationStatus;
        // Pete's Oct 18 plan as a pre-provisioning build stored
        // it: no provisioning record on the experiment and no
        // chained download item. Loading it under current code
        // must restore exactly what plan building produces —
        // the authored add linked, the download chained — so
        // validation and the runner stop dead-ending it.
        let (cfg, built) = field_plan();
        let mut plan = built.clone();
        plan.items.retain(|i| i.kind != PlanKind::ModelDownload);
        plan.experiments[0].provisioning = None;
        // Before repair: the field failure — plan-aware
        // validation calls the candidate unresolvable even
        // though the plan carries its registration.
        let ev_before = crate::experiment::validate_experiment_for_plan(
            &plan.experiments[0],
            &cfg,
            &[],
            &plan.items,
        );
        assert_eq!(ev_before.status, ValidationStatus::Invalid);

        assert!(reground_experiment_provisioning(&mut plan, &cfg));
        let exp = &plan.experiments[0];
        let prov = exp
            .provisioning
            .as_ref()
            .expect("reground must restore provisioning");
        assert_eq!(prov.model_id, "qwen3.8-27b-moe");
        assert_eq!(
            prov.item_ids,
            vec![1, 7],
            "the authored add and the chained download"
        );
        let download = plan
            .items
            .iter()
            .find(|i| i.id == 7)
            .expect("download appended");
        assert_eq!(download.kind, PlanKind::ModelDownload);
        assert_eq!(download.provision_for_experiment, Some(1));
        assert_eq!(
            download.decision,
            Decision::Denied,
            "the chained download follows the experiment's decision"
        );
        // After repair the experiment validates exactly as the
        // freshly built plan's does.
        let ev = crate::experiment::validate_experiment_for_plan(exp, &cfg, &[], &plan.items);
        assert_eq!(
            ev.status,
            ValidationStatus::Valid,
            "E1 verdict: {:?}",
            ev.reason
        );
        // Idempotent: a second pass changes nothing.
        assert!(!reground_experiment_provisioning(&mut plan, &cfg));
    }

    #[test]
    fn reground_leaves_an_unresolvable_candidate_unprovisioned() {
        // No model.add in the plan, no catalog entry, no HF
        // form: load-time re-grounding has no source either,
        // and the precise missing-source verdict stands.
        let cfg = cfg_planner_pair();
        let mut plan = parse_with(
            "{\"items\":[],\"experiments\":[{\"candidate_model_id\":\"totally-invented-model\",\"serves\":\"the directive\"}]}",
            &cfg,
            json!({}),
        )
        .unwrap();
        assert!(plan.experiments[0].provisioning.is_none());
        assert!(!reground_experiment_provisioning(&mut plan, &cfg));
        assert!(plan.experiments[0].provisioning.is_none());
    }

    #[test]
    fn unresolvable_experiment_candidate_states_what_is_missing() {
        // A candidate no source can resolve: no provisioning
        // is chained, the summary says so, and plan-aware
        // validation names every missing source instead of
        // the old generic dead end.
        let cfg = cfg_planner_pair();
        let r = parse_with(
            "{\"items\":[],\"experiments\":[{\"candidate_model_id\":\"totally-invented-model\",\"serves\":\"the directive\"}]}",
            &cfg,
            json!({}),
        )
        .unwrap();
        let exp = &r.experiments[0];
        assert!(exp.provisioning.is_none());
        assert!(
            r.summary.contains("cannot be provisioned"),
            "summary states it: {}",
            r.summary
        );
        let ev = crate::experiment::validate_experiment_for_plan(exp, &cfg, &[], &r.items);
        assert_eq!(ev.status, crate::validate::ValidationStatus::Invalid);
        let reason = ev.reason.unwrap();
        assert!(reason.contains("not a registered model"), "{reason}");
        assert!(reason.contains("not in the curated catalog"), "{reason}");
        assert!(reason.contains("not among the candidates discovered"), "{reason}");
        assert!(reason.contains("not an HF repo/file"), "{reason}");
    }

    #[test]
    fn catalog_experiment_candidate_chains_add_and_download() {
        // An unregistered catalog candidate: provisioning
        // chains the catalog registration plus the download,
        // both marked as the experiment's own steps.
        let cfg = cfg_planner_pair();
        let r = parse_with(
            "{\"items\":[],\"experiments\":[{\"candidate_model_id\":\"qwen3-coder-30b\",\"serves\":\"the directive\"}]}",
            &cfg,
            json!({}),
        )
        .unwrap();
        let exp = &r.experiments[0];
        let prov = exp.provisioning.as_ref().expect("provisioning chained");
        assert_eq!(prov.model_id, "qwen3-coder-30b");
        assert_eq!(prov.item_ids.len(), 2);
        let add = &r.items[prov.item_ids[0] - 1];
        let download = &r.items[prov.item_ids[1] - 1];
        assert_eq!(add.kind, PlanKind::ModelAdd);
        assert_eq!(add.provision_for_experiment, Some(1));
        assert_eq!(download.kind, PlanKind::ModelDownload);
        assert_eq!(download.provision_for_experiment, Some(1));
        // The registration precedes the download.
        assert!(prov.item_ids[0] < prov.item_ids[1]);
    }

    #[test]
    fn authored_model_add_after_its_create_is_relocated() {
        // (d) The consultant authored both items but in the wrong
        // order: the add moves ahead of the create, unchanged,
        // and no second add is chained.
        let r = parse(
            "{\"items\":[{\"kind\":\"backend.create\",\"target\":\"planner-alt\",\"change\":{\"model_id\":\"qwen3-27b-planner\",\"port\":8090},\"rationale\":\"r\"},{\"kind\":\"model.add\",\"target\":\"qwen3-27b-planner\",\"change\":{\"source_url\":\"https://example.com/Qwen3-27B-Planner.gguf\",\"quant\":\"Q4_K_M\"},\"rationale\":\"register it\"}]}",
        )
        .unwrap();
        assert_eq!(r.items.len(), 2);
        assert_eq!(r.items[0].kind, PlanKind::ModelAdd);
        assert_eq!(
            r.items[0].change["source_url"].as_str(),
            Some("https://example.com/Qwen3-27B-Planner.gguf"),
            "the authored add is the one relocated, not a chained copy"
        );
        assert_eq!(r.items[1].kind, PlanKind::BackendCreate);
        assert!(r.excluded_items.is_empty());
    }
}
