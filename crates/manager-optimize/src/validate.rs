//! The validation simulation (design §5).
//!
//! Items are simulated in plan order over a clone of the current
//! configuration: each item's target must resolve against the
//! simulated state, no-ops are invalid ("already satisfied"), two
//! non-denied items may not write the same field of the same
//! target, and after each item the whole simulated config must
//! pass `manager_config::validate` — a failing item is rolled
//! back and marked invalid, so later items are judged against the
//! state the earlier surviving items actually produce. Memory-
//! touching items additionally pass through the fit evaluator,
//! whose before/after figures become the row's expected effect —
//! app-computed, never model prose (§1.3).

use std::collections::HashMap;

use manager_config::{
    AppConfig, BackendConfig, ModelConfig, RestartPolicy, RoutingStrategy, SplitMode,
};
use manager_models::MemoryRequirement;
use serde_json::Value;

use crate::context::resolve_model;
use crate::fit::{evaluate_fit, gpu_total_mib, layout_requirement, others_residency, LayoutFacts};
use crate::plan::{Decision, Plan, PlanItem, PlanKind};

/// Live facts the simulation cannot derive from the config: model
/// layouts, per-GPU free VRAM (GPU-index order), and which
/// backends are currently running.
#[derive(Debug, Clone, Default)]
pub struct LiveSnapshot {
    pub layouts: HashMap<String, LayoutFacts>,
    pub free_vram_mib_per_gpu: Vec<u64>,
    pub backend_running: HashMap<String, bool>,
}

/// Per-item validation verdict (design §1.3): Valid carries the
/// app-computed expected effect; Warning names its reason;
/// Invalid names why the item cannot be applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationStatus {
    Valid,
    Warning,
    Invalid,
}

/// The app-computed expected effect for a row (§1.3): GPU/system
/// RAM before → after where the requirement changes, plus a fixed
/// note for effects no number expresses (stop-then-delete).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExpectedEffect {
    pub gpu_ram_mib: Option<(u64, u64)>,
    pub sys_ram_mib: Option<(u64, u64)>,
    pub note: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ItemValidation {
    pub status: ValidationStatus,
    pub reason: Option<String>,
    pub expected: ExpectedEffect,
}

impl ItemValidation {
    fn invalid(reason: String) -> Self {
        ItemValidation {
            status: ValidationStatus::Invalid,
            reason: Some(reason),
            expected: ExpectedEffect::default(),
        }
    }
    fn ok(expected: ExpectedEffect) -> Self {
        ItemValidation {
            status: ValidationStatus::Valid,
            reason: None,
            expected,
        }
    }
}

fn get_u32(change: &Value, key: &str) -> Option<u32> {
    change.get(key).and_then(|v| v.as_u64()).map(|n| n as u32)
}
fn get_u16(change: &Value, key: &str) -> Option<u16> {
    change.get(key).and_then(|v| v.as_u64()).map(|n| n as u16)
}
fn get_bool(change: &Value, key: &str) -> Option<bool> {
    change.get(key).and_then(|v| v.as_bool())
}
fn get_str<'a>(change: &'a Value, key: &str) -> Option<&'a str> {
    change.get(key).and_then(|v| v.as_str())
}
fn get_str_vec(change: &Value, key: &str) -> Vec<String> {
    change
        .get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}
fn get_f32_vec(change: &Value, key: &str) -> Vec<f32> {
    change
        .get(key)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect())
        .unwrap_or_default()
}

fn backend_mut<'a>(cfg: &'a mut AppConfig, id: &str) -> Option<&'a mut BackendConfig> {
    cfg.backends.iter_mut().find(|b| b.id == id)
}

fn requirement_of(
    live: &LiveSnapshot,
    cfg: &AppConfig,
    backend: &BackendConfig,
) -> Option<MemoryRequirement> {
    let model = resolve_model(cfg, backend)?;
    let facts = live.layouts.get(&model.id)?;
    let offload = manager_models::expert_offload(backend.raw_flags.as_deref());
    Some(layout_requirement(
        facts,
        offload,
        manager_models::cache_selection(backend.raw_flags.as_deref()),
        backend.flags.n_ctx,
        manager_models::effective_slots(backend.raw_flags.as_deref()),
        manager_models::moe_cache_mib(backend.raw_flags.as_deref()).unwrap_or(0),
    ))
}

fn strategy_of(s: &str) -> RoutingStrategy {
    match s {
        "least-busy" => RoutingStrategy::LeastBusy,
        "latency-based" => RoutingStrategy::LatencyBased,
        _ => RoutingStrategy::SimpleShuffle,
    }
}

fn split_eq(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| (x - y).abs() < 0.001)
}

/// The (scope, field) keys one item writes, for conflict
/// detection (§5). Scope is "b:<id>", "g:<name>", "m:<id>".
fn write_keys(item: &PlanItem) -> Vec<(String, &'static str)> {
    let target = item.target.clone().unwrap_or_default();
    let mut keys: Vec<(String, &'static str)> = Vec::new();
    let b = |f: &'static str| (format!("b:{target}"), f);
    let g = |f: &'static str| (format!("g:{target}"), f);
    match item.kind {
        PlanKind::BackendCreate => {
            keys.push(b("exists"));
            keys.push(b("model"));
            keys.push(b("enabled"));
            // A create claims only the fields its change actually
            // sets: creating a backend and then setting its
            // context is coherent sequencing, while setting the
            // same field twice — once at create, once after — is
            // the contradiction the conflict rule exists for.
            if item.change.get("n_ctx").is_some() {
                keys.push(b("n_ctx"));
            }
            if item.change.get("n_batch").is_some() {
                keys.push(b("n_batch"));
            }
            if item.change.get("tensor_split").is_some() {
                keys.push(b("placement"));
            }
            if item.change.get("offload_layers").is_some() {
                keys.push(b("offload"));
            }
            if item.change.get("moe_cache_mib").is_some() {
                keys.push(b("moe_cache"));
            }
        }
        PlanKind::BackendDelete => {
            for f in ["exists", "model", "enabled", "n_ctx", "n_batch", "offload", "moe_cache", "placement"] {
                keys.push(b(f));
            }
        }
        PlanKind::BackendSetContext => keys.push(b("n_ctx")),
        PlanKind::BackendSetFlags => {
            if item.change.get("n_ctx").is_some() {
                keys.push(b("n_ctx"));
            }
            if item.change.get("n_batch").is_some() {
                keys.push(b("n_batch"));
            }
        }
        PlanKind::BackendSetOffload => {
            keys.push(b("offload"));
            if item.change.get("moe_cache_mib").is_some() {
                keys.push(b("moe_cache"));
            }
        }
        PlanKind::BackendSetPlacement => keys.push(b("placement")),
        PlanKind::BackendSetModel => keys.push(b("model")),
        PlanKind::BackendSetEnabled => keys.push(b("enabled")),
        PlanKind::GroupSetMembers => keys.push(g("members")),
        PlanKind::GroupSetStrategy => keys.push(g("strategy")),
        PlanKind::GroupSetFallbacks => keys.push(g("fallbacks")),
        PlanKind::GroupCreate => {
            keys.push(g("exists"));
            keys.push(g("members"));
            keys.push(g("strategy"));
            keys.push(g("fallbacks"));
        }
        PlanKind::GroupDelete => {
            for f in ["exists", "members", "strategy", "fallbacks"] {
                keys.push(g(f));
            }
        }
        PlanKind::ModelAdd => keys.push((format!("m:{target}"), "exists")),
        PlanKind::ModelDownload => keys.push((format!("m:{target}"), "download")),
        PlanKind::Advice => {}
    }
    keys
}

/// Fit + warnings + expected effect for a memory-touching item.
/// `backend_id` names the affected backend in the *candidate*
/// config; `req_before` is its requirement in the pre-item state.
struct FitCheck {
    status: ValidationStatus,
    reasons: Vec<String>,
    expected: ExpectedEffect,
}

fn fit_check_for(
    item: &PlanItem,
    candidate: &AppConfig,
    live: &LiveSnapshot,
    backend_id: &str,
    req_before: Option<MemoryRequirement>,
) -> FitCheck {
    let mut reasons: Vec<String> = Vec::new();
    let mut status = ValidationStatus::Valid;
    let mut expected = ExpectedEffect::default();
    let backend = candidate.backends.iter().find(|b| b.id == backend_id);
    let Some(backend) = backend else {
        return FitCheck {
            status,
            reasons,
            expected,
        };
    };
    let req_after = requirement_of(live, candidate, backend);
    if let (Some(before), Some(after)) = (&req_before, &req_after) {
        let (gb, ga) = (gpu_total_mib(before), gpu_total_mib(after));
        if gb != ga {
            expected.gpu_ram_mib = Some((gb, ga));
        }
        if before.sys_weights_mib != after.sys_weights_mib {
            expected.sys_ram_mib = Some((before.sys_weights_mib, after.sys_weights_mib));
        }
    } else if req_before.is_none() {
        if let Some(after) = &req_after {
            expected.gpu_ram_mib = Some((0, gpu_total_mib(after)));
            if after.sys_weights_mib > 0 {
                expected.sys_ram_mib = Some((0, after.sys_weights_mib));
            }
        }
    }
    // Rated-context sanity (§5): a context above the model's
    // rated maximum is a warning, not an invalid — llama.cpp will
    // run it; quality is the operator's call.
    if let Some(model) = resolve_model(candidate, backend) {
        if let Some(facts) = live.layouts.get(&model.id) {
            if facts.ctx_max > 0 && backend.flags.n_ctx > facts.ctx_max {
                status = ValidationStatus::Warning;
                reasons.push(format!(
                    "context {} exceeds the model's rated maximum {}",
                    backend.flags.n_ctx, facts.ctx_max
                ));
            }
        }
    }
    let _ = item;
    let n_gpus = live.free_vram_mib_per_gpu.len();
    if let Some(after) = &req_after {
        if n_gpus > 0 {
            let (others_running, others_accounted) = others_residency(
                candidate,
                &live.layouts,
                &live.backend_running,
                backend_id,
                n_gpus,
            );
            let outcome = evaluate_fit(
                after,
                &backend.flags.tensor_split,
                &live.free_vram_mib_per_gpu,
                &others_running,
                &others_accounted,
            );
            if !outcome.placeable {
                let free_after: u64 = live
                    .free_vram_mib_per_gpu
                    .iter()
                    .zip(others_running.iter())
                    .map(|(f, o)| f.saturating_sub(*o))
                    .sum();
                return FitCheck {
                    status: ValidationStatus::Invalid,
                    reasons: vec![format!(
                        "does not fit: needs {} MiB GPU RAM, {} MiB free after running backends' residency",
                        gpu_total_mib(after),
                        free_after
                    )],
                    expected,
                };
            }
            if outcome.only_by_moving_accounted {
                status = ValidationStatus::Warning;
                reasons.push(
                    "fits now, but not alongside every configured backend when all are running"
                        .to_string(),
                );
            }
            if let Some(g) = outcome.split_overflow_gpu {
                status = ValidationStatus::Warning;
                reasons.push(format!(
                    "the proposed split puts more on GPU {g} than its free VRAM; a different split would fit"
                ));
            }
        }
    }
    FitCheck {
        status,
        reasons,
        expected,
    }
}

/// Validate a whole plan against the current configuration
/// (design §5). The returned vector aligns with `plan.items`.
pub fn validate_plan(plan: &Plan, cfg: &AppConfig, live: &LiveSnapshot) -> Vec<ItemValidation> {
    let mut sim = cfg.clone();
    let mut writers: HashMap<(String, &'static str), usize> = HashMap::new();
    let mut results = Vec::with_capacity(plan.items.len());
    for item in &plan.items {
        results.push(validate_one(item, &mut sim, live, &mut writers));
    }
    results
}

fn validate_one(
    item: &PlanItem,
    sim: &mut AppConfig,
    live: &LiveSnapshot,
    writers: &mut HashMap<(String, &'static str), usize>,
) -> ItemValidation {
    if item.kind.is_advice() {
        return ItemValidation::ok(ExpectedEffect::default());
    }
    // Conflict detection (§5): two accepted-or-undecided items
    // writing the same field of the same target — the second is
    // invalid. Denied items participate in neither direction.
    if item.decision != Decision::Denied {
        for key in write_keys(item) {
            if let Some(other) = writers.get(&key) {
                return ItemValidation::invalid(format!("conflicts with item {other}"));
            }
        }
    }
    let target = item.target.clone().unwrap_or_default();
    let pre = sim.clone();
    let mut candidate = sim.clone();
    let mut warnings: Vec<String> = Vec::new();
    let mut fit_backend: Option<String> = None;
    let mut req_before: Option<MemoryRequirement> = None;
    let mut expected_note: Option<String> = None;

    macro_rules! fail {
        ($reason:expr) => {
            return ItemValidation::invalid($reason)
        };
    }

    match item.kind {
        PlanKind::BackendCreate => {
            if candidate.backends.iter().any(|b| b.id == target) {
                fail!(format!("backend '{target}' already exists"));
            }
            let model_id = get_str(&item.change, "model_id").unwrap_or_default();
            let Some(model) = candidate
                .models
                .iter()
                .find(|m| m.id == model_id)
                .cloned()
            else {
                fail!(format!(
                    "model '{model_id}' is not registered; add it before creating a backend on it"
                ));
            };
            let port = get_u16(&item.change, "port").unwrap_or(0);
            if let Some(other) = candidate.backends.iter().find(|b| b.port == port) {
                fail!(format!(
                    "port {port} is already used by backend '{}'",
                    other.id
                ));
            }
            if port == candidate.gateway.port || port == candidate.postgres.port {
                fail!(format!("port {port} is claimed by the gateway or Postgres"));
            }
            let split = get_f32_vec(&item.change, "tensor_split");
            if !split.is_empty() {
                let sum: f32 = split.iter().sum();
                if (sum - 1.0).abs() > 0.01 {
                    fail!(format!("tensor split sums to {sum:.2}, not 1.0"));
                }
            }
            let mut raw_flags: Option<String> = None;
            if let Some(layers) = get_u32(&item.change, "offload_layers") {
                match live.layouts.get(&model.id).and_then(|f| f.moe_layers) {
                    Some(moe) => {
                        raw_flags = manager_models::set_expert_offload(
                            None,
                            manager_models::offload_for_layers(layers, moe),
                        );
                    }
                    None => warnings.push(
                        "offload_layers cannot be encoded: the model's MoE layer count is unknown"
                            .to_string(),
                    ),
                }
            }
            if let Some(cache) = get_u32(&item.change, "moe_cache_mib") {
                if cache > 0 {
                    match live.layouts.get(&model.id).and_then(|f| f.moe_layers) {
                        Some(_) => {
                            if get_u32(&item.change, "offload_layers").unwrap_or(0) == 0 {
                                fail!(
                                    "moe_cache_mib requires offload_layers above 0 in the same change: the cache holds the offloaded experts, and this create offloads none".to_string()
                                );
                            }
                            raw_flags = manager_models::set_moe_cache(
                                raw_flags.as_deref(),
                                Some(cache),
                            );
                        }
                        None => warnings.push(
                            "moe_cache_mib cannot be encoded: the model's MoE layer count is unknown"
                                .to_string(),
                        ),
                    }
                }
            }
            candidate.backends.push(BackendConfig {
                id: target.clone(),
                enabled: true,
                model_file: model.gguf_path.clone(),
                port,
                flags: manager_config::ServerFlags {
                    n_ctx: get_u32(&item.change, "n_ctx").unwrap_or(32768),
                    n_batch: get_u32(&item.change, "n_batch").unwrap_or(512),
                    tensor_split: split.clone(),
                    split_mode: if split.is_empty() {
                        SplitMode::None
                    } else {
                        SplitMode::Layer
                    },
                },
                raw_flags,
                flags_advanced: false,
                restart_policy: RestartPolicy {
                    max_retries: 5,
                    backoff_base_secs: 2,
                },
                overrides_global: false,
                purpose: String::new(),
                tool_call_probe: None,
            });
            if let Some(m) = candidate.models.iter_mut().find(|m| m.id == model_id) {
                if !m.assigned_backends.contains(&target) {
                    m.assigned_backends.push(target.clone());
                }
            }
            // Group placement mirrors the apply executor: the named
            // group (created when absent), else the group of a
            // sibling backend on the same model, else a new group
            // named after the backend.
            let group_name = match get_str(&item.change, "group") {
                Some(g) => g.to_string(),
                None => candidate
                    .backends
                    .iter()
                    .find(|b| b.id != target && b.model_file == model.gguf_path)
                    .and_then(|sib| {
                        candidate
                            .gateway
                            .groups
                            .iter()
                            .find(|g| g.members.iter().any(|m| m == &sib.id))
                            .map(|g| g.name.clone())
                    })
                    .unwrap_or_else(|| target.clone()),
            };
            if let Some(g) = candidate
                .gateway
                .groups
                .iter_mut()
                .find(|g| g.name == group_name)
            {
                if !g.members.contains(&target) {
                    g.members.push(target.clone());
                }
            } else {
                candidate.gateway.groups.push(manager_config::ModelGroup {
                    name: group_name,
                    members: vec![target.clone()],
                    strategy: RoutingStrategy::SimpleShuffle,
                    fallbacks: Vec::new(),
                    purpose: String::new(),
                });
            }
            fit_backend = Some(target.clone());
        }
        PlanKind::BackendDelete => {
            let Some(pos) = candidate.backends.iter().position(|b| b.id == target) else {
                fail!(format!("backend '{target}' does not exist"));
            };
            if live.backend_running.get(&target).copied().unwrap_or(false) {
                warnings.push("backend is running".to_string());
                expected_note = Some("Backend will be stopped and removed".to_string());
            }
            candidate.backends.remove(pos);
            // The apply executor scrubs the id from group
            // member lists AND fallback chains (config
            // validation rejects an unknown fallback, so a
            // delete that left one behind could never be
            // saved); the simulation mirrors it exactly, and
            // the scrub is surfaced as a warning — the plan
            // does not need a separate routing item to make
            // the delete valid, but the operator should see
            // the consequence on the delete's own row.
            let mut fallback_groups: Vec<String> = Vec::new();
            for g in candidate.gateway.groups.iter_mut() {
                g.members.retain(|m| m != &target);
                let before = g.fallbacks.len();
                g.fallbacks.retain(|f| f != &target);
                if g.fallbacks.len() != before {
                    fallback_groups.push(g.name.clone());
                }
            }
            if !fallback_groups.is_empty() {
                warnings.push(format!(
                    "deleting '{target}' also removes it from the fallback chain of: {}",
                    fallback_groups.join(", ")
                ));
            }
            for m in candidate.models.iter_mut() {
                m.assigned_backends.retain(|b| b != &target);
            }
        }
        PlanKind::BackendSetContext | PlanKind::BackendSetFlags => {
            let Some(b) = candidate.backends.iter().find(|b| b.id == target).cloned() else {
                fail!(format!("backend '{target}' does not exist"));
            };
            req_before = requirement_of(live, &pre, &b);
            let n_ctx = get_u32(&item.change, "n_ctx");
            let n_batch = get_u32(&item.change, "n_batch");
            let noop = n_ctx.map(|n| n == b.flags.n_ctx).unwrap_or(true)
                && n_batch.map(|n| n == b.flags.n_batch).unwrap_or(true);
            if noop {
                fail!("already satisfied".to_string());
            }
            let bm = backend_mut(&mut candidate, &target).unwrap();
            if let Some(n) = n_ctx {
                bm.flags.n_ctx = n;
            }
            if let Some(n) = n_batch {
                bm.flags.n_batch = n;
            }
            if n_ctx.is_some() {
                fit_backend = Some(target.clone());
            }
        }
        PlanKind::BackendSetOffload => {
            let Some(b) = candidate.backends.iter().find(|b| b.id == target).cloned() else {
                fail!(format!("backend '{target}' does not exist"));
            };
            req_before = requirement_of(live, &pre, &b);
            let Some(model) = resolve_model(&candidate, &b).cloned() else {
                fail!(format!("backend '{target}' has no resolvable model"));
            };
            let Some(moe) = live.layouts.get(&model.id).and_then(|f| f.moe_layers) else {
                fail!(format!(
                    "offload cannot be resolved for model '{}': its MoE layer count is unknown",
                    model.id
                ));
            };
            let layers = get_u32(&item.change, "offload_layers").unwrap_or(0);
            let cache = get_u32(&item.change, "moe_cache_mib");
            if let Some(c) = cache {
                if c > 0 && layers == 0 {
                    fail!(
                        "moe_cache_mib requires offload_layers above 0: the cache holds the offloaded experts, and this change offloads none"
                            .to_string()
                    );
                }
                if c > 0 && b.flags.split_mode == SplitMode::Row {
                    fail!(
                        "moe_cache_mib is not supported with a row (tensor) split: the pinned llama.cpp refuses the expert cache under tensor parallelism"
                            .to_string()
                    );
                }
            }
            let current = manager_models::expert_offload(b.raw_flags.as_deref()).layers(moe);
            let current_cache =
                manager_models::moe_cache_mib(b.raw_flags.as_deref()).unwrap_or(0);
            if current == layers && cache.map_or(true, |c| c == current_cache) {
                fail!("already satisfied".to_string());
            }
            let bm = backend_mut(&mut candidate, &target).unwrap();
            bm.raw_flags = manager_models::set_expert_offload(
                bm.raw_flags.as_deref(),
                manager_models::offload_for_layers(layers, moe),
            );
            if let Some(c) = cache {
                // The writer's disabled form: a 0 value removes
                // the token the offload rewrite just preserved.
                bm.raw_flags = manager_models::set_moe_cache(bm.raw_flags.as_deref(), Some(c));
            }
            fit_backend = Some(target.clone());
        }
        PlanKind::BackendSetPlacement => {
            let Some(b) = candidate.backends.iter().find(|b| b.id == target).cloned() else {
                fail!(format!("backend '{target}' does not exist"));
            };
            req_before = requirement_of(live, &pre, &b);
            let split = get_f32_vec(&item.change, "tensor_split");
            if !split.is_empty() {
                let sum: f32 = split.iter().sum();
                if (sum - 1.0).abs() > 0.01 {
                    fail!(format!("tensor split sums to {sum:.2}, not 1.0"));
                }
                let n_gpus = live.free_vram_mib_per_gpu.len();
                if n_gpus > 0 && split.len() != n_gpus {
                    fail!(format!(
                        "tensor split has {} entries but there are {n_gpus} GPUs",
                        split.len()
                    ));
                }
            }
            let mode = get_str(&item.change, "split_mode").map(|m| match m {
                "layer" => SplitMode::Layer,
                "row" => SplitMode::Row,
                _ => SplitMode::None,
            });
            let noop = split_eq(&split, &b.flags.tensor_split)
                && mode.map(|m| m == b.flags.split_mode).unwrap_or(true);
            if noop {
                fail!("already satisfied".to_string());
            }
            let bm = backend_mut(&mut candidate, &target).unwrap();
            bm.flags.tensor_split = split;
            if let Some(m) = mode {
                bm.flags.split_mode = m;
            }
            fit_backend = Some(target.clone());
        }
        PlanKind::BackendSetModel => {
            let Some(b) = candidate.backends.iter().find(|b| b.id == target).cloned() else {
                fail!(format!("backend '{target}' does not exist"));
            };
            req_before = requirement_of(live, &pre, &b);
            let model_id = get_str(&item.change, "model_id").unwrap_or_default();
            let Some(model) = candidate
                .models
                .iter()
                .find(|m| m.id == model_id)
                .cloned()
            else {
                fail!(format!("model '{model_id}' is not registered"));
            };
            if resolve_model(&candidate, &b).map(|m| m.id.as_str()) == Some(model_id) {
                fail!("already satisfied".to_string());
            }
            for m in candidate.models.iter_mut() {
                m.assigned_backends.retain(|x| x != &target);
            }
            if let Some(m) = candidate.models.iter_mut().find(|m| m.id == model_id) {
                m.assigned_backends.push(target.clone());
            }
            let bm = backend_mut(&mut candidate, &target).unwrap();
            bm.model_file = model.gguf_path.clone();
            fit_backend = Some(target.clone());
        }
        PlanKind::BackendSetEnabled => {
            let Some(b) = candidate.backends.iter().find(|b| b.id == target) else {
                fail!(format!("backend '{target}' does not exist"));
            };
            let enabled = get_bool(&item.change, "enabled").unwrap_or(false);
            if b.enabled == enabled {
                fail!("already satisfied".to_string());
            }
            backend_mut(&mut candidate, &target).unwrap().enabled = enabled;
        }
        PlanKind::GroupSetMembers => {
            if !candidate.gateway.groups.iter().any(|g| g.name == target) {
                fail!(format!("group '{target}' does not exist"));
            }
            let members = get_str_vec(&item.change, "members");
            for m in &members {
                if !candidate.backends.iter().any(|b| &b.id == m) && !m.contains(':') {
                    fail!(format!("member '{m}' is not a backend"));
                }
            }
            let g = candidate
                .gateway
                .groups
                .iter_mut()
                .find(|g| g.name == target)
                .unwrap();
            if g.members == members {
                fail!("already satisfied".to_string());
            }
            g.members = members;
        }
        PlanKind::GroupSetStrategy => {
            if !candidate.gateway.groups.iter().any(|g| g.name == target) {
                fail!(format!("group '{target}' does not exist"));
            }
            let strategy = strategy_of(get_str(&item.change, "strategy").unwrap_or(""));
            let g = candidate
                .gateway
                .groups
                .iter_mut()
                .find(|g| g.name == target)
                .unwrap();
            if g.strategy == strategy {
                fail!("already satisfied".to_string());
            }
            g.strategy = strategy;
        }
        PlanKind::GroupSetFallbacks => {
            if !candidate.gateway.groups.iter().any(|g| g.name == target) {
                fail!(format!("group '{target}' does not exist"));
            }
            let fallbacks = get_str_vec(&item.change, "fallbacks");
            let g = candidate
                .gateway
                .groups
                .iter_mut()
                .find(|g| g.name == target)
                .unwrap();
            if g.fallbacks == fallbacks {
                fail!("already satisfied".to_string());
            }
            g.fallbacks = fallbacks;
        }
        PlanKind::GroupCreate => {
            if candidate.gateway.groups.iter().any(|g| g.name == target) {
                fail!(format!("group '{target}' already exists"));
            }
            let members = get_str_vec(&item.change, "members");
            for m in &members {
                if !candidate.backends.iter().any(|b| &b.id == m) && !m.contains(':') {
                    fail!(format!("member '{m}' is not a backend"));
                }
            }
            candidate.gateway.groups.push(manager_config::ModelGroup {
                name: target.clone(),
                members,
                strategy: strategy_of(get_str(&item.change, "strategy").unwrap_or("")),
                fallbacks: get_str_vec(&item.change, "fallbacks"),
                purpose: String::new(),
            });
        }
        PlanKind::GroupDelete => {
            let Some(pos) = candidate
                .gateway
                .groups
                .iter()
                .position(|g| g.name == target)
            else {
                fail!(format!("group '{target}' does not exist"));
            };
            candidate.gateway.groups.remove(pos);
        }
        PlanKind::ModelAdd => {
            if candidate.models.iter().any(|m| m.id == target) {
                fail!(format!("model '{target}' already exists"));
            }
            let url = get_str(&item.change, "source_url").unwrap_or_default();
            let file_name = url.rsplit('/').next().unwrap_or("");
            if !url.starts_with("https://") || !file_name.to_lowercase().ends_with(".gguf") {
                fail!("source_url must be an https URL of a .gguf file".to_string());
            }
            // Standing warning (§4): the warning is the control on
            // an executable item whose inputs are model-supplied.
            warnings.push(
                "registers the model only — the source URL and any hash are model-supplied and unverified; confirm the pinned hash in the Models view before downloading"
                    .to_string(),
            );
            // The real model_add resolves the figure from the
            // item, then the catalog (by id, then by source URL —
            // tracker #32), and refuses to guess. The sim mirrors
            // that resolution exactly (tracker #38): an item whose
            // count no source supplies would fail at apply, so it
            // is invalid here, at plan time, with the same message
            // model_add would give — never masked by a placeholder.
            // A positive value on the item itself (supplied by the
            // consultation, grounded from a discovered candidate at
            // parse, or edited onto the item afterwards) is the
            // value the simulation carries, exactly as apply does.
            let Some(params_b) = item
                .change
                .get("params_b")
                .and_then(|v| v.as_f64())
                .filter(|p| *p > 0.0)
                .map(|p| p as f32)
                .or_else(|| manager_models::catalog_params_b(&target, url))
            else {
                fail!(format!(
                    "parameter count (params_b) is unknown for model '{target}': no value was supplied on the plan item and the model is not in the curated catalog — supply the model's total parameter count in billions (e.g. 80 for an 80B model) on the model.add item"
                ));
            };
            candidate.models.push(ModelConfig {
                id: target.clone(),
                gguf_path: std::path::PathBuf::from("models").join(file_name),
                quant: get_str(&item.change, "quant")
                    .map(|q| q.to_string())
                    .filter(|q| !q.is_empty())
                    .unwrap_or_else(|| "unknown".to_string()),
                params_b,
                sha256: String::new(),
                verified_at: None,
                verified_sha256: None,
                source_url: url.to_string(),
                assigned_backends: Vec::new(),
                enabled: true,
                display_name: None,
                moe_layers: None,
                default_offload_layers: None,
                mtp: None,
            });
        }
        PlanKind::ModelDownload => {
            // A download changes no configuration — it fetches
            // the registered model's file — so the item is
            // valid exactly when the model exists at this point
            // in the simulated plan (registered in the config,
            // or registered by an earlier item). Whether the
            // file is already on disk is the executor's check,
            // not the simulation's: re-downloading is harmless.
            if !candidate.models.iter().any(|m| m.id == target) {
                fail!(format!(
                    "model '{target}' is not registered; register it before downloading it"
                ));
            }
            expected_note = Some("Model file will be downloaded".to_string());
        }
        PlanKind::Advice => unreachable!("advice returns earlier"),
    }

    // Whole-config structural validity after the item (§5): a
    // failing item is invalid and rolled back.
    if let Err(e) = manager_config::validate(&candidate) {
        return ItemValidation::invalid(format!("would leave the configuration invalid: {e}"));
    }

    let mut status = if warnings.is_empty() {
        ValidationStatus::Valid
    } else {
        ValidationStatus::Warning
    };
    let mut expected = ExpectedEffect {
        note: expected_note,
        ..Default::default()
    };
    if let Some(backend_id) = &fit_backend {
        let fit = fit_check_for(item, &candidate, live, backend_id, req_before);
        expected.gpu_ram_mib = fit.expected.gpu_ram_mib;
        expected.sys_ram_mib = fit.expected.sys_ram_mib;
        if fit.status == ValidationStatus::Invalid {
            return ItemValidation::invalid(fit.reasons.join("; "));
        }
        if fit.status == ValidationStatus::Warning {
            status = ValidationStatus::Warning;
            warnings.extend(fit.reasons);
        }
    }

    // Commit: a non-denied item shapes the state later items are
    // judged against and registers its writes. A denied item is
    // still validated on its own merits (the operator may flip
    // it), but it will never be applied, so it must not shape the
    // simulation or block a later item's write.
    if item.decision != Decision::Denied {
        *sim = candidate;
        for key in write_keys(item) {
            writers.insert(key, item.id);
        }
    }
    ItemValidation {
        status,
        reason: if warnings.is_empty() {
            None
        } else {
            Some(warnings.join("; "))
        },
        expected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Confidence, Plan};
    use serde_json::json;
    use std::path::PathBuf;

    fn item(
        id: usize,
        kind: PlanKind,
        target: &str,
        change: Value,
        decision: Decision,
    ) -> PlanItem {
        PlanItem {
            id,
            fingerprint: format!("fp{id}"),
            kind,
            target: Some(target.to_string()),
            change,
            advice_text: None,
            rationale: String::new(),
            expected_effect: String::new(),
            confidence: Confidence::Medium,
            decision,
            previously_denied: false,
            provision_for_experiment: None,
            apply_outcome: None,
            apply_error: None,
        }
    }

    fn plan(items: Vec<PlanItem>) -> Plan {
        Plan {
            summary: String::new(),
            directive: String::new(),
            experiments: Vec::new(),
            experiments_dropped_invalid: 0,
            experiments_truncated: 0,
            items,
            excluded_items: Vec::new(),
            consulted_backend: "planner".to_string(),
            created_at: String::new(),
            config_hash: String::new(),
            dropped_invalid: 0,
            truncated: 0,
            dropped_fragments: vec![],
            raw_response: String::new(),
            context: json!({}),
        }
    }

    fn cfg_with_model() -> AppConfig {
        // A minimal VALID config: default_config() alone does not
        // validate (its backends reference model files that only
        // catalog seeding registers), so trim to one backend whose
        // model is registered here.
        let mut cfg = AppConfig::default_config();
        cfg.backends.retain(|b| b.id == "tool-runner");
        cfg.gateway.groups = vec![manager_config::ModelGroup {
            name: "tool-runner".to_string(),
            members: vec!["tool-runner".to_string()],
            strategy: RoutingStrategy::SimpleShuffle,
            fallbacks: Vec::new(),
            purpose: String::new(),
        }];
        cfg.models.push(ModelConfig {
            id: "qwen3-8b".to_string(),
            gguf_path: PathBuf::from("models/qwen3-8b.gguf"),
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
            mtp: None,
        });
        cfg.backends[0].model_file = PathBuf::from("models/qwen3-8b.gguf");
        assert!(
            manager_config::validate(&cfg).is_ok(),
            "fixture must validate: {:?}",
            manager_config::validate(&cfg)
        );
        cfg
    }

    fn live_for(cfg: &AppConfig) -> LiveSnapshot {
        let mut layouts = HashMap::new();
        layouts.insert(
            "qwen3-8b".to_string(),
            LayoutFacts {
                total_bytes: 5 * 1024 * manager_models::MIB,
                expert_bytes: None,
                expert_bytes_by_block: None,
                moe_layers: None,
                kv_per_token: Some(8192),
                kv_geometry: None,
                rs_bytes_per_seq: None,
                basis: manager_models::MemoryBasis::Estimate,
                ctx_max: 131072,
                params_b: 8.0,
            },
        );
        let _ = cfg;
        LiveSnapshot {
            layouts,
            free_vram_mib_per_gpu: vec![16384, 16384],
            backend_running: HashMap::new(),
        }
    }

    #[test]
    fn noop_and_missing_target_are_invalid() {
        let cfg = cfg_with_model();
        let live = live_for(&cfg);
        let p = plan(vec![
            item(
                1,
                PlanKind::BackendSetContext,
                "tool-runner",
                json!({"n_ctx": 32768}),
                Decision::Undecided,
            ),
            item(
                2,
                PlanKind::BackendSetContext,
                "ghost",
                json!({"n_ctx": 65536}),
                Decision::Undecided,
            ),
        ]);
        let out = validate_plan(&p, &cfg, &live);
        assert_eq!(out[0].status, ValidationStatus::Invalid);
        assert_eq!(out[0].reason.as_deref(), Some("already satisfied"));
        assert_eq!(out[1].status, ValidationStatus::Invalid);
    }

    #[test]
    fn second_write_to_same_field_conflicts() {
        let cfg = cfg_with_model();
        let live = live_for(&cfg);
        let p = plan(vec![
            item(
                1,
                PlanKind::BackendSetContext,
                "tool-runner",
                json!({"n_ctx": 65536}),
                Decision::Accepted,
            ),
            item(
                2,
                PlanKind::BackendSetFlags,
                "tool-runner",
                json!({"n_ctx": 131072}),
                Decision::Undecided,
            ),
        ]);
        let out = validate_plan(&p, &cfg, &live);
        assert_eq!(out[0].status, ValidationStatus::Valid);
        assert_eq!(out[1].status, ValidationStatus::Invalid);
        assert_eq!(out[1].reason.as_deref(), Some("conflicts with item 1"));
    }

    #[test]
    fn denied_items_do_not_conflict_or_simulate() {
        let cfg = cfg_with_model();
        let live = live_for(&cfg);
        let p = plan(vec![
            item(
                1,
                PlanKind::BackendSetContext,
                "tool-runner",
                json!({"n_ctx": 65536}),
                Decision::Denied,
            ),
            item(
                2,
                PlanKind::BackendSetContext,
                "tool-runner",
                json!({"n_ctx": 65536}),
                Decision::Undecided,
            ),
        ]);
        let out = validate_plan(&p, &cfg, &live);
        // The denied item validates as a change (it is not a no-op
        // against the untouched config), but it does not register
        // its write, so item 2 is judged against the same state and
        // is not a conflict.
        assert_eq!(out[0].status, ValidationStatus::Valid);
        assert_eq!(out[1].status, ValidationStatus::Valid);
    }

    #[test]
    fn sequential_items_see_each_others_effects() {
        let cfg = cfg_with_model();
        let live = live_for(&cfg);
        // Create a backend, then set its context: the second item
        // resolves only because the first simulated.
        let p = plan(vec![
            item(
                1,
                PlanKind::BackendCreate,
                "extra",
                json!({"model_id": "qwen3-8b", "port": 8090, "group": "planner"}),
                Decision::Undecided,
            ),
            item(
                2,
                PlanKind::BackendSetContext,
                "extra",
                json!({"n_ctx": 65536}),
                Decision::Undecided,
            ),
        ]);
        let out = validate_plan(&p, &cfg, &live);
        assert_eq!(
            out[0].status,
            ValidationStatus::Valid,
            "create: {:?}",
            out[0].reason
        );
        assert_eq!(
            out[1].status,
            ValidationStatus::Valid,
            "set_context: {:?}",
            out[1].reason
        );
        // The created backend's expected effect counts from zero.
        assert_eq!(out[0].expected.gpu_ram_mib.map(|(_, a)| a > 0), Some(true));
    }

    #[test]
    fn ctx_above_rated_maximum_warns() {
        let cfg = cfg_with_model();
        let live = live_for(&cfg);
        let p = plan(vec![item(
            1,
            PlanKind::BackendSetContext,
            "tool-runner",
            json!({"n_ctx": 262144}),
            Decision::Undecided,
        )]);
        let out = validate_plan(&p, &cfg, &live);
        assert_eq!(out[0].status, ValidationStatus::Warning);
        assert!(out[0].reason.as_ref().unwrap().contains("rated maximum"));
    }

    #[test]
    fn running_backend_delete_warns_with_stop_note() {
        let cfg = cfg_with_model();
        let mut live = live_for(&cfg);
        live.backend_running.insert("tool-runner".to_string(), true);
        let p = plan(vec![item(
            1,
            PlanKind::BackendDelete,
            "tool-runner",
            json!({}),
            Decision::Denied,
        )]);
        let out = validate_plan(&p, &cfg, &live);
        assert_eq!(out[0].status, ValidationStatus::Warning);
        assert_eq!(
            out[0].expected.note.as_deref(),
            Some("Backend will be stopped and removed")
        );
    }

    #[test]
    fn backend_delete_scrubs_group_fallbacks_and_warns() {
        // The fixture defect: a group names the backend in its
        // fallback chain, and no plan item rewrites that chain.
        // The delete itself scrubs fallback references (the
        // executor does the same), so the item is a Warning
        // naming the scrub — not Invalid for a condition the
        // delete's own application resolves.
        let mut cfg = cfg_with_model();
        cfg.gateway.groups[0].fallbacks = vec!["tool-runner".to_string()];
        assert!(manager_config::validate(&cfg).is_ok());
        let live = live_for(&cfg);
        let p = plan(vec![item(
            1,
            PlanKind::BackendDelete,
            "tool-runner",
            json!({}),
            Decision::Denied,
        )]);
        let out = validate_plan(&p, &cfg, &live);
        assert_eq!(
            out[0].status,
            ValidationStatus::Warning,
            "delete verdict: {:?}",
            out[0].reason
        );
        let reason = out[0].reason.clone().unwrap_or_default();
        assert!(reason.contains("fallback chain of: tool-runner"), "{reason}");
    }

    #[test]
    fn model_add_always_warns_and_bad_url_is_invalid() {
        let cfg = cfg_with_model();
        let live = live_for(&cfg);
        let p = plan(vec![
            item(
                1,
                PlanKind::ModelAdd,
                "new-model",
                json!({"source_url": "https://example.com/new.gguf", "params_b": 12.0}),
                Decision::Undecided,
            ),
            item(
                2,
                PlanKind::ModelAdd,
                "bad-model",
                json!({"source_url": "http://example.com/bad.gguf"}),
                Decision::Undecided,
            ),
        ]);
        let out = validate_plan(&p, &cfg, &live);
        assert_eq!(out[0].status, ValidationStatus::Warning);
        assert_eq!(out[1].status, ValidationStatus::Invalid);
    }

    #[test]
    fn model_add_unknown_size_without_params_b_is_invalid_at_plan_time() {
        // Tracker #38: an unknown-size, non-catalog model.add
        // with no params_b on the item can never apply (the real
        // model_add refuses to guess, tracker #32), so validation
        // must say so at plan time — the old positive placeholder
        // let it validate as a warning and fail only at apply.
        let cfg = cfg_with_model();
        let live = live_for(&cfg);
        let p = plan(vec![item(
            1,
            PlanKind::ModelAdd,
            "obscure-model",
            json!({"source_url": "https://example.com/obscure.gguf"}),
            Decision::Undecided,
        )]);
        let out = validate_plan(&p, &cfg, &live);
        assert_eq!(out[0].status, ValidationStatus::Invalid);
        let reason = out[0].reason.clone().unwrap_or_default();
        assert!(reason.contains("params_b"), "reason names the gap: {reason}");
        assert!(reason.contains("obscure-model"), "reason names the model: {reason}");
        assert!(
            reason.contains("not in the curated catalog"),
            "reason states the catalog miss: {reason}"
        );
    }

    #[test]
    fn model_add_with_consultation_params_b_validates() {
        // Tracker #38: a consultation-supplied params_b on the
        // item is the value validation carries — the add is
        // actionable (the standing warning only), exactly as
        // apply passes the same field to model_add.
        let cfg = cfg_with_model();
        let live = live_for(&cfg);
        let p = plan(vec![item(
            1,
            PlanKind::ModelAdd,
            "obscure-model",
            json!({"source_url": "https://example.com/obscure.gguf", "params_b": 13.5}),
            Decision::Undecided,
        )]);
        let out = validate_plan(&p, &cfg, &live);
        assert_eq!(
            out[0].status,
            ValidationStatus::Warning,
            "verdict: {:?}",
            out[0].reason
        );
    }

    #[test]
    fn model_add_operator_edited_params_b_validates() {
        // Tracker #38: an item that arrived without a count is
        // invalid; an operator edit that puts a positive
        // params_b on the item's change makes the same item
        // actionable on re-validation — validation reads the
        // item's current change, not its provenance.
        let cfg = cfg_with_model();
        let live = live_for(&cfg);
        let mut p = plan(vec![item(
            1,
            PlanKind::ModelAdd,
            "obscure-model",
            json!({"source_url": "https://example.com/obscure.gguf"}),
            Decision::Undecided,
        )]);
        let out = validate_plan(&p, &cfg, &live);
        assert_eq!(out[0].status, ValidationStatus::Invalid);
        p.items[0].change["params_b"] = json!(21.0);
        let out = validate_plan(&p, &cfg, &live);
        assert_eq!(
            out[0].status,
            ValidationStatus::Warning,
            "verdict after edit: {:?}",
            out[0].reason
        );
    }

    #[test]
    fn model_add_catalog_model_needs_no_params_b_on_the_item() {
        // Tracker #38: catalog models are unaffected — the count
        // resolves from the catalog by id (an unrelated URL) and
        // by source URL (an operator-chosen id), the same two
        // arms model_add itself uses (tracker #32).
        let cfg = cfg_with_model();
        let live = live_for(&cfg);
        let spec = manager_models::curated_models()
            .into_iter()
            .find(|s| s.id == "qwen3-coder-30b")
            .expect("catalog carries the coder model");
        let p = plan(vec![
            item(
                1,
                PlanKind::ModelAdd,
                "qwen3-coder-30b",
                json!({"source_url": "https://example.com/renamed.gguf"}),
                Decision::Undecided,
            ),
            item(
                2,
                PlanKind::ModelAdd,
                "my-coder-copy",
                json!({"source_url": spec.url}),
                Decision::Undecided,
            ),
        ]);
        let out = validate_plan(&p, &cfg, &live);
        assert_eq!(
            out[0].status,
            ValidationStatus::Warning,
            "by-id verdict: {:?}",
            out[0].reason
        );
        assert_eq!(
            out[1].status,
            ValidationStatus::Warning,
            "by-url verdict: {:?}",
            out[1].reason
        );
    }

    /// The dense fixture's model as an MoE model in the live
    /// facts (48 MoE layers with expert bytes), its backend
    /// carrying a partial offload.
    fn moe_fixture() -> (AppConfig, LiveSnapshot) {
        let mut cfg = cfg_with_model();
        let mut live = live_for(&cfg);
        let facts = live.layouts.get_mut("qwen3-8b").unwrap();
        facts.expert_bytes = Some(20 * 1024 * manager_models::MIB);
        facts.moe_layers = Some(48);
        cfg.backends[0].raw_flags = Some("--n-cpu-moe 16".to_string());
        (cfg, live)
    }

    #[test]
    fn set_offload_cache_follows_the_control_discipline() {
        let (cfg, live) = moe_fixture();
        // Offload + cache together: valid, and the expected GPU
        // total carries the cache (the same item without it
        // prices exactly 4096 less).
        let with = plan(vec![item(
            1,
            PlanKind::BackendSetOffload,
            "tool-runner",
            json!({"offload_layers": 24, "moe_cache_mib": 4096}),
            Decision::Undecided,
        )]);
        let out = validate_plan(&with, &cfg, &live);
        assert_eq!(out[0].status, ValidationStatus::Valid, "{:?}", out[0].reason);
        let gpu_with = out[0].expected.gpu_ram_mib.expect("fit verdict priced").1;
        let without = plan(vec![item(
            1,
            PlanKind::BackendSetOffload,
            "tool-runner",
            json!({"offload_layers": 24}),
            Decision::Undecided,
        )]);
        let out = validate_plan(&without, &cfg, &live);
        assert_eq!(out[0].status, ValidationStatus::Valid, "{:?}", out[0].reason);
        let gpu_without = out[0].expected.gpu_ram_mib.expect("fit verdict priced").1;
        assert_eq!(gpu_with, gpu_without + 4096);
        // A cache with no offload in the same change: invalid.
        let p = plan(vec![item(
            1,
            PlanKind::BackendSetOffload,
            "tool-runner",
            json!({"offload_layers": 0, "moe_cache_mib": 4096}),
            Decision::Undecided,
        )]);
        let out = validate_plan(&p, &cfg, &live);
        assert_eq!(out[0].status, ValidationStatus::Invalid);
        assert!(
            out[0].reason.as_deref().unwrap_or("").contains("offload_layers above 0"),
            "{:?}",
            out[0].reason
        );
        // A cache-only change (the saved offload restated, a new
        // cache) is a real change, not "already satisfied"; the
        // exact saved state (offload 16, cache 0) is.
        let p = plan(vec![item(
            1,
            PlanKind::BackendSetOffload,
            "tool-runner",
            json!({"offload_layers": 16, "moe_cache_mib": 4096}),
            Decision::Undecided,
        )]);
        let out = validate_plan(&p, &cfg, &live);
        assert_eq!(out[0].status, ValidationStatus::Valid, "{:?}", out[0].reason);
        let p = plan(vec![item(
            1,
            PlanKind::BackendSetOffload,
            "tool-runner",
            json!({"offload_layers": 16, "moe_cache_mib": 0}),
            Decision::Undecided,
        )]);
        let out = validate_plan(&p, &cfg, &live);
        assert_eq!(out[0].status, ValidationStatus::Invalid);
        assert_eq!(out[0].reason.as_deref(), Some("already satisfied"));
    }

    #[test]
    fn create_cache_requires_offload_in_the_same_change() {
        let (cfg, live) = moe_fixture();
        // A create naming a cache but no offload: invalid.
        let p = plan(vec![item(
            1,
            PlanKind::BackendCreate,
            "new-moe",
            json!({"model_id": "qwen3-8b", "port": 8091, "group": "tool-runner", "moe_cache_mib": 4096}),
            Decision::Undecided,
        )]);
        let out = validate_plan(&p, &cfg, &live);
        assert_eq!(out[0].status, ValidationStatus::Invalid, "{:?}", out[0].reason);
        assert!(
            out[0].reason.as_deref().unwrap_or("").contains("offload_layers above 0"),
            "{:?}",
            out[0].reason
        );
        // Offload + cache together: valid.
        let p = plan(vec![item(
            1,
            PlanKind::BackendCreate,
            "new-moe",
            json!({"model_id": "qwen3-8b", "port": 8091, "group": "tool-runner", "offload_layers": 24, "moe_cache_mib": 4096}),
            Decision::Undecided,
        )]);
        let out = validate_plan(&p, &cfg, &live);
        assert_eq!(out[0].status, ValidationStatus::Valid, "{:?}", out[0].reason);
    }

    #[test]
    fn create_cache_on_a_dense_model_warns_like_its_offload() {
        let cfg = cfg_with_model();
        let live = live_for(&cfg);
        // The model's MoE layer count is unknown (dense fixture):
        // the cache cannot be encoded — a warning, mirroring the
        // create-offload posture, not a failure.
        let p = plan(vec![item(
            1,
            PlanKind::BackendCreate,
            "new-dense",
            json!({"model_id": "qwen3-8b", "port": 8091, "group": "tool-runner", "offload_layers": 4, "moe_cache_mib": 4096}),
            Decision::Undecided,
        )]);
        let out = validate_plan(&p, &cfg, &live);
        assert_eq!(out[0].status, ValidationStatus::Warning, "{:?}", out[0].reason);
        assert!(
            out[0].reason.as_deref().unwrap_or("").contains("moe_cache_mib cannot be encoded"),
            "{:?}",
            out[0].reason
        );
    }
}
