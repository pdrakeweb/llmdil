//! Model-reference grounding (trackers #51/#54): backend.create /
//! backend.set_model references resolved against registered, catalog,
//! and discovered models, chaining `model.add` where needed.

use std::collections::HashSet;

use manager_config::AppConfig;
use serde_json::{json, Value};

use crate::plan::{fingerprint, Confidence, Decision, PlanItem, PlanKind};

/// Assign a port to a backend.create item that did not name one:
/// the lowest free port from 8081 up, skipping the gateway and
/// Postgres ports and every configured or already-assigned port.
/// The assigned port becomes part of the item's change, so the
/// plan row can state it and apply uses exactly it.
pub(super) fn assign_create_port(cfg: &AppConfig, assigned: &HashSet<u16>) -> u16 {
    let mut used: HashSet<u16> = cfg.backends.iter().map(|b| b.port).collect();
    used.insert(cfg.gateway.port);
    used.insert(cfg.postgres.port);
    used.extend(assigned.iter().copied());
    (8081..=8999).find(|p| !used.contains(p)).unwrap_or(8999)
}

/// One discovered candidate from the consultation's context
/// document (the `candidates` block `build_context` assembled
/// from the discovery pre-pass): a grounding source for
/// new-model proposals that are neither registered nor catalog
/// models (tracker #51).
pub(super) struct CandidateGrounding {
    pub(super) repo_id: String,
    pub(super) file: String,
    pub(super) params_b: f64,
}

/// The context document's discovered candidates, in the shape
/// the grounding passes consume.
pub(super) fn candidates_from_context(context: &Value) -> Vec<CandidateGrounding> {
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
pub(super) fn candidate_model_id(file: &str) -> String {
    // Case-insensitive, like the `.gguf` acceptance check in
    // experiment.rs (P-11c): `X.GGUF` derives the same id as `X.gguf`.
    let stem = file
        .len()
        .checked_sub(".gguf".len())
        .and_then(|cut| {
            file.get(cut..)
                .filter(|ext| ext.eq_ignore_ascii_case(".gguf"))
                .map(|_| &file[..cut])
        })
        .unwrap_or(file);
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
/// the denial-memory decision rule, the dependent item's own
/// confidence (the add exists to serve its proposal).
pub(super) fn chained_model_add(
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

/// Ground every `backend.create`'s and every
/// `backend.set_model`'s model reference (trackers #51 and
/// #54). Plan construction is the reliable layer for this
/// rule — the prompt states it, but a local model can still
/// invent a plausible model id (the field case: a create on
/// `qwen3.8-27b-moe`, offered with an Accept button, invalid at
/// validation, silently skipped at apply). For each create
/// and each set_model, in plan order:
///
/// - model registered → the item stands;
/// - a `model.add` for the model already emitted (authored
///   earlier, or chained for an earlier item) → stands;
/// - an authored `model.add` for the model sits LATER in the
///   plan → it is moved ahead of the item (validation and
///   apply both run in plan order, so order is the defect);
/// - model in the curated catalog → a `model.add` carrying the
///   catalog entry's URL, quant, and hash is chained ahead;
/// - model naming a discovered candidate (its repo id, its
///   `repo/file`, or the id derived from its file) → a
///   `model.add` carrying the candidate's concrete HF resolve
///   URL and real `params_b` is chained ahead, and the item's
///   `model_id` is rewritten to the derived registration id;
/// - anything else → the item is EXCLUDED: recorded in the
///   plan's `excluded_items` with its reason, and stated in the
///   plan summary. Never silently dropped, never offered.
///
/// New-model proposals therefore only ever come from the
/// catalog or the discovery results — never free invention.
/// That invariant covers both kinds identically: a
/// `backend.create` and a `backend.set_model` naming the same
/// unregistered model share one chained add, and neither kind
/// can introduce a model from anywhere else.
pub(super) fn ground_backend_model_refs(
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
        if !matches!(
            item.kind,
            PlanKind::BackendCreate | PlanKind::BackendSetModel
        ) {
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
        // move it ahead of the item.
        if let Some(pos) = pending.iter().position(|p| {
            p.kind == PlanKind::ModelAdd && p.target.as_deref() == Some(model_id.as_str())
        }) {
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
                    "Registers the catalog model '{model_id}' ({}), chained by the app at plan build: the {} for '{target}' names it, and it is in the curated catalog but not yet registered.",
                    spec.name,
                    item.kind.as_str()
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
                // the plan serves the item: move it ahead.
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
                            "Registers the discovered model '{}/{}' (Hugging Face discovery pre-pass for this consultation), chained by the app at plan build: the {} for '{target}' is grounded in it.",
                            c.repo_id,
                            c.file,
                            item.kind.as_str()
                        ),
                        item.confidence,
                        denied_fingerprints,
                    );
                    adds_available.insert(derived.clone());
                    out.push(add);
                }
            }
            // The item must name the id the add registers (or
            // the already-registered model with that id): rewrite
            // and re-fingerprint, as port assignment rewrites a
            // create's port at parse.
            if item.change.get("model_id").and_then(|v| v.as_str()) != Some(derived.as_str()) {
                item.change["model_id"] = json!(derived);
                item.fingerprint = fingerprint(item.kind, item.target.as_deref(), &item.change);
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
        let consequence = if item.kind == PlanKind::BackendSetModel {
            "so a backend can never be switched to it"
        } else {
            "so a backend on it can never be created"
        };
        let reason = format!(
            "model '{model_id}' is not registered, is not in the catalog, and was not among the models discovered for this consultation, {consequence}"
        );
        if !summary.is_empty() {
            summary.push(' ');
        }
        summary.push_str(&format!(
            "Excluded at plan build: {} '{target}' names {reason}.",
            item.kind.as_str()
        ));
        excluded.push(crate::plan::ExcludedItem {
            kind: item.kind,
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
pub(super) fn ground_model_add_params(
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
                    && (c.file == file_name || url.contains(&format!("{}/{}", c.repo_id, c.file)))
            });
            if let Some(c) = candidate {
                item.change["params_b"] = json!(c.params_b);
                item.fingerprint =
                    fingerprint(PlanKind::ModelAdd, item.target.as_deref(), &item.change);
                item.previously_denied = denied_fingerprints.contains(&item.fingerprint);
                if item.previously_denied {
                    item.decision = Decision::Denied;
                }
            }
            item
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::test_fixtures::*;
    use crate::parse::*;

    /// P-11c (docs/code-review-v0.1.60.md): `.gguf` is accepted case-
    /// insensitively, so the id derivation strips it the same way.
    #[test]
    fn candidate_model_id_strips_extension_case_insensitively() {
        assert_eq!(
            candidate_model_id("Qwen3-8B-Q4_K_M.gguf"),
            "qwen3-8b-q4_k_m"
        );
        assert_eq!(
            candidate_model_id("Qwen3-8B-Q4_K_M.GGUF"),
            "qwen3-8b-q4_k_m"
        );
        assert_eq!(
            candidate_model_id("Qwen3-8B-Q4_K_M.Gguf"),
            "qwen3-8b-q4_k_m"
        );
        // No extension, or a name shorter than one: unchanged rule.
        assert_eq!(candidate_model_id("model"), "model");
        assert_eq!(candidate_model_id("gguf"), "gguf");
        // A multi-byte character before the cut does not panic.
        assert_eq!(candidate_model_id("é.gguf"), "-");
    }

    // ---- Tracker #51: grounding backend.create model references.

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
        assert_eq!(
            r.items[0].change["quant"].as_str(),
            Some(spec.quant.as_str())
        );
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
        assert_eq!(r.items[0].target.as_deref(), Some("qwen3.8-27b-moe-q4_k_m"));
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

    // ---- Tracker #54: grounding backend.set_model model
    // references, mirroring every #51 create case above.

    #[test]
    fn set_model_on_registered_model_stands() {
        let cfg = cfg_one_model();
        let r = parse_with(
            "{\"items\":[{\"kind\":\"backend.set_model\",\"target\":\"tool-runner\",\"change\":{\"model_id\":\"qwen3-8b\"},\"rationale\":\"r\"}]}",
            &cfg,
            json!({}),
        )
        .unwrap();
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].kind, PlanKind::BackendSetModel);
        assert!(r.excluded_items.is_empty());
    }

    #[test]
    fn set_model_with_earlier_authored_add_stands() {
        // A model.add already emitted earlier in the plan
        // registers the model: the set_model stands and no
        // second add is chained.
        let cfg = cfg_one_model();
        let r = parse_with(
            "{\"items\":[{\"kind\":\"model.add\",\"target\":\"obscure-model\",\"change\":{\"source_url\":\"https://example.com/obscure.gguf\",\"params_b\":13.5},\"rationale\":\"r\"},{\"kind\":\"backend.set_model\",\"target\":\"tool-runner\",\"change\":{\"model_id\":\"obscure-model\"},\"rationale\":\"r\"}]}",
            &cfg,
            json!({}),
        )
        .unwrap();
        assert_eq!(r.items.len(), 2);
        assert_eq!(r.items[0].kind, PlanKind::ModelAdd);
        assert_eq!(r.items[1].kind, PlanKind::BackendSetModel);
        assert!(r.excluded_items.is_empty());
    }

    #[test]
    fn set_model_authored_add_later_is_relocated_ahead() {
        let cfg = cfg_one_model();
        let r = parse_with(
            "{\"items\":[{\"kind\":\"backend.set_model\",\"target\":\"tool-runner\",\"change\":{\"model_id\":\"obscure-model\"},\"rationale\":\"r\"},{\"kind\":\"model.add\",\"target\":\"obscure-model\",\"change\":{\"source_url\":\"https://example.com/obscure.gguf\",\"params_b\":13.5},\"rationale\":\"r\"}]}",
            &cfg,
            json!({}),
        )
        .unwrap();
        assert_eq!(r.items.len(), 2);
        assert_eq!(r.items[0].kind, PlanKind::ModelAdd);
        assert_eq!(r.items[0].target.as_deref(), Some("obscure-model"));
        assert_eq!(r.items[1].kind, PlanKind::BackendSetModel);
        assert!(r.excluded_items.is_empty());
    }

    #[test]
    fn set_model_on_unregistered_catalog_model_chains_model_add() {
        let cfg = cfg_one_model();
        let r = parse_with(
            "{\"summary\":\"s\",\"items\":[{\"kind\":\"backend.set_model\",\"target\":\"tool-runner\",\"change\":{\"model_id\":\"qwen3-27b-planner\"},\"rationale\":\"r\",\"confidence\":\"medium\"}]}",
            &cfg,
            json!({}),
        )
        .unwrap();
        assert_eq!(r.items.len(), 2);
        assert_eq!(r.items[0].kind, PlanKind::ModelAdd);
        assert_eq!(r.items[0].target.as_deref(), Some("qwen3-27b-planner"));
        assert_eq!(r.items[1].kind, PlanKind::BackendSetModel);
        assert_eq!(r.items[1].target.as_deref(), Some("tool-runner"));
        assert!(r.excluded_items.is_empty());
        let spec = manager_models::curated_models()
            .into_iter()
            .find(|s| s.id == "qwen3-27b-planner")
            .expect("catalog carries the planner model");
        assert_eq!(
            r.items[0].change["source_url"].as_str(),
            Some(spec.url.as_str())
        );
        assert_eq!(
            r.items[0].change["quant"].as_str(),
            Some(spec.quant.as_str())
        );
        if !spec.sha256.is_empty() {
            assert_eq!(
                r.items[0].change["sha256"].as_str(),
                Some(spec.sha256.as_str())
            );
        }
        // Both items validate actionable in plan order: the add
        // carries the standing model.add warning, the set_model
        // is valid against the simulated registration.
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
            "chained backend.set_model verdict: {:?}",
            verdicts[1].reason
        );
    }

    #[test]
    fn set_model_on_discovered_candidate_chains_concrete_add_and_rewrites() {
        let context = json!({
            "candidates": [{
                "repo_id": "someorg/Qwen3.8-27B-MoE-GGUF",
                "file": "Qwen3.8-27B-MoE-Q4_K_M.gguf",
                "params_b": 27.0,
                "quant": "Q4_K_M"
            }]
        });
        let cfg = cfg_one_model();
        let r = parse_with(
            "{\"items\":[{\"kind\":\"backend.set_model\",\"target\":\"tool-runner\",\"change\":{\"model_id\":\"someorg/Qwen3.8-27B-MoE-GGUF/Qwen3.8-27B-MoE-Q4_K_M.gguf\"},\"rationale\":\"r\"}]}",
            &cfg,
            context,
        )
        .unwrap();
        assert_eq!(r.items.len(), 2);
        assert_eq!(r.items[0].kind, PlanKind::ModelAdd);
        assert_eq!(r.items[0].target.as_deref(), Some("qwen3.8-27b-moe-q4_k_m"));
        assert_eq!(
            r.items[0].change["source_url"].as_str(),
            Some("https://huggingface.co/someorg/Qwen3.8-27B-MoE-GGUF/resolve/main/Qwen3.8-27B-MoE-Q4_K_M.gguf")
        );
        assert_eq!(r.items[0].change["params_b"].as_f64(), Some(27.0));
        assert_eq!(r.items[1].kind, PlanKind::BackendSetModel);
        // The set_model's model_id is rewritten to the derived
        // registration id the chained add registers.
        assert_eq!(
            r.items[1].change["model_id"].as_str(),
            Some("qwen3.8-27b-moe-q4_k_m")
        );
        assert_eq!(
            r.items[1].fingerprint,
            fingerprint(
                PlanKind::BackendSetModel,
                Some("tool-runner"),
                &r.items[1].change
            )
        );
        assert!(r.excluded_items.is_empty());
    }

    #[test]
    fn set_model_on_invented_model_is_excluded_with_reason() {
        let r = parse(
            "{\"summary\":\"Plan.\",\"items\":[{\"kind\":\"backend.set_model\",\"target\":\"planner\",\"change\":{\"model_id\":\"qwen3.8-27b-moe\"},\"rationale\":\"r\",\"confidence\":\"low\"},{\"kind\":\"backend.set_context\",\"target\":\"planner\",\"change\":{\"n_ctx\":131072},\"rationale\":\"r\"}]}",
        )
        .unwrap();
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].kind, PlanKind::BackendSetContext);
        assert_eq!(r.excluded_items.len(), 1);
        assert_eq!(r.excluded_items[0].kind, PlanKind::BackendSetModel);
        assert_eq!(r.excluded_items[0].target.as_deref(), Some("planner"));
        assert!(
            r.excluded_items[0].reason.contains("qwen3.8-27b-moe"),
            "reason names the model: {}",
            r.excluded_items[0].reason
        );
        assert!(
            r.summary.contains("planner") && r.summary.contains("qwen3.8-27b-moe"),
            "summary states the exclusion: {}",
            r.summary
        );
        let round = serde_json::to_string(&r).unwrap();
        let back: Plan = serde_json::from_str(&round).unwrap();
        assert_eq!(back.excluded_items.len(), 1);
        assert_eq!(back.excluded_items[0].kind, PlanKind::BackendSetModel);
    }

    #[test]
    fn mixed_create_and_set_model_share_one_chained_add() {
        // Both orders: whichever dependent item comes first
        // chains the catalog add; the other stands on the
        // already-emitted add. Exactly one add in the plan.
        let cfg = cfg_one_model();
        for response in [
            "{\"items\":[{\"kind\":\"backend.create\",\"target\":\"planner-alt\",\"change\":{\"model_id\":\"qwen3-27b-planner\",\"port\":8090},\"rationale\":\"r\"},{\"kind\":\"backend.set_model\",\"target\":\"tool-runner\",\"change\":{\"model_id\":\"qwen3-27b-planner\"},\"rationale\":\"r\"}]}",
            "{\"items\":[{\"kind\":\"backend.set_model\",\"target\":\"tool-runner\",\"change\":{\"model_id\":\"qwen3-27b-planner\"},\"rationale\":\"r\"},{\"kind\":\"backend.create\",\"target\":\"planner-alt\",\"change\":{\"model_id\":\"qwen3-27b-planner\",\"port\":8090},\"rationale\":\"r\"}]}",
        ] {
            let r = parse_with(response, &cfg, json!({})).unwrap();
            let adds: Vec<_> = r
                .items
                .iter()
                .filter(|i| i.kind == PlanKind::ModelAdd)
                .collect();
            assert_eq!(adds.len(), 1, "plan: {:?}", r.items.iter().map(|i| i.kind).collect::<Vec<_>>());
            assert_eq!(adds[0].target.as_deref(), Some("qwen3-27b-planner"));
            assert_eq!(r.items.len(), 3);
            assert!(r
                .items
                .iter()
                .any(|i| i.kind == PlanKind::BackendCreate));
            assert!(r
                .items
                .iter()
                .any(|i| i.kind == PlanKind::BackendSetModel));
            assert!(r.excluded_items.is_empty());
        }
    }
}
