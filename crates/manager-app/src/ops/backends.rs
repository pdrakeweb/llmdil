//! Backend listing and start / stop / restart / test operations, plus
//! orphan adoption.

use crate::*;

/// Adopt llama-server processes already listening on a configured
/// backend's port that this session does not supervise. Backend
/// processes are detached and survive the app closing, so after a
/// restart the previous session's llama-servers still hold their
/// ports and GPU memory while the new supervisor reports every
/// backend stopped — and every pid-keyed stat (VRAM attribution,
/// process health) reads empty. Adoption is conservative: the port
/// owner must be a live `llama-server` image, and a backend that is
/// already supervised is never touched.
///
/// Never adopted (SCM-3): a backend whose Windows service owns it (the
/// service is registered and live, or registered in service mode — its
/// llama-server is the service host's child, and adopting it would turn
/// an SCM stop into a phantom crash), and a backend with a lifecycle
/// operation in flight (its old process may still be draining on the
/// port). Each adoption writes the backend's "adopted" marker and
/// records intent Running and readiness (an adopted process is already
/// serving). Returns the adoptions as `(id, pid, port)` so callers that
/// hold an event sink can emit them ([`emit_adopted`]). Reaping exits
/// is the monitor's job, not this function's.
pub(crate) async fn adopt_orphan_backends(state: &AppState) -> Vec<(String, u32, u16)> {
    ensure_backend_specs(state).await;
    let (ports, service_mode): (Vec<(String, u16)>, bool) = {
        let cfg = state.config.read().await;
        (
            cfg.backends
                .iter()
                .map(|b| (b.id.clone(), b.port))
                .collect(),
            cfg.supervision.mode == manager_config::SupervisionMode::Service,
        )
    };
    let live: std::collections::HashSet<String> = {
        let sup = state.supervisor().await.lock().await;
        sup.status()
            .into_iter()
            .filter(|s| s.pid.is_some())
            .map(|s| s.id)
            .collect()
    };
    let scm = lifecycle::scm_snapshot(state, Duration::from_secs(5)).await;
    let mut candidates: Vec<(String, u32, u16)> = Vec::new();
    for (id, port) in ports {
        if live.contains(&id) {
            continue;
        }
        let key = lifecycle::backend_key(&id);
        if state.lifecycle.transition(&key).is_some() {
            continue;
        }
        if let Some(s) = scm.get(&manager_services::backend_service_name(&id)) {
            if lifecycle::registered(s) && (service_mode || lifecycle::scm_live(s)) {
                continue;
            }
        }
        if let Some(pid) = manager_supervisor::port_owner_pid(port) {
            if manager_supervisor::process_image_is(pid, "llama-server.exe") {
                candidates.push((id, pid, port));
            }
        }
    }
    if candidates.is_empty() {
        return Vec::new();
    }
    let mut adopted = Vec::new();
    {
        let mut sup = state.supervisor().await.lock().await;
        for (id, pid, port) in candidates {
            if sup.adopt(&id, pid).is_ok() {
                adopted.push((id, pid, port));
            }
        }
    }
    for (id, pid, port) in &adopted {
        let key = lifecycle::backend_key(id);
        state.lifecycle.set_intent(&key, lifecycle::Intent::Running);
        state.lifecycle.set_ready(&key, *pid);
        lifecycle::mark(
            &state.data_dir,
            &key,
            "adopted",
            Some(&format!(
                "pid {pid} on port {port} (started by a previous session)"
            )),
            Some(lifecycle::Origin::Startup),
        );
    }
    adopted
}

/// Emit the `adopted` lifecycle event for each adoption
/// [`adopt_orphan_backends`] made.
pub(crate) fn emit_adopted(sink: &dyn EventSink, adopted: &[(String, u32, u16)]) {
    for (id, pid, port) in adopted {
        lifecycle::emit(
            sink,
            &lifecycle::backend_key(id),
            "adopted",
            lifecycle::LifecycleState::Running,
            Some(lifecycle::Origin::Startup),
            Some(&format!("pid {pid} on port {port}")),
        );
    }
}

/// Wire string of a supervisor state: the shared lifecycle vocabulary's
/// spelling (`LifecycleState::as_str`). Rows read their state from the
/// liveness view now; kept for the vocabulary test.
#[cfg(test)]
pub(crate) fn process_state_str(s: ProcessState) -> &'static str {
    lifecycle::LifecycleState::from(s).as_str()
}

/// All backends with their launch ports, models, and live supervisor
/// runtime state (process state, pid, restarts, uptime). Configured
/// backends carry `configured: true`; the list also appends
/// supervisor-only rows (`configured: false`, `enabled: false`) for
/// backend processes still live under ids that left the config, so they
/// stay visible and stoppable instead of silently holding their port.
/// MCP server processes share the Supervisor but are never listed here.
pub async fn op_list_backends(state: &Arc<AppState>, sink: &Arc<dyn EventSink>) -> ApiResult {
    let adopted = adopt_orphan_backends(state).await;
    emit_adopted(sink.as_ref(), &adopted);
    // Clone and drop the config guard here: the offload guardrail
    // block below needs the telemetry inventory and the activation
    // journal, and the telemetry accessor reads the config while
    // initializing — it must never be called while this command
    // holds the config guard (the rule ensure_backend_specs follows).
    let cfg = state.config.read().await.clone();
    // Every row's lifecycle fields come from the one shared liveness
    // view (supervisor + SCM + in-flight transitions + readiness), so
    // this list, the dashboard, the activation plan and bench/optimize
    // agree in both run modes. Reaping exits is the monitor's job.
    let liveness = lifecycle::backend_liveness(state).await;
    // Orphan (removed-but-live) rows need their spawn spec's port and
    // model: one short supervisor block, released before any other
    // await below (the telemetry, direct-slots and gateway-stats locks
    // are never taken under the supervisor lock here).
    let configured_ids: std::collections::HashSet<&str> =
        cfg.backends.iter().map(|b| b.id.as_str()).collect();
    let orphan_specs: HashMap<String, (u16, String)> = {
        let sup = state.supervisor().await.lock().await;
        liveness
            .keys()
            .filter(|id| !configured_ids.contains(id.as_str()))
            .filter_map(|id| {
                let spec = sup.spec(id)?;
                let model_file = spec
                    .args
                    .iter()
                    .position(|a| a == "-m")
                    .and_then(|i| spec.args.get(i + 1).cloned())
                    .unwrap_or_default();
                Some((id.clone(), (spec.port.unwrap_or(0), model_file)))
            })
            .collect()
    };
    let mut backends: Vec<Value> = cfg
        .backends
        .iter()
        .map(|b| {
            let mut row = json!({
                "id": b.id,
                "enabled": b.enabled,
                "port": b.port,
                "model_file": b.model_file,
                "configured": true,
                "n_ctx": b.flags.n_ctx,
                "n_batch": b.flags.n_batch,
                "tensor_split": b.flags.tensor_split,
                // The saved raw flags and the advanced flags mode
                // (tracker #60): the card renders the string itself
                // in the mode, and the flags panel pre-fills its
                // editor from it.
                "raw_flags": b.raw_flags,
                "flags_advanced": b.flags_advanced,
                "group": cfg.gateway.groups.iter()
                    .find(|g| g.members.iter().any(|m| m == &b.id))
                    .map(|g| g.name.clone()),
                // The persisted tool-call probe (MCP design §7.2):
                // {outcome: verified|unparsed|failed, at_ms} or null
                // when the backend has never been probed — the UI
                // renders null as "not measured", never as a state.
                "tool_call_probe": serde_json::to_value(b.tool_call_probe).unwrap_or(Value::Null),
            });
            apply_liveness(&mut row, liveness.get(&b.id));
            row
        })
        .collect();
    // Memory requirements per configured backend (GPU-resident
    // weights, system-RAM weights under expert offload, KV at the
    // configured context, recurrent-state cache at the effective
    // slot count, and the derivation basis). Absent when
    // the backend's model cannot be resolved.
    {
        let memory: HashMap<String, manager_models::MemoryRequirement> = cfg
            .backends
            .iter()
            .filter_map(|b| backend_memory_requirement(&cfg, b).map(|r| (b.id.clone(), r)))
            .collect();
        for row in backends.iter_mut() {
            let Some(id) = row["id"].as_str() else {
                continue;
            };
            let Some(req) = memory.get(id) else { continue };
            row["gpu_ram_mib"] = json!(req.gpu_weights_mib);
            row["sys_ram_mib"] = json!(req.sys_weights_mib);
            row["kv_ram_mib"] = json!(req.kv_mib);
            row["rs_ram_mib"] = json!(req.rs_mib);
            // The expert-cache term of the requirement (0 when
            // unset): the card itemizes it beside KV/state so its
            // tiles carry the same components the start gate sums.
            row["moe_cache_mib"] = json!(req.moe_cache_mib);
            row["memory_basis"] = serde_json::to_value(req.basis).unwrap_or(Value::Null);
        }
    }
    // Three-state fit per configured backend, from the same
    // requirement model and per-GPU distribution the start gate
    // judges (residency_per_gpu over the telemetry inventory):
    // each placed GPU's required share against its total VRAM and
    // driver model. Absent when placement or inventory cannot
    // judge the backend — no fit claim then, never a default.
    {
        let inventory: Vec<GpuInfo> = {
            let store = state.telemetry().await;
            let inventory = lock_telemetry(&store).inventory();
            inventory
        };
        if !inventory.is_empty() {
            let n = inventory.len();
            for row in backends.iter_mut() {
                let Some(id) = row["id"].as_str().map(str::to_string) else {
                    continue;
                };
                let Some(b) = cfg.backends.iter().find(|b| b.id == id) else {
                    continue;
                };
                let placed: Vec<u32> = match backend_gpu_visibility(b) {
                    GpuVisibility::Gpus(indices) => indices,
                    _ => continue,
                };
                let Some(req) = backend_memory_requirement(&cfg, b) else {
                    continue;
                };
                let per_gpu = manager_optimize::residency_per_gpu(&req, &b.flags.tensor_split, n);
                let mut states: Vec<manager_models::FitState> = Vec::new();
                let mut per: Vec<Value> = Vec::new();
                for &index in &placed {
                    let Some(pos) = inventory.iter().position(|g| g.index == index) else {
                        continue;
                    };
                    let gpu = &inventory[pos];
                    let (st, paged) = manager_models::fit_state(
                        per_gpu[pos],
                        gpu.total_vram_mib,
                        gpu.driver_model == Some(DriverModel::Wddm),
                    );
                    states.push(st);
                    per.push(json!({
                        "gpu": index,
                        "required_mib": per_gpu[pos],
                        "total_mib": gpu.total_vram_mib,
                        "driver_model": gpu.driver_model.map(|d| match d {
                            DriverModel::Wddm => "wddm",
                            DriverModel::Tcc => "tcc",
                        }),
                        "state": st.as_str(),
                        "paged_mib": paged,
                    }));
                }
                if per.is_empty() {
                    continue;
                }
                let worst = manager_models::worst_fit_state(&states);
                let paged_total: u64 = per
                    .iter()
                    .map(|p| p["paged_mib"].as_u64().unwrap_or(0))
                    .sum();
                row["fit"] = json!({
                    "state": worst.as_str(),
                    "paged_mib": paged_total,
                    "per_gpu": per,
                });
            }
        }
    }
    // Expert-offload slider state per configured backend whose
    // model is offload-capable (see backend_offload_info). Absent
    // for every other backend — the Edit Flags form shows the
    // slider only when this is present.
    {
        let offload: HashMap<String, OffloadInfo> = cfg
            .backends
            .iter()
            .filter_map(|b| backend_offload_info(&cfg, b).map(|o| (b.id.clone(), o)))
            .collect();
        // Guardrail evaluations for the same backends. The config
        // guard is NOT held here (cfg is a clone, dropped above), so
        // the telemetry inventory can be fetched directly.
        let gpus: Vec<GpuInfo> = {
            let store = state.telemetry().await;
            let inventory = lock_telemetry(&store).inventory();
            inventory
        };
        let journal = activation::load_journal(&state.data_dir);
        let evaluations: HashMap<String, OffloadEvaluation> = cfg
            .backends
            .iter()
            .filter_map(|b| {
                evaluate_backend_offload(&cfg, b, &gpus, &journal).map(|e| (b.id.clone(), e))
            })
            .collect();
        let probes: std::collections::HashMap<String, OffloadProbe> = state
            .offload_probes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for row in backends.iter_mut() {
            let Some(id) = row["id"].as_str().map(str::to_string) else {
                continue;
            };
            // The probe state attaches to every configured row,
            // offload-capable or not (a probe only ever exists for
            // an offload change, but the field's shape is uniform).
            row["offload_probe"] = match probes.get(&id) {
                Some(p) => json!({
                    "state": p.state,
                    "layers": p.layers,
                    "detail": p.detail,
                }),
                None => Value::Null,
            };
            let Some(o) = offload.get(&id) else { continue };
            row["offload"] = offload_row_json(o, evaluations.get(&id));
        }
    }
    // MTP draft control state per configured backend whose model
    // is MTP-capable (see backend_mtp_info). Absent for every
    // other backend — the flags form shows the control only when
    // this is present.
    {
        let mtp: HashMap<String, MtpInfo> = cfg
            .backends
            .iter()
            .filter_map(|b| backend_mtp_info(&cfg, b).map(|m| (b.id.clone(), m)))
            .collect();
        for row in backends.iter_mut() {
            let Some(id) = row["id"].as_str() else {
                continue;
            };
            let Some(m) = mtp.get(id) else { continue };
            row["mtp"] = json!({
                "enabled": m.enabled,
                "n_max": m.n_max,
                "parallel": m.parallel,
                "custom": m.custom,
            });
        }
    }
    // Expert-cache control state per offload-capable backend
    // (see backend_moe_cache_info): attaches only where the
    // offload slider's own source resolves, so the control
    // appears for precisely the backends that can offload.
    {
        let moe_cache: HashMap<String, MoeCacheInfo> = cfg
            .backends
            .iter()
            .filter_map(|b| backend_moe_cache_info(&cfg, b).map(|m| (b.id.clone(), m)))
            .collect();
        for row in backends.iter_mut() {
            let Some(id) = row["id"].as_str() else {
                continue;
            };
            let Some(m) = moe_cache.get(id) else { continue };
            row["moe_cache"] = json!({
                "mib": m.mib,
                "custom": m.custom,
                "offload_layers": m.offload_layers,
                "moe_layers": m.moe_layers,
            });
        }
    }
    // Parallel slots state per configured backend (tracker #61):
    // the flags form's Parallel slots field renders for every
    // backend, so — unlike the capability-gated offload/MTP
    // states above — this attaches to every configured row.
    {
        let parallel: HashMap<String, ParallelInfo> = cfg
            .backends
            .iter()
            .map(|b| (b.id.clone(), backend_parallel_info(b)))
            .collect();
        for row in backends.iter_mut() {
            let Some(id) = row["id"].as_str() else {
                continue;
            };
            let Some(p) = parallel.get(id) else { continue };
            row["parallel"] = json!({
                "slots": p.slots,
                "custom": p.custom,
            });
        }
    }
    // KV cache / flash-attention state per configured backend:
    // the flags form's cache controls render for every backend,
    // so — like the parallel state — this attaches to every
    // configured row.
    {
        for row in backends.iter_mut() {
            let Some(id) = row["id"].as_str() else {
                continue;
            };
            let Some(b) = cfg.backends.iter().find(|b| b.id == id) else {
                continue;
            };
            row["cache"] = cache_row_json(&backend_cache_info(b));
        }
    }
    // Predicted throughput per configured backend
    // (predicted-throughput design §7): the exact-match
    // measurement for the backend's SAVED configuration,
    // resolved through the same fingerprint path the Measure
    // action uses and the store's fingerprint-only lookup.
    // Absent when no exact measurement exists — never
    // null-as-zero — and independent of run state.
    {
        let gpu_pairs: Vec<(u32, String)> = {
            let store = state.telemetry().await;
            let guard = lock_telemetry(&store);
            guard
                .snapshot()
                .iter()
                .map(|s| (s.index, s.name.clone()))
                .collect()
        };
        let store = manager_optimize::MeasurementStore::load(&optimize::measurements_path(state));
        let predicted: HashMap<String, Value> = cfg
            .backends
            .iter()
            .filter_map(|b| {
                let inputs = optimize::fingerprint_inputs(&cfg, b, &gpu_pairs);
                let fp = manager_optimize::measurement_fingerprint(&inputs);
                store
                    .lookup(&fp)
                    .map(|e| (b.id.clone(), optimize::predicted_json(e)))
            })
            .collect();
        for row in backends.iter_mut() {
            let Some(id) = row["id"].as_str() else {
                continue;
            };
            let Some(p) = predicted.get(id) else { continue };
            row["predicted"] = p.clone();
        }
    }
    // Placement in short form (Groups page, predicted-throughput
    // design §8.2): where the saved configuration loads, derived
    // from config alone by the same visibility resolution the
    // measurement fingerprint uses. Absent when the placement
    // cannot be determined from the config — unknown placement
    // renders as nothing, never as a default (principle 12).
    {
        for row in backends.iter_mut() {
            let Some(id) = row["id"].as_str() else {
                continue;
            };
            let Some(b) = cfg.backends.iter().find(|b| b.id == id) else {
                continue;
            };
            let placement = match backend_gpu_visibility(b) {
                GpuVisibility::CpuOnly => Some("CPU".to_string()),
                GpuVisibility::Gpus(idxs) => Some(format!(
                    "GPU {}",
                    idxs.iter()
                        .map(|i| i.to_string())
                        .collect::<Vec<_>>()
                        .join("+")
                )),
                GpuVisibility::Undetermined => None,
            };
            if let Some(p) = placement {
                row["placement"] = json!(p);
            }
        }
    }
    // Gateway request statistics (LiteLLM Prometheus snapshots from
    // the background poller): attach the trailing-hour figures where
    // the store has a window for the backend, and the LIVE
    // throughput rate where the store measured one. Absent fields
    // render as "—" in the UI — no stats yet is not zero stats.
    // Throughput is the recent-window live rate, NOT the
    // trailing-hour tokens/window_secs the card carried before
    // Oct 22, 2026: that quotient read "Throughput 0 tok/s" while
    // a backend was actively generating (an hour's denominator
    // over seconds of counted tokens, and no counted tokens at
    // all while a request is still in flight — LiteLLM counts a
    // request's tokens only when it completes).
    {
        let pairs: Vec<(String, u16)> = cfg
            .backends
            .iter()
            .map(|b| (b.id.clone(), b.port))
            .collect();
        // Backends an Optimize operation is currently driving
        // DIRECTLY (consultation / measure / experiment traffic
        // goes to the backend's own port, never through the
        // gateway): the gateway sampler cannot see that
        // generation, so its rate for the backend is not a
        // measurement of the backend's current throughput —
        // omit the gateway fallback while the operation holds
        // the backend instead of reporting the gateway's idle
        // figure. The direct /slots rate below is NOT omitted:
        // it polls the backend itself, so it sees direct-drive
        // generation exactly as it sees gateway traffic.
        let direct_ops: std::collections::HashSet<String> = state
            .optimize
            .running_ops
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        // Direct /slots live rates first: the direct store is
        // read (and its lock released) before the gateway store
        // is locked, so this block never holds both locks in
        // the opposite order anywhere else.
        let direct_rates: std::collections::HashMap<String, Option<f64>> = {
            let direct = state.direct_slots.lock().await;
            pairs
                .iter()
                .map(|(id, _)| (id.clone(), direct.live_rate(id)))
                .collect()
        };
        let mut stats = state.gateway_stats.lock().await;
        let windows = stats.window_stats(&pairs);
        let series = stats.series_stats(&pairs);
        let live = stats.live_throughput(&pairs);
        for row in backends.iter_mut() {
            let Some(id) = row["id"].as_str().map(str::to_string) else {
                continue;
            };
            if let Some(w) = windows.as_ref().and_then(|m| m.get(&id)) {
                row["requests_1h"] = json!(w.requests);
                row["errors_1h"] = json!(w.errors);
                // Health is the trailing-hour success share — the
                // one per-backend percentage the app actually
                // measures (the gateway card's ring uses the same
                // derivation). Absent when the window holds no
                // requests: no traffic is no basis for a score.
                if w.requests > 0 {
                    let share = (w.requests - w.errors) as f64 / w.requests as f64;
                    row["health"] = json!((share * 1000.0).round() / 10.0);
                }
            }
            // Tile precedence (Nov 3, 2026 throughput fix):
            // the direct /slots rate when it was measured, the
            // gateway live rate as fallback, absent otherwise.
            // A missed token attribution leaves the field
            // absent — unknown is not zero (design principle
            // 12). During a direct-drive Optimize op the
            // gateway fallback is skipped (the gateway cannot
            // see that traffic), but the direct rate still
            // attaches: it polls the backend itself.
            let live_rate = match direct_rates.get(&id).copied().flatten() {
                Some(rate) => Some(rate),
                None if direct_ops.contains(&id) => None,
                None => live.as_ref().and_then(|m| m.get(&id)).and_then(|l| l.rate),
            };
            if let Some(rate) = live_rate {
                row["throughput_toks"] = json!((rate * 10.0).round() / 10.0);
            }
            if let Some(s) = series.as_ref().and_then(|m| m.get(&id)) {
                // Dashboard sparklines: tok/s for backend rows,
                // requests/min for the gateway row the UI sums
                // these into.
                row["spark"] = json!(s.toks_per_sec);
                row["req_spark"] = json!(s.req_per_min);
            }
        }
    }
    // Processes still live (or mid-operation) under ids that left the
    // config — their specs survive pruning while live: surface them so
    // they stay visible and stoppable instead of silently holding their
    // port.
    let mut orphan_ids: Vec<&String> = liveness
        .keys()
        .filter(|id| !configured_ids.contains(id.as_str()))
        .collect();
    orphan_ids.sort();
    for id in orphan_ids {
        let Some((port, model_file)) = orphan_specs.get(id) else {
            continue;
        };
        // The spec argv carries the data-dir-resolved absolute path;
        // configured rows show the config-relative path. Strip the
        // data-dir prefix so orphan rows display the same form.
        let model_file = match std::path::Path::new(model_file).strip_prefix(&cfg.data_dir) {
            Ok(rel) => rel.to_string_lossy().into_owned(),
            Err(_) => model_file.clone(),
        };
        let mut row = json!({
            "id": id,
            "enabled": false,
            "port": port,
            "model_file": model_file,
            "configured": false,
        });
        apply_liveness(&mut row, liveness.get(id));
        backends.push(row);
    }
    // Leftover backend services (SCM-18): a registered, live
    // `llm-backend-<id>` service whose id left the config keeps running
    // invisibly unless listed. Rows come from the definitions the app
    // wrote under <data_dir>/services/; they are stoppable (the stop
    // route reaches a live registered service in either mode).
    let listed: std::collections::HashSet<String> = backends
        .iter()
        .filter_map(|r| r["id"].as_str().map(str::to_string))
        .collect();
    for (id, port, model_file) in orphan_service_definitions(&state.data_dir, &listed) {
        let name = manager_services::backend_service_name(&id);
        let Some(status) = lifecycle::scm_status(state, &name, Duration::from_secs(1)).await else {
            continue;
        };
        if !lifecycle::registered(&status) || !lifecycle::scm_live(&status) {
            continue;
        }
        let key = lifecycle::backend_key(&id);
        let record = state.lifecycle.record(&key);
        let lifecycle_state = record
            .transition
            .as_ref()
            .map(|t| lifecycle::LifecycleState::from(t.kind))
            .unwrap_or_else(|| status.lifecycle_state());
        backends.push(json!({
            "id": id,
            "enabled": false,
            "port": port,
            "model_file": model_file,
            "configured": false,
            "service_managed": true,
            "state": lifecycle_state.as_str(),
            "lifecycle": lifecycle_state.as_str(),
            "pid": status.pid,
            "child_pid": status.child_pid,
            "adopted": false,
            "restarts": 0,
            "uptime_secs": Value::Null,
            "failure_reason": status.failure_reason(),
            "transition": transition_json(record.transition.as_ref()),
        }));
    }
    Ok(json!({ "backends": backends }))
}

/// The `transition` field of a backend row: `{kind, origin, since}` of
/// the operation in flight, or null.
pub(crate) fn transition_json(t: Option<&lifecycle::Transition>) -> Value {
    match t {
        Some(t) => json!({
            "kind": t.kind,
            "origin": t.origin,
            "since": t.since.to_rfc3339(),
        }),
        None => Value::Null,
    }
}

/// Write a backend row's lifecycle fields from the shared liveness view
/// (`state` and `lifecycle` carry the same wire string). A backend the
/// view does not know reads stopped.
pub(crate) fn apply_liveness(row: &mut Value, live: Option<&lifecycle::BackendLive>) {
    let state = live
        .map(|l| l.state)
        .unwrap_or(lifecycle::LifecycleState::Stopped);
    row["state"] = json!(state.as_str());
    row["lifecycle"] = json!(state.as_str());
    row["pid"] = json!(live.and_then(|l| l.pid));
    row["child_pid"] = json!(live.and_then(|l| l.child_pid));
    row["adopted"] = json!(live.is_some_and(|l| l.adopted));
    row["restarts"] = json!(live.map(|l| l.restarts).unwrap_or(0));
    row["uptime_secs"] = json!(live.and_then(|l| l.uptime_secs));
    // The recorded reason for the last spawn/load failure (e.g. the
    // device-unavailable fail-fast verdict, or the service's exit-code
    // reason), or null. The card's failed state surfaces it verbatim.
    row["failure_reason"] = json!(live.and_then(|l| l.failure_reason.clone()));
    row["service_managed"] = json!(live.is_some_and(|l| l.service_managed));
    row["transition"] = transition_json(live.and_then(|l| l.transition.as_ref()));
}

/// Backend service definitions under `<data_dir>/services/` whose id is
/// not in `listed`: `(id, port, model_file)` from each definition's
/// argv (`--port`, `-m`).
pub(crate) fn orphan_service_definitions(
    data_dir: &std::path::Path,
    listed: &std::collections::HashSet<String>,
) -> Vec<(String, u16, String)> {
    let prefix = manager_services::backend_service_name("");
    let Ok(entries) = std::fs::read_dir(manager_services::services_dir(data_dir)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(id) = stem.strip_prefix(&prefix) else {
            continue;
        };
        if id.is_empty() || listed.contains(id) {
            continue;
        }
        let Ok(def) = manager_services::read_definition(&path) else {
            continue;
        };
        let arg_after = |flag: &str| {
            def.args
                .iter()
                .position(|a| a == flag)
                .and_then(|i| def.args.get(i + 1).cloned())
        };
        let port = arg_after("--port")
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(0);
        let model_file = arg_after("-m").unwrap_or_default();
        out.push((id.to_string(), port, model_file));
    }
    out.sort();
    out
}

/// The origin of an op invocation: the `origin` argument when given
/// (`activation`, `dashboard`, ...), else `ui` (desktop calls; the REST
/// layer passes its own).
pub(crate) fn op_origin(origin: Option<&str>) -> lifecycle::Origin {
    origin
        .map(lifecycle::Origin::parse)
        .unwrap_or(lifecycle::Origin::Ui)
}

/// Start a backend (supervisor spawn, or SCM start in service mode
/// when the backend is registered as a service). Returns a task id.
/// `allow_wddm_overcommit` is the operator's two-click confirmation
/// for a start whose requirement exceeds a WDDM GPU's total VRAM
/// (the excess pages to system RAM); it never overrides a TCC
/// refusal — see [`vram_start_gate`]. A thin wrapper over
/// [`backend_lifecycle::start_backend`].
pub async fn op_start_backend(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    id: String,
    allow_wddm_overcommit: Option<bool>,
    origin: Option<String>,
) -> ApiResult {
    let opts = backend_lifecycle::BackendOpOpts {
        allow_wddm_overcommit: allow_wddm_overcommit == Some(true),
        ..backend_lifecycle::BackendOpOpts::new(op_origin(origin.as_deref()))
    };
    backend_lifecycle::start_backend(state, sink, &id, opts).await
}

/// Stop a backend, gracefully (drain) or immediately. A thin wrapper
/// over [`backend_lifecycle::stop_backend`].
pub async fn op_stop_backend(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    id: String,
    graceful: bool,
    origin: Option<String>,
) -> ApiResult {
    let opts = backend_lifecycle::BackendOpOpts {
        graceful,
        ..backend_lifecycle::BackendOpOpts::new(op_origin(origin.as_deref()))
    };
    backend_lifecycle::stop_backend(state, sink, &id, opts).await
}

/// Restart a backend: stop + start inside one `restarting` transition
/// ([`backend_lifecycle::restart_backend`]). `wait_ready_secs` makes the
/// call return only once the backend answers `/health` (the activation
/// plan uses it so "applied" means live).
pub async fn op_restart_backend(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    id: String,
    graceful: bool,
    allow_wddm_overcommit: Option<bool>,
    origin: Option<String>,
    wait_ready_secs: Option<u64>,
) -> ApiResult {
    let opts = backend_lifecycle::BackendOpOpts {
        graceful,
        allow_wddm_overcommit: allow_wddm_overcommit == Some(true),
        wait_ready: wait_ready_secs.filter(|s| *s > 0).map(Duration::from_secs),
        ..backend_lifecycle::BackendOpOpts::new(op_origin(origin.as_deref()))
    };
    match backend_lifecycle::restart_backend(state, sink, &id, opts).await {
        Ok(v) => Ok(v),
        Err(e) => {
            // A restart the pending activation plan depends on just
            // failed: that is the plan failing to apply. A restart
            // failure with no plan entry for this backend is a plain
            // command error the UI already shows — no toast. A busy
            // refusal is not a failure of the restart in flight.
            if !matches!(e, ApiError::Busy(_)) {
                let (unresolved, _) = activation_unresolved(state).await;
                if unresolved
                    .iter()
                    .any(|en| en.scope == "backend" && en.targets.iter().any(|t| t == &id))
                {
                    notify::notify(
                        sink.as_ref(),
                        state,
                        notify::Category::Activation,
                        "Activation plan failed",
                        &format!("Backend {id} could not be restarted: {e}"),
                    )
                    .await;
                }
            }
            Err(e)
        }
    }
}

/// One raw prompt against a backend; raw JSON result, no history (spec §1.3).
/// Minimal inference test request against a backend's
/// `/v1/chat/completions` (spec §14 `backend_test_request`).
pub async fn op_test_backend(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    prompt: String,
) -> ApiResult {
    let cfg = state.config.read().await;
    let backend = cfg
        .backends
        .iter()
        .find(|b| b.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
    let base_url = format!("http://127.0.0.1:{}", backend.port);
    let result = test_request(&base_url, &prompt)
        .await
        .map_err(ApiError::from)?;
    Ok(serde_json::to_value(&result)?)
}

/// Tool-call probe against a backend (MCP design §7.2): health gate,
/// `/props` template evidence, then a direct chat completion that
/// must produce a structured tool call. The outcome is persisted on
/// the backend as `tool_call_probe` (runtime state, not operator
/// config — it never appears in a config diff) and the full probe
/// result is returned.
pub async fn op_backend_test_tool_call(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    let base_url = {
        let cfg = state.config.read().await;
        let backend = cfg
            .backends
            .iter()
            .find(|b| b.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("backend {id}")))?;
        format!("http://127.0.0.1:{}", backend.port)
    };
    let result = test_tool_call(&base_url).await.map_err(ApiError::from)?;
    // The supervisor and config crates spell the same three outcomes
    // with separate enums (neither depends on the other); the
    // mapping between them lives here, at the layer that holds both.
    let outcome = match result.outcome {
        manager_supervisor::ToolCallProbeOutcome::Verified => {
            manager_config::ToolCallProbeOutcome::Verified
        }
        manager_supervisor::ToolCallProbeOutcome::Unparsed => {
            manager_config::ToolCallProbeOutcome::Unparsed
        }
        manager_supervisor::ToolCallProbeOutcome::Failed => {
            manager_config::ToolCallProbeOutcome::Failed
        }
    };
    let mut cfg = state.config.write().await;
    mutate_save_install(&mut cfg, &state.config_path, |cfg| {
        if let Some(b) = cfg.backends.iter_mut().find(|b| b.id == id) {
            b.tool_call_probe = Some(manager_config::ToolCallProbe {
                outcome,
                at_ms: now_ms(),
            });
        }
    })
    .map_err(ApiError::from)?;
    drop(cfg);
    Ok(serde_json::to_value(&result)?)
}

#[cfg(test)]
mod tests;
