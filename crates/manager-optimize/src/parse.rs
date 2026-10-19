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
    experiment_fingerprint, ExperimentFlags, ExperimentPredicted, ExperimentProposal,
    MAX_EXPERIMENTS,
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
    let candidates: Vec<CandidateGrounding> = context
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
        .unwrap_or_default();

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
            result: None,
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

    // Ground every backend.create's model reference (tracker
    // #51): chain the model.add that registers an unregistered
    // catalog or discovered model ahead of its create, and
    // exclude a create whose model resolves to neither —
    // recorded on the plan and stated in the summary.
    let mut summary = summary;
    let (items, excluded_items) =
        ground_create_models(items, cfg, &context, denied_fingerprints, &mut summary);
    let mut items = items;

    let truncated = items.len().saturating_sub(MAX_ITEMS);
    items.truncate(MAX_ITEMS);
    for (i, item) in items.iter_mut().enumerate() {
        item.id = i + 1;
    }

    // Experiments (§A.3): the sibling array, parsed after the
    // items so its dropped fragments append to the same list.
    let (experiments, experiments_dropped_invalid, experiments_truncated) =
        parse_experiments(&root, denied_fingerprints, &mut dropped_fragments);

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
