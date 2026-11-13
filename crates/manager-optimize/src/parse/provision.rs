//! Experiment provisioning: ground each experiment's candidate model,
//! chaining `model.add` / `model.download`, and re-ground stored plans.

use std::collections::HashSet;

use manager_config::AppConfig;
use serde_json::{json, Value};

use super::grounding::{candidate_model_id, candidates_from_context, chained_model_add};
use crate::experiment::{
    experiment_fingerprint, is_hf_form, ExperimentProposal, ExperimentProvisioning,
};
use crate::plan::{fingerprint, Confidence, Decision, Plan, PlanItem, PlanKind};

/// A synthesized `model.download` item for experiment
/// provisioning, built like [`chained_model_add`]: fingerprint
/// over kind + target + the empty change, the denial-memory
/// decision rule, and the experiment's own initial decision (a
/// proposal that starts denied — it names backends to stop, or
/// was denied before — starts with its provisioning denied
/// too; accepting the experiment accepts the steps).
pub(super) fn chained_model_download(
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
pub(super) fn ground_experiment_provisioning(
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
        if items.iter().any(|i| {
            i.kind == PlanKind::ModelAdd && i.target.as_deref() == Some(candidate.as_str())
        }) {
            resolved = Some((candidate.clone(), None));
        } else if let Some(c) = candidates.iter().find(|c| {
            candidate == c.repo_id
                || candidate == format!("{}/{}", c.repo_id, c.file)
                || candidate == candidate_model_id(&c.file)
        }) {
            let derived = candidate_model_id(&c.file);
            if registered.contains(derived.as_str()) {
                already_registered_as = Some(derived);
            } else if items.iter().any(|i| {
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
                already_registered_as = Some(derived);
            } else if items.iter().any(|i| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::test_fixtures::*;
    use crate::parse::*;

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
            mtp: None,
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
        assert!(plan
            .items
            .iter()
            .enumerate()
            .all(|(i, item)| item.id == i + 1));

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
                moe_block_map: None,
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
        for (idx, label) in [
            (1, "backend.create"),
            (2, "group.set_members"),
            (4, "backend.delete planner"),
        ] {
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
        assert_eq!(
            exp.decision,
            Decision::Denied,
            "requires_stopping starts denied"
        );
        let prov = exp
            .provisioning
            .as_ref()
            .expect("the experiment must carry provisioning");
        assert_eq!(prov.model_id, "qwen3.8-27b-moe");
        assert_eq!(
            prov.item_ids,
            vec![1, 7],
            "the authored add and the chained download"
        );
        let download = &plan.items[6];
        assert_eq!(download.provision_for_experiment, Some(1));
        assert_eq!(
            download.decision,
            Decision::Denied,
            "provisioning starts with the experiment's decision"
        );
        // Plan-aware validation: not a dead-end Invalid.
        let extra = vec!["Qwen/Qwen3.8-27B-MoE-GGUF/Qwen3.8-27B-MoE-Q4_K_M.gguf".to_string()];
        let ev =
            crate::experiment::validate_experiment_for_plan(exp, &cfg, &extra, &plan.items, None);
        assert_eq!(
            ev.status,
            ValidationStatus::Valid,
            "E1 verdict: {:?}",
            ev.reason
        );

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
        assert_eq!(
            accepted_plan.items[0].decision,
            Decision::Accepted,
            "linked authored add accepted"
        );
        assert_eq!(
            accepted_plan.items[6].decision,
            Decision::Accepted,
            "chained download accepted"
        );
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
            mtp: None,
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
            matches!(
                resolution,
                crate::experiment::CandidateResolution::OnDisk { .. }
            ),
            "post-provisioning resolution: {resolution:?}"
        );

        // Denying the experiment denies only the item chained
        // for it — the authored registration (which the
        // backend.create also relies on) is untouched.
        let mut denied_plan = plan.clone();
        crate::experiment::apply_experiment_decision(&mut denied_plan, 1, Decision::Denied)
            .expect("experiment exists");
        assert_eq!(
            denied_plan.items[6].decision,
            Decision::Denied,
            "chained download denied"
        );
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
            None,
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
        let ev = crate::experiment::validate_experiment_for_plan(exp, &cfg, &[], &plan.items, None);
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
        let ev = crate::experiment::validate_experiment_for_plan(exp, &cfg, &[], &r.items, None);
        assert_eq!(ev.status, crate::validate::ValidationStatus::Invalid);
        let reason = ev.reason.unwrap();
        assert!(reason.contains("not a registered model"), "{reason}");
        assert!(reason.contains("not in the curated catalog"), "{reason}");
        assert!(
            reason.contains("not among the candidates discovered"),
            "{reason}"
        );
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
}
