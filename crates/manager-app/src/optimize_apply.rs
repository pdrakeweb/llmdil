//! The Rust apply executor for Optimize plans (docs/optimize-design.md
//! §5.2 "Apply execution", §7 Phase 3).
//!
//! Phase 1 applied plans from the UI because every mutation's
//! validation, journal recording, and persistence lived in the
//! command handlers, and a Rust executor duplicating them would have
//! been a second mutation path. The admin API's ops-layer extraction
//! has since landed (`manager-app`'s `op_*` functions are what both
//! the Tauri commands and the REST API call), so this executor sits
//! on top of that layer exactly as the design anticipated: every
//! item executes through the same `op_*` functions the UI
//! orchestration called, in the same composition, so validation,
//! the activation journal's `record_*` entries, and persistence are
//! the ops' own — there is still exactly one mutation path.
//!
//! The rules are §5.2's, unchanged:
//!
//! - Validation re-runs against live state first; only **accepted**
//!   items that are not Invalid (and not already applied) execute.
//! - Execution is sequential in plan order and **halts on the first
//!   failure**; the remaining queued items are recorded Not
//!   attempted.
//! - **No rollback**: applied items are ordinary configuration saves.
//! - Destructive items (deletes) run only in the destructive phase,
//!   which the caller requests explicitly (the UI's armed confirm).
//! - Apply saves configuration only; activation stays with the
//!   Activation Plan (§5.3). Nothing here restarts a backend; a
//!   running backend's delete is stop-then-delete (§5.4).
//!
//! A **dry run** performs the same re-validation and translation and
//! returns, per queued item, the operations it would run — without
//! executing any of them or recording outcomes.

use std::sync::Arc;
use std::time::Duration;

use manager_optimize::{
    validate_plan, ApplyOutcome, Decision, PlanItem, PlanKind, ValidationStatus,
};
use serde_json::{json, Value};

use crate::optimize::{
    current_config_hash, gather, load_plan_file_current, op_optimize_set_item_outcome, opt_log1,
    state_payload, validation_json,
};
use crate::optimize_stream::ApplyGuard;
use crate::{ApiError, ApiResult, AppState, EventSink};

/// How long a running backend's stop may take before its delete
/// item fails (the UI orchestration's figure).
const STOP_DEADLINE: Duration = Duration::from_secs(30);

/// How long a provisioning download may take before its item fails;
/// the download itself continues in the background (the UI
/// orchestration's figure).
const DOWNLOAD_DEADLINE: Duration = Duration::from_secs(2 * 60 * 60);

/// A gateway-group edit, applied to the live group list at execution
/// time (an earlier item may have changed it).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GroupEdit {
    SetMembers(Vec<String>),
    SetStrategy(String),
    SetFallbacks(Vec<String>),
    Create {
        members: Vec<String>,
        strategy: String,
        fallbacks: Vec<String>,
    },
    Delete,
}

/// One existing operation an item executes as (§4's "Executes as"
/// column). Values the item does not carry (the other flag of an
/// `update_flags` pair, a model assign's split, a create's default
/// group) are resolved from live configuration at execution time.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PlannedOp {
    BackendAdd {
        id: String,
        model_id: String,
        port: u16,
        group: Option<String>,
    },
    UpdateFlags {
        id: String,
        n_ctx: Option<u32>,
        n_batch: Option<u32>,
    },
    SetOffload {
        id: String,
        layers: u32,
    },
    /// `mib == 0` turns the cache off.
    SetMoeCache {
        id: String,
        mib: u32,
    },
    ApplyConfig {
        id: String,
        patch: Value,
    },
    ModelAssign {
        backend_id: String,
        model_id: String,
    },
    StopIfRunning {
        id: String,
    },
    BackendDelete {
        id: String,
    },
    Groups {
        name: String,
        edit: GroupEdit,
    },
    ModelAdd {
        id: String,
        source_url: String,
        quant: Option<String>,
        params_b: Option<f32>,
    },
    ModelDownload {
        id: String,
    },
}

impl PlannedOp {
    /// The registry name of the operation this step calls.
    pub(crate) fn op_name(&self) -> &'static str {
        match self {
            PlannedOp::BackendAdd { .. } => "backend_add",
            PlannedOp::UpdateFlags { .. } => "backend_update_flags",
            PlannedOp::SetOffload { .. } => "backend_set_offload",
            PlannedOp::SetMoeCache { .. } => "backend_set_moe_cache",
            PlannedOp::ApplyConfig { .. } => "backend_apply_config",
            PlannedOp::ModelAssign { .. } => "model_assign",
            PlannedOp::StopIfRunning { .. } => "stop_backend",
            PlannedOp::BackendDelete { .. } => "backend_delete",
            PlannedOp::Groups { .. } => "update_gateway_config",
            PlannedOp::ModelAdd { .. } => "model_add",
            PlannedOp::ModelDownload { .. } => "download_model",
        }
    }

    /// One line naming the operation and its arguments (dry-run
    /// output and the Optimize log).
    pub(crate) fn describe(&self) -> String {
        let args = match self {
            PlannedOp::BackendAdd {
                id,
                model_id,
                port,
                group,
            } => format!(
                "id={id}, model_id={model_id}, port={port}, group={}",
                group.as_deref().unwrap_or("(sibling's or new)")
            ),
            PlannedOp::UpdateFlags { id, n_ctx, n_batch } => format!(
                "id={id}, n_ctx={}, n_batch={}",
                n_ctx.map(|n| n.to_string()).unwrap_or_else(|| "(current)".into()),
                n_batch
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "(current)".into())
            ),
            PlannedOp::SetOffload { id, layers } => format!("id={id}, layers={layers}"),
            PlannedOp::SetMoeCache { id, mib } => {
                if *mib == 0 {
                    format!("id={id}, mib=off")
                } else {
                    format!("id={id}, mib={mib}")
                }
            }
            PlannedOp::ApplyConfig { id, patch } => {
                format!("id={id}, patch={patch}, apply_mode=OnNextRestart")
            }
            PlannedOp::ModelAssign {
                backend_id,
                model_id,
            } => format!("model={model_id}, backend={backend_id}, split=(current)"),
            PlannedOp::StopIfRunning { id } => format!("id={id}, only if running, graceful"),
            PlannedOp::BackendDelete { id } => format!("id={id}"),
            PlannedOp::Groups { name, edit } => match edit {
                GroupEdit::SetMembers(m) => format!("group {name}: members={m:?}"),
                GroupEdit::SetStrategy(s) => format!("group {name}: strategy={s}"),
                GroupEdit::SetFallbacks(f) => format!("group {name}: fallbacks={f:?}"),
                GroupEdit::Create {
                    members,
                    strategy,
                    fallbacks,
                } => format!(
                    "create group {name}: members={members:?}, strategy={strategy}, fallbacks={fallbacks:?}"
                ),
                GroupEdit::Delete => format!("delete group {name}"),
            },
            PlannedOp::ModelAdd {
                id,
                source_url,
                quant,
                params_b,
            } => format!(
                "id={id}, source_url={source_url}{}{}",
                quant.as_ref().map(|q| format!(", quant={q}")).unwrap_or_default(),
                params_b
                    .map(|p| format!(", params_b={p}"))
                    .unwrap_or_default()
            ),
            PlannedOp::ModelDownload { id } => format!("id={id}, wait for verified file"),
        };
        format!("{}({args})", self.op_name())
    }
}

fn u32_of(change: &Value, key: &str) -> Option<u32> {
    change
        .get(key)
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok())
}

fn str_of(change: &Value, key: &str) -> Option<String> {
    change
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn str_vec_of(change: &Value, key: &str) -> Vec<String> {
    change
        .get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Translate one plan item into the operations it executes as
/// (design §4's mapping table; the same composition the Phase 1
/// UI orchestration used). Pure over the item: the parser has
/// already normalized `change` to exactly the fields the kind
/// admits.
pub(crate) fn item_operations(item: &PlanItem) -> Result<Vec<PlannedOp>, String> {
    let c = &item.change;
    let target = || {
        item.target
            .clone()
            .ok_or_else(|| format!("item {} has no target", item.id))
    };
    let need_u32 =
        |key: &str| u32_of(c, key).ok_or_else(|| format!("item {} has no {key}", item.id));
    let need_str =
        |key: &str| str_of(c, key).ok_or_else(|| format!("item {} has no {key}", item.id));
    Ok(match item.kind {
        PlanKind::BackendCreate => {
            let id = target()?;
            let port = match c.get("port").and_then(|v| v.as_u64()) {
                Some(p) => u16::try_from(p).map_err(|_| format!("port {p} is out of range"))?,
                None => 0,
            };
            let mut ops = vec![PlannedOp::BackendAdd {
                id: id.clone(),
                model_id: need_str("model_id")?,
                port,
                group: str_of(c, "group").filter(|g| !g.trim().is_empty()),
            }];
            let (n_ctx, n_batch) = (u32_of(c, "n_ctx"), u32_of(c, "n_batch"));
            if n_ctx.is_some() || n_batch.is_some() {
                ops.push(PlannedOp::UpdateFlags {
                    id: id.clone(),
                    n_ctx,
                    n_batch,
                });
            }
            if let Some(layers) = u32_of(c, "offload_layers") {
                ops.push(PlannedOp::SetOffload {
                    id: id.clone(),
                    layers,
                });
            }
            if let Some(mib) = u32_of(c, "moe_cache_mib") {
                ops.push(PlannedOp::SetMoeCache {
                    id: id.clone(),
                    mib,
                });
            }
            if let Some(split) = c.get("tensor_split").filter(|v| v.is_array()) {
                ops.push(PlannedOp::ApplyConfig {
                    id,
                    patch: json!({ "tensor_split": split, "split_mode": "layer" }),
                });
            }
            ops
        }
        PlanKind::BackendDelete => {
            let id = target()?;
            vec![
                PlannedOp::StopIfRunning { id: id.clone() },
                PlannedOp::BackendDelete { id },
            ]
        }
        PlanKind::BackendSetContext | PlanKind::BackendSetFlags => {
            let (n_ctx, n_batch) = (u32_of(c, "n_ctx"), u32_of(c, "n_batch"));
            if n_ctx.is_none() && n_batch.is_none() {
                return Err(format!("item {} changes no flag", item.id));
            }
            vec![PlannedOp::UpdateFlags {
                id: target()?,
                n_ctx,
                n_batch,
            }]
        }
        PlanKind::BackendSetOffload => {
            let id = target()?;
            let mut ops = vec![PlannedOp::SetOffload {
                id: id.clone(),
                layers: need_u32("offload_layers")?,
            }];
            if let Some(mib) = u32_of(c, "moe_cache_mib") {
                ops.push(PlannedOp::SetMoeCache { id, mib });
            }
            ops
        }
        PlanKind::BackendSetPlacement => {
            let split = c
                .get("tensor_split")
                .filter(|v| v.is_array())
                .cloned()
                .ok_or_else(|| format!("item {} has no tensor_split", item.id))?;
            let mut patch = json!({ "tensor_split": split });
            if let Some(m) = str_of(c, "split_mode") {
                patch["split_mode"] = json!(m);
            }
            vec![PlannedOp::ApplyConfig {
                id: target()?,
                patch,
            }]
        }
        PlanKind::BackendSetModel => vec![PlannedOp::ModelAssign {
            backend_id: target()?,
            model_id: need_str("model_id")?,
        }],
        PlanKind::BackendSetEnabled => {
            let enabled = c
                .get("enabled")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| format!("item {} has no enabled flag", item.id))?;
            vec![PlannedOp::ApplyConfig {
                id: target()?,
                patch: json!({ "enabled": enabled }),
            }]
        }
        PlanKind::GroupSetMembers => vec![PlannedOp::Groups {
            name: target()?,
            edit: GroupEdit::SetMembers(str_vec_of(c, "members")),
        }],
        PlanKind::GroupSetStrategy => vec![PlannedOp::Groups {
            name: target()?,
            edit: GroupEdit::SetStrategy(need_str("strategy")?),
        }],
        PlanKind::GroupSetFallbacks => vec![PlannedOp::Groups {
            name: target()?,
            edit: GroupEdit::SetFallbacks(str_vec_of(c, "fallbacks")),
        }],
        PlanKind::GroupCreate => vec![PlannedOp::Groups {
            name: target()?,
            edit: GroupEdit::Create {
                members: str_vec_of(c, "members"),
                strategy: str_of(c, "strategy").unwrap_or_else(|| "simple-shuffle".into()),
                fallbacks: str_vec_of(c, "fallbacks"),
            },
        }],
        PlanKind::GroupDelete => vec![PlannedOp::Groups {
            name: target()?,
            edit: GroupEdit::Delete,
        }],
        // Registers only (§4); the plan's hash, if any, stays
        // unverified text the operator confirms in the Models view
        // before downloading — it is not passed here.
        PlanKind::ModelAdd => vec![PlannedOp::ModelAdd {
            id: target()?,
            source_url: need_str("source_url")?,
            quant: str_of(c, "quant"),
            params_b: c.get("params_b").and_then(|v| v.as_f64()).map(|p| p as f32),
        }],
        PlanKind::ModelDownload => vec![PlannedOp::ModelDownload { id: target()? }],
        PlanKind::Advice => return Err("advice items do not execute".to_string()),
    })
}

/// Run one planned operation through its `op_*` function.
async fn run_op(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    op: &PlannedOp,
) -> Result<(), String> {
    let e = |err: ApiError| err.to_string();
    match op {
        PlannedOp::BackendAdd {
            id,
            model_id,
            port,
            group,
        } => {
            let cfg = state.config.read().await.clone();
            let model = cfg
                .models
                .iter()
                .find(|m| &m.id == model_id)
                .ok_or_else(|| format!("model '{model_id}' is not registered"))?;
            // No group named: join the group a sibling backend on
            // the same model serves, else a new group named after
            // the backend (the UI orchestration's rule).
            let group = match group {
                Some(g) => g.clone(),
                None => cfg
                    .backends
                    .iter()
                    .find(|b| b.model_file == model.gguf_path)
                    .and_then(|sib| {
                        cfg.gateway
                            .groups
                            .iter()
                            .find(|g| g.members.contains(&sib.id))
                    })
                    .map(|g| g.name.clone())
                    .unwrap_or_else(|| id.clone()),
            };
            let create_group = !cfg.gateway.groups.iter().any(|g| g.name == group);
            crate::op_backend_add(
                state,
                sink,
                id.clone(),
                model_id.clone(),
                *port,
                group,
                create_group,
                true,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .map(|_| ())
            .map_err(e)
        }
        PlannedOp::UpdateFlags { id, n_ctx, n_batch } => {
            let (cur_ctx, cur_batch) = {
                let cfg = state.config.read().await;
                let b = cfg
                    .backends
                    .iter()
                    .find(|b| &b.id == id)
                    .ok_or_else(|| format!("backend '{id}' not found"))?;
                (b.flags.n_ctx, b.flags.n_batch)
            };
            crate::op_backend_update_flags(
                state,
                sink,
                id.clone(),
                n_ctx.unwrap_or(cur_ctx),
                n_batch.unwrap_or(cur_batch),
            )
            .await
            .map(|_| ())
            .map_err(e)
        }
        PlannedOp::SetOffload { id, layers } => {
            crate::op_backend_set_offload(state, sink, id.clone(), *layers, None)
                .await
                .map(|_| ())
                .map_err(e)
        }
        PlannedOp::SetMoeCache { id, mib } => crate::op_backend_set_moe_cache(
            state,
            sink,
            id.clone(),
            if *mib == 0 { None } else { Some(*mib) },
            None,
        )
        .await
        .map(|_| ())
        .map_err(e),
        PlannedOp::ApplyConfig { id, patch } => crate::op_backend_apply_config(
            state,
            sink,
            id.clone(),
            patch.clone(),
            "OnNextRestart".to_string(),
        )
        .await
        .map(|_| ())
        .map_err(e),
        PlannedOp::ModelAssign {
            backend_id,
            model_id,
        } => {
            // Dependency guard (tracker #54): a set_model whose
            // chained model.add was not applied must not run
            // against an unregistered model.
            let split = {
                let cfg = state.config.read().await;
                if !cfg.models.iter().any(|m| &m.id == model_id) {
                    return Err(format!(
                        "model '{model_id}' is not registered — apply its model.add first"
                    ));
                }
                cfg.backends
                    .iter()
                    .find(|b| &b.id == backend_id)
                    .map(|b| b.flags.tensor_split.clone())
                    .unwrap_or_default()
            };
            crate::op_model_assign(state, sink, model_id.clone(), backend_id.clone(), split)
                .await
                .map(|_| ())
                .map_err(e)
        }
        PlannedOp::StopIfRunning { id } => {
            if backend_state(state, sink, id).await?.as_deref() != Some("running") {
                return Ok(());
            }
            crate::op_stop_backend(state, sink, id.clone(), true, Some("optimize".to_string()))
                .await
                .map_err(e)?;
            let deadline = tokio::time::Instant::now() + STOP_DEADLINE;
            loop {
                match backend_state(state, sink, id).await?.as_deref() {
                    None | Some("stopped") => return Ok(()),
                    _ => {}
                }
                if tokio::time::Instant::now() > deadline {
                    return Err(format!(
                        "backend '{id}' did not stop within {} s",
                        STOP_DEADLINE.as_secs()
                    ));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        PlannedOp::BackendDelete { id } => crate::op_backend_delete(state, sink, id.clone())
            .await
            .map(|_| ())
            .map_err(e),
        PlannedOp::Groups { name, edit } => {
            let groups = {
                let cfg = state.config.read().await;
                serde_json::to_value(&cfg.gateway.groups).map_err(|err| err.to_string())?
            };
            let groups = edit_groups(groups, name, edit)?;
            crate::op_update_gateway_config(state, sink, json!({ "groups": groups }))
                .await
                .map(|_| ())
                .map_err(e)
        }
        PlannedOp::ModelAdd {
            id,
            source_url,
            quant,
            params_b,
        } => crate::op_model_add(
            state,
            sink,
            id.clone(),
            source_url.clone(),
            None,
            quant.clone(),
            *params_b,
            None,
            None,
            None,
            None,
        )
        .await
        .map(|_| ())
        .map_err(e),
        PlannedOp::ModelDownload { id } => {
            // An experiment runs its candidate from disk, so the step
            // is done only when the file is on disk and verified.
            let row = model_row(state, sink, id)
                .await?
                .ok_or_else(|| format!("model '{id}' is not registered"))?;
            let flag = |r: &Value, k: &str| r.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
            if flag(&row, "on_disk") && !flag(&row, "downloading") {
                return Ok(());
            }
            if !flag(&row, "downloading") {
                crate::op_download_model(state, sink, id.clone())
                    .await
                    .map_err(e)?;
            }
            let deadline = tokio::time::Instant::now() + DOWNLOAD_DEADLINE;
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let row = model_row(state, sink, id).await?.unwrap_or(Value::Null);
                if let Some(err) = row.get("download_error").and_then(|v| v.as_str()) {
                    return Err(err.to_string());
                }
                if flag(&row, "on_disk") && !flag(&row, "downloading") && !flag(&row, "verifying") {
                    return Ok(());
                }
                if tokio::time::Instant::now() > deadline {
                    return Err(format!(
                        "download of '{id}' did not finish within 2 hours — it continues in the background; watch it in the Models view"
                    ));
                }
            }
        }
    }
}

/// Apply one group edit to the serialized group list.
pub(crate) fn edit_groups(groups: Value, name: &str, edit: &GroupEdit) -> Result<Value, String> {
    let mut list = match groups {
        Value::Array(a) => a,
        _ => return Err("gateway groups are not a list".to_string()),
    };
    let pos = list
        .iter()
        .position(|g| g.get("name").and_then(|n| n.as_str()) == Some(name));
    match edit {
        GroupEdit::Create {
            members,
            strategy,
            fallbacks,
        } => {
            if pos.is_some() {
                return Err(format!("group '{name}' already exists"));
            }
            list.push(json!({
                "name": name,
                "members": members,
                "strategy": strategy,
                "fallbacks": fallbacks,
                "purpose": "",
            }));
        }
        GroupEdit::Delete => {
            let i = pos.ok_or_else(|| format!("group '{name}' not found"))?;
            list.remove(i);
        }
        other => {
            let i = pos.ok_or_else(|| format!("group '{name}' not found"))?;
            let g = &mut list[i];
            match other {
                GroupEdit::SetMembers(m) => g["members"] = json!(m),
                GroupEdit::SetStrategy(s) => g["strategy"] = json!(s),
                GroupEdit::SetFallbacks(f) => g["fallbacks"] = json!(f),
                GroupEdit::Create { .. } | GroupEdit::Delete => unreachable!(),
            }
        }
    }
    Ok(Value::Array(list))
}

/// A backend's process state from `list_backends` (`None` when the
/// backend no longer exists).
async fn backend_state(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    id: &str,
) -> Result<Option<String>, String> {
    let v = crate::op_list_backends(state, sink)
        .await
        .map_err(|e| e.to_string())?;
    Ok(v.get("backends")
        .and_then(|b| b.as_array())
        .and_then(|rows| {
            rows.iter()
                .find(|r| r.get("id").and_then(|i| i.as_str()) == Some(id))
        })
        .and_then(|r| r.get("state").and_then(|s| s.as_str()))
        .map(|s| s.to_string()))
}

/// A model's row from `list_models`.
async fn model_row(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    id: &str,
) -> Result<Option<Value>, String> {
    let v = crate::op_list_models(state, sink)
        .await
        .map_err(|e| e.to_string())?;
    Ok(v.get("models")
        .and_then(|m| m.as_array())
        .and_then(|rows| {
            rows.iter()
                .find(|r| r.get("id").and_then(|i| i.as_str()) == Some(id))
        })
        .cloned())
}

/// Apply the stored plan's accepted items (design §5.2) through the
/// existing operations, or — with `dry_run` — report what would run.
///
/// `destructive` selects the phase: `false` (default) runs the
/// non-destructive accepted items, `true` runs only the accepted
/// deletes (the UI arms this with an explicit confirm). The result
/// is the ordinary state payload plus an `apply_result` object.
pub async fn op_optimize_apply_plan(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    destructive: Option<bool>,
    dry_run: Option<bool>,
) -> ApiResult {
    let destructive = destructive.unwrap_or(false);
    let dry_run = dry_run.unwrap_or(false);
    let live = &state.optimize.live;
    if !live.try_begin_apply() {
        return Err(ApiError::InvalidInput(
            "a plan apply is already running".to_string(),
        ));
    }
    let guard = ApplyGuard(live);
    // A consultation replaces the plan when it finishes; applying a
    // plan that is about to be replaced would record outcomes on
    // the wrong plan.
    if state.optimize.consult_abort.lock().await.is_some() {
        return Err(ApiError::InvalidInput(
            "a consultation is running; apply after it finishes".to_string(),
        ));
    }

    let file = load_plan_file_current(state).await;
    let plan = file
        .plan
        .ok_or_else(|| ApiError::NotFound("no plan".to_string()))?;
    // Re-validate against live state (§5.1): nothing applies
    // against a stale validation.
    let cfg = state.config.read().await.clone();
    let gathered = gather(state, &cfg).await;
    let verdicts = validate_plan(&plan, &cfg, &gathered.live);
    let config_changed = plan.config_hash != current_config_hash(&cfg);

    let mut queue: Vec<(&PlanItem, Vec<PlannedOp>, Value)> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    for (item, verdict) in plan.items.iter().zip(verdicts.iter()) {
        if item.decision != Decision::Accepted || item.kind.is_destructive() != destructive {
            continue;
        }
        if item.apply_outcome == Some(ApplyOutcome::Applied) {
            skipped.push(json!({ "item_id": item.id, "reason": "already applied" }));
            continue;
        }
        if verdict.status == ValidationStatus::Invalid {
            skipped.push(json!({
                "item_id": item.id,
                "reason": verdict.reason.clone().unwrap_or_else(|| "invalid".to_string()),
            }));
            continue;
        }
        match item_operations(item) {
            Ok(ops) => queue.push((item, ops, validation_json(verdict))),
            Err(reason) => skipped.push(json!({ "item_id": item.id, "reason": reason })),
        }
    }

    let steps: Vec<Value> = queue
        .iter()
        .map(|(item, ops, validation)| {
            json!({
                "item_id": item.id,
                "kind": item.kind.as_str(),
                "target": item.target,
                "validation": validation,
                "operations": ops.iter().map(|o| o.describe()).collect::<Vec<_>>(),
            })
        })
        .collect();

    if dry_run {
        opt_log1(
            state,
            &format!(
                "apply dry run ({} phase): {} items would execute, {} skipped",
                if destructive {
                    "destructive"
                } else {
                    "non-destructive"
                },
                steps.len(),
                skipped.len()
            ),
        );
        drop(guard);
        let mut payload = state_payload(state).await?;
        payload["apply_result"] = json!({
            "dry_run": true,
            "destructive": destructive,
            "config_changed": config_changed,
            "steps": steps,
            "skipped": skipped,
        });
        return Ok(payload);
    }

    opt_log1(
        state,
        &format!(
            "apply start ({} phase): {} items queued, {} skipped{}",
            if destructive {
                "destructive"
            } else {
                "non-destructive"
            },
            queue.len(),
            skipped.len(),
            if config_changed {
                "; configuration changed since the plan (re-validated)"
            } else {
                ""
            }
        ),
    );
    let mut applied: Vec<usize> = Vec::new();
    let mut not_attempted: Vec<usize> = Vec::new();
    let mut failed: Option<Value> = None;
    for (item, ops, _) in &queue {
        if failed.is_some() {
            op_optimize_set_item_outcome(state, sink, item.id, "not_attempted".to_string(), None)
                .await?;
            not_attempted.push(item.id);
            continue;
        }
        let mut error: Option<String> = None;
        for op in ops {
            opt_log1(state, &format!("apply item {}: {}", item.id, op.describe()));
            if let Err(err) = run_op(state, sink, op).await {
                error = Some(format!("{}: {err}", op.op_name()));
                break;
            }
        }
        match error {
            None => {
                op_optimize_set_item_outcome(state, sink, item.id, "applied".to_string(), None)
                    .await?;
                applied.push(item.id);
            }
            Some(err) => {
                op_optimize_set_item_outcome(
                    state,
                    sink,
                    item.id,
                    "failed".to_string(),
                    Some(err.clone()),
                )
                .await?;
                failed = Some(json!({ "item_id": item.id, "error": err }));
            }
        }
    }
    opt_log1(
        state,
        &format!(
            "apply end: {} applied, {} failed, {} not attempted; rolled back: no",
            applied.len(),
            usize::from(failed.is_some()),
            not_attempted.len()
        ),
    );
    // Release the claim first: the returned payload's `applying`
    // must describe the state after this apply, not during it.
    drop(guard);
    let mut payload = state_payload(state).await?;
    payload["apply_result"] = json!({
        "dry_run": false,
        "destructive": destructive,
        "config_changed": config_changed,
        "steps": steps,
        "skipped": skipped,
        "applied": applied,
        "failed": failed,
        "not_attempted": not_attempted,
        // §5.2: no rollback — applied items are ordinary saves.
        "rolled_back": false,
    });
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;

    use manager_config::{AppConfig, ModelGroup, RoutingStrategy};
    use manager_optimize::{parse_plan, Confidence};

    use crate::optimize::{load_plan_file, save_plan_file, PlanFile};

    fn unique_dir(prefix: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{n}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// One backend (`tool-runner`) on one registered model, served
    /// by its own group plus a `spare` group — a configuration
    /// that passes whole-config validation.
    fn test_cfg() -> AppConfig {
        let mut cfg = AppConfig::default_config();
        cfg.backends.retain(|b| b.id == "tool-runner");
        cfg.gateway.groups = vec![
            ModelGroup {
                name: "tool-runner".to_string(),
                members: vec!["tool-runner".to_string()],
                strategy: RoutingStrategy::SimpleShuffle,
                fallbacks: Vec::new(),
                purpose: String::new(),
            },
            ModelGroup {
                name: "spare".to_string(),
                members: vec!["tool-runner".to_string()],
                strategy: RoutingStrategy::SimpleShuffle,
                fallbacks: Vec::new(),
                purpose: String::new(),
            },
        ];
        cfg.models.push(manager_config::ModelConfig {
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
        cfg.backends[0].flags.n_ctx = 8192;
        cfg
    }

    fn test_state(cfg: AppConfig) -> (Arc<AppState>, PathBuf) {
        let dir = unique_dir("llm-mgr-optimize-apply");
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = cfg;
        cfg.data_dir = dir.clone();
        let state = Arc::new(AppState::new(cfg, dir.join("config.json"), dir.clone()));
        // Pin an empty telemetry store (no host poller, no GPUs):
        // the same hermetic setup the advanced-flags op tests use.
        let _ = state.telemetry.set(Arc::new(std::sync::Mutex::new(
            manager_telemetry::TelemetryStore::new(),
        )));
        (state, dir)
    }

    fn sink() -> Arc<dyn EventSink> {
        Arc::new(crate::NoopSink)
    }

    /// Store a plan parsed from `items` (as a consultation would),
    /// every non-advice item accepted.
    async fn store_plan(state: &Arc<AppState>, items: Value) {
        let cfg = state.config.read().await.clone();
        let response = json!({ "summary": "test", "items": items }).to_string();
        let mut plan = parse_plan(
            &response,
            &cfg,
            "tool-runner",
            &current_config_hash(&cfg),
            json!({}),
            &HashSet::new(),
            "",
            chrono::Utc::now(),
        )
        .unwrap();
        for item in plan.items.iter_mut() {
            if !item.kind.is_advice() {
                item.decision = Decision::Accepted;
            }
        }
        save_plan_file(
            state,
            &PlanFile {
                plan: Some(plan),
                sample: false,
                denied_fingerprints: Vec::new(),
            },
        )
        .unwrap();
    }

    fn item_json(kind: &str, target: &str, change: Value) -> Value {
        json!({
            "kind": kind, "target": target, "change": change,
            "rationale": "r", "expected_effect": "e", "confidence": "high",
        })
    }

    fn outcome_of(state: &AppState, kind: PlanKind) -> Option<ApplyOutcome> {
        load_plan_file(state)
            .plan
            .unwrap()
            .items
            .iter()
            .find(|i| i.kind == kind)
            .and_then(|i| i.apply_outcome)
    }

    fn two_item_plan() -> Value {
        json!([
            item_json(
                "backend.set_context",
                "tool-runner",
                json!({ "n_ctx": 16384 })
            ),
            item_json(
                "group.set_strategy",
                "tool-runner",
                json!({ "strategy": "least-busy" })
            ),
        ])
    }

    /// Dry run: re-validates and lists each queued item's
    /// operations, and changes nothing — no configuration write,
    /// no outcome recorded, no journal entry.
    #[tokio::test]
    async fn dry_run_lists_operations_and_changes_nothing() {
        let (state, dir) = test_state(test_cfg());
        store_plan(&state, two_item_plan()).await;
        let v = op_optimize_apply_plan(&state, &sink(), None, Some(true))
            .await
            .unwrap();
        // Additive payload fields (Phase 3): no consultation is
        // streaming, and the returned payload describes the state
        // after the call (the apply claim is released first).
        assert_eq!(v["consult_progress"], Value::Null);
        assert_eq!(v["applying"], json!(false));
        let r = &v["apply_result"];
        assert_eq!(r["dry_run"], json!(true));
        let steps = r["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 2, "{r}");
        let ops: Vec<String> = steps
            .iter()
            .flat_map(|s| s["operations"].as_array().unwrap().clone())
            .map(|o| o.as_str().unwrap().to_string())
            .collect();
        assert!(
            ops.iter()
                .any(|o| o.starts_with("backend_update_flags(id=tool-runner, n_ctx=16384")),
            "{ops:?}"
        );
        assert!(
            ops.iter()
                .any(|o| o
                    .starts_with("update_gateway_config(group tool-runner: strategy=least-busy")),
            "{ops:?}"
        );
        let cfg = state.config.read().await.clone();
        assert_eq!(cfg.backends[0].flags.n_ctx, 8192);
        assert_eq!(
            cfg.gateway.groups[0].strategy,
            RoutingStrategy::SimpleShuffle
        );
        assert_eq!(outcome_of(&state, PlanKind::BackendSetContext), None);
        assert!(crate::activation::load_journal(&dir).entries.is_empty());
        assert!(!state.optimize.live.applying(), "claim released");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Apply: every accepted item executes through the existing
    /// ops in plan order; outcomes are recorded and the ops' own
    /// activation-journal entries land.
    #[tokio::test]
    async fn apply_executes_through_the_ops_with_journaling() {
        let (state, dir) = test_state(test_cfg());
        store_plan(&state, two_item_plan()).await;
        let v = op_optimize_apply_plan(&state, &sink(), None, None)
            .await
            .unwrap();
        let r = &v["apply_result"];
        assert_eq!(r["dry_run"], json!(false));
        assert_eq!(r["applied"].as_array().unwrap().len(), 2, "{r}");
        assert_eq!(r["failed"], Value::Null);
        assert_eq!(r["rolled_back"], json!(false));
        let cfg = state.config.read().await.clone();
        assert_eq!(cfg.backends[0].flags.n_ctx, 16384);
        assert_eq!(cfg.gateway.groups[0].strategy, RoutingStrategy::LeastBusy);
        // The untouched group is preserved by the group edit.
        assert_eq!(cfg.gateway.groups[1].name, "spare");
        assert_eq!(
            outcome_of(&state, PlanKind::BackendSetContext),
            Some(ApplyOutcome::Applied)
        );
        assert_eq!(
            outcome_of(&state, PlanKind::GroupSetStrategy),
            Some(ApplyOutcome::Applied)
        );
        let journal = crate::activation::load_journal(&dir);
        assert!(
            !journal.entries.is_empty(),
            "the ops record their journal entries"
        );
        // A second pass finds nothing left to run.
        let v = op_optimize_apply_plan(&state, &sink(), None, None)
            .await
            .unwrap();
        assert!(v["apply_result"]["steps"].as_array().unwrap().is_empty());
        assert_eq!(v["apply_result"]["skipped"].as_array().unwrap().len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Halt on first failure (§5.2): an item whose op refuses (a
    /// backend in advanced flags mode — validation cannot see that,
    /// the op enforces it) fails with the op's reason, and every
    /// later queued item is recorded Not attempted, unexecuted.
    #[tokio::test]
    async fn apply_halts_on_first_failure() {
        let mut cfg = test_cfg();
        cfg.backends[0].flags_advanced = true;
        let (state, dir) = test_state(cfg);
        store_plan(&state, two_item_plan()).await;
        let v = op_optimize_apply_plan(&state, &sink(), None, None)
            .await
            .unwrap();
        let r = &v["apply_result"];
        assert!(r["applied"].as_array().unwrap().is_empty(), "{r}");
        let err = r["failed"]["error"].as_str().unwrap();
        assert!(
            err.contains("backend_update_flags") && err.contains("advanced flags mode"),
            "{err}"
        );
        assert_eq!(r["not_attempted"].as_array().unwrap().len(), 1);
        assert_eq!(
            outcome_of(&state, PlanKind::BackendSetContext),
            Some(ApplyOutcome::Failed)
        );
        assert_eq!(
            outcome_of(&state, PlanKind::GroupSetStrategy),
            Some(ApplyOutcome::NotAttempted)
        );
        let cfg = state.config.read().await.clone();
        assert_eq!(
            cfg.gateway.groups[0].strategy,
            RoutingStrategy::SimpleShuffle
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Destructive items run only in the destructive phase.
    #[tokio::test]
    async fn deletes_run_only_in_the_destructive_phase() {
        let (state, dir) = test_state(test_cfg());
        store_plan(
            &state,
            json!([item_json("group.delete", "spare", json!({}))]),
        )
        .await;
        let v = op_optimize_apply_plan(&state, &sink(), Some(false), None)
            .await
            .unwrap();
        assert!(v["apply_result"]["steps"].as_array().unwrap().is_empty());
        assert_eq!(state.config.read().await.gateway.groups.len(), 2);
        let v = op_optimize_apply_plan(&state, &sink(), Some(true), None)
            .await
            .unwrap();
        assert_eq!(
            v["apply_result"]["applied"].as_array().unwrap().len(),
            1,
            "{}",
            v["apply_result"]
        );
        let cfg = state.config.read().await.clone();
        assert_eq!(cfg.gateway.groups.len(), 1);
        assert_eq!(cfg.gateway.groups[0].name, "tool-runner");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Undecided and denied items never execute.
    #[tokio::test]
    async fn only_accepted_items_execute() {
        let (state, dir) = test_state(test_cfg());
        store_plan(&state, two_item_plan()).await;
        let mut file = load_plan_file(&state);
        for item in file.plan.as_mut().unwrap().items.iter_mut() {
            item.decision = if item.kind == PlanKind::BackendSetContext {
                Decision::Undecided
            } else {
                Decision::Denied
            };
        }
        save_plan_file(&state, &file).unwrap();
        let v = op_optimize_apply_plan(&state, &sink(), None, None)
            .await
            .unwrap();
        assert!(v["apply_result"]["steps"].as_array().unwrap().is_empty());
        assert_eq!(state.config.read().await.backends[0].flags.n_ctx, 8192);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn apply_is_exclusive_and_refused_during_consultation() {
        let (state, dir) = test_state(test_cfg());
        store_plan(&state, two_item_plan()).await;
        assert!(state.optimize.live.try_begin_apply());
        let err = op_optimize_apply_plan(&state, &sink(), None, Some(true))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already running"), "{err}");
        state.optimize.live.end_apply();
        let (tx, _rx) = tokio::sync::oneshot::channel::<()>();
        *state.optimize.consult_abort.lock().await = Some(tx);
        let err = op_optimize_apply_plan(&state, &sink(), None, Some(true))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("consultation is running"), "{err}");
        assert!(
            !state.optimize.live.applying(),
            "a refused apply releases its claim"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The reverse exclusion: a consultation (which replaces the
    /// plan) refuses while an apply records outcomes on it.
    #[tokio::test]
    async fn consultation_refused_during_apply() {
        let (state, dir) = test_state(test_cfg());
        assert!(state.optimize.live.try_begin_apply());
        let err =
            crate::optimize::op_optimize_consult(&state, &sink(), None, Some(true), None, None)
                .await
                .unwrap_err();
        assert!(err.to_string().contains("plan apply is running"), "{err}");
        state.optimize.live.end_apply();
        std::fs::remove_dir_all(&dir).ok();
    }

    fn plan_item(kind: PlanKind, target: &str, change: Value) -> PlanItem {
        PlanItem {
            id: 1,
            fingerprint: String::new(),
            kind,
            target: Some(target.to_string()),
            change,
            advice_text: None,
            rationale: String::new(),
            expected_effect: String::new(),
            confidence: Confidence::High,
            decision: Decision::Accepted,
            previously_denied: false,
            provision_for_experiment: None,
            apply_outcome: None,
            apply_error: None,
        }
    }

    /// The §4 mapping, kind by kind, is the UI orchestration's
    /// composition.
    #[test]
    fn item_operations_follow_the_mapping_table() {
        let ops = item_operations(&plan_item(
            PlanKind::BackendCreate,
            "new",
            json!({
                "model_id": "m", "port": 8090, "n_ctx": 4096,
                "offload_layers": 4, "moe_cache_mib": 0, "tensor_split": [0.5, 0.5],
            }),
        ))
        .unwrap();
        let names: Vec<&str> = ops.iter().map(|o| o.op_name()).collect();
        assert_eq!(
            names,
            vec![
                "backend_add",
                "backend_update_flags",
                "backend_set_offload",
                "backend_set_moe_cache",
                "backend_apply_config",
            ]
        );
        assert_eq!(
            ops[1],
            PlannedOp::UpdateFlags {
                id: "new".into(),
                n_ctx: Some(4096),
                n_batch: None
            }
        );
        assert_eq!(
            ops[4],
            PlannedOp::ApplyConfig {
                id: "new".into(),
                patch: json!({ "tensor_split": [0.5, 0.5], "split_mode": "layer" }),
            }
        );

        let ops = item_operations(&plan_item(PlanKind::BackendDelete, "old", json!({}))).unwrap();
        assert_eq!(
            ops.iter().map(|o| o.op_name()).collect::<Vec<_>>(),
            vec!["stop_backend", "backend_delete"]
        );

        let ops = item_operations(&plan_item(
            PlanKind::BackendSetPlacement,
            "b",
            json!({ "tensor_split": [1.0], "split_mode": "row" }),
        ))
        .unwrap();
        assert_eq!(
            ops,
            vec![PlannedOp::ApplyConfig {
                id: "b".into(),
                patch: json!({ "tensor_split": [1.0], "split_mode": "row" }),
            }]
        );

        let ops = item_operations(&plan_item(
            PlanKind::BackendSetEnabled,
            "b",
            json!({ "enabled": false }),
        ))
        .unwrap();
        assert_eq!(ops[0].op_name(), "backend_apply_config");

        let ops = item_operations(&plan_item(
            PlanKind::ModelAdd,
            "m",
            json!({ "source_url": "https://x/m.gguf", "sha256": "ab", "params_b": 7.0 }),
        ))
        .unwrap();
        assert_eq!(
            ops,
            vec![PlannedOp::ModelAdd {
                id: "m".into(),
                source_url: "https://x/m.gguf".into(),
                quant: None,
                params_b: Some(7.0),
            }],
            "the unverified hash is not passed to model_add"
        );

        assert!(item_operations(&plan_item(PlanKind::Advice, "x", json!({}))).is_err());
        assert!(item_operations(&plan_item(PlanKind::BackendSetFlags, "x", json!({}))).is_err());
    }

    #[test]
    fn edit_groups_preserves_other_fields_and_groups() {
        let groups = json!([
            { "name": "a", "members": ["x"], "strategy": "simple-shuffle", "fallbacks": [], "purpose": "keep" },
            { "name": "b", "members": ["y"], "strategy": "simple-shuffle", "fallbacks": [], "purpose": "" },
        ]);
        let out = edit_groups(
            groups.clone(),
            "a",
            &GroupEdit::SetMembers(vec!["z".into()]),
        )
        .unwrap();
        assert_eq!(out[0]["members"], json!(["z"]));
        assert_eq!(out[0]["purpose"], json!("keep"));
        assert_eq!(out[1], groups[1]);
        let out = edit_groups(groups.clone(), "b", &GroupEdit::Delete).unwrap();
        assert_eq!(out.as_array().unwrap().len(), 1);
        let new_group = GroupEdit::Create {
            members: vec![],
            strategy: "least-busy".into(),
            fallbacks: vec![],
        };
        let out = edit_groups(groups.clone(), "c", &new_group).unwrap();
        assert_eq!(out[2]["name"], json!("c"));
        assert!(
            edit_groups(groups.clone(), "a", &new_group).is_err(),
            "create refuses an existing name"
        );
        assert!(edit_groups(groups, "zz", &GroupEdit::SetStrategy("least-busy".into())).is_err());
    }
}
