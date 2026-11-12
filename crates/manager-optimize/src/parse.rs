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
use serde_json::{json, Value};

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

mod extract;
mod grounding;
mod normalize;
mod ordering;
mod provision;
#[cfg(test)]
mod test_fixtures;

pub use extract::{extract_json_object, json_object_candidates};
pub use provision::reground_experiment_provisioning;

use grounding::{assign_create_port, ground_backend_model_refs, ground_model_add_params};
use normalize::{
    as_u32, demote_text, known_unmapped, normalize_change, req_f32_vec, req_str, ChangeVerdict,
};
use ordering::order_items_by_dependency;
use provision::ground_experiment_provisioning;

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
            if fo.contains_key("moe_cache_mib") {
                match fo.get("moe_cache_mib").and_then(as_u32) {
                    Some(n) => flags.moe_cache_mib = Some(n),
                    None => flags_ok = false,
                }
            }
            if fo.contains_key("tensor_split") {
                match req_f32_vec(fo, "tensor_split") {
                    Some(v) => {
                        flags.tensor_split = Some(
                            v.iter()
                                .filter_map(|x| x.as_f64().map(|f| f as f32))
                                .collect(),
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
    // Prefer the first object that carries an `items` array (the plan),
    // so an unrelated JSON object quoted in the prose ahead of it does
    // not win; fall back to the first object (which then fails as
    // `NoItems`, as before).
    let mut candidates = json_object_candidates(response)
        .map(|t| serde_json::from_str::<Value>(t).expect("candidates are parseable JSON objects"));
    let first = candidates.next().ok_or(ParseError::NoJsonObject)?;
    let root: Value = if first.get("items").is_some_and(Value::is_array) {
        first
    } else {
        candidates
            .find(|v| v.get("items").is_some_and(Value::is_array))
            .unwrap_or(first)
    };
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

    // Ground every backend.create's and backend.set_model's
    // model reference (trackers #51 and #54): chain the
    // model.add that registers an unregistered catalog or
    // discovered model ahead of its dependent item, and
    // exclude an item whose model resolves to neither —
    // recorded on the plan and stated in the summary.
    let mut summary = summary;
    let (items, excluded_items) =
        ground_backend_model_refs(items, cfg, &context, denied_fingerprints, &mut summary);

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
    use test_fixtures::*;

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

    /// P-8: prose braces or an unrelated object before the plan no
    /// longer fail the consultation.
    #[test]
    fn plan_survives_braces_and_objects_in_prose() {
        let plan = "{\"summary\":\"s\",\"items\":[{\"kind\":\"backend.set_context\",\"target\":\"coder\",\"change\":{\"n_ctx\":65536},\"rationale\":\"r\",\"confidence\":\"high\"}]}";
        for prefix in [
            "Use the {ctx} setting.\n```json\n",
            "a { b unbalanced ...\n",
            "Current: {\"n_ctx\": 32768}. Plan:\n",
            "Größe {ü} — plan:\n",
        ] {
            let r = parse(&format!("{prefix}{plan}\n```"))
                .unwrap_or_else(|e| panic!("{prefix:?}: {e:?}"));
            assert_eq!(r.items.len(), 1, "{prefix:?}");
        }
        assert_eq!(
            extract_json_object("x {y} {\"a\":1}").as_deref(),
            Some("{\"a\":1}")
        );
    }

    /// P-7: an out-of-range port drops the create instead of being
    /// truncated to a different port.
    #[test]
    fn out_of_range_port_drops_the_create() {
        // Control: the same create with a valid port survives.
        let ok = parse(
            "{\"items\":[{\"kind\":\"backend.create\",\"target\":\"x-new\",\"change\":{\"model_id\":\"qwen3-8b\",\"port\":8090},\"rationale\":\"r\"}]}",
        )
        .unwrap();
        assert!(
            ok.items.iter().any(|i| i.kind == PlanKind::BackendCreate),
            "control create must survive: {:?}",
            ok.dropped_fragments
        );
        for port in [0u64, 65536, 73617] {
            let r = parse(&format!(
                "{{\"items\":[{{\"kind\":\"backend.create\",\"target\":\"x-new\",\"change\":{{\"model_id\":\"qwen3-8b\",\"port\":{port}}},\"rationale\":\"r\"}}]}}"
            ))
            .unwrap();
            assert!(
                r.items.iter().all(|i| i.kind != PlanKind::BackendCreate),
                "port {port} must not survive as a create"
            );
            assert_eq!(r.dropped_invalid, 1, "port {port}");
        }
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
}
