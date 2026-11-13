//! Candidate funnel, measurement fingerprints, and context gathering
//! for a consult.

use super::*;

// ---------------------------------------------------------------------------
// Discovery persistence (<data_dir>/optimize-candidates.json)
// ---------------------------------------------------------------------------

/// The stored discovery pre-pass (Phase 5c, §A.5): the candidate
/// facts from one `optimize_discover` run, the directive text
/// they were gathered for, and when they were retrieved. A
/// consultation only receives candidates whose stored directive
/// matches its own (see `optimize_consult`) — facts gathered
/// for a different question are not evidence for this one.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct CandidatesFile {
    #[serde(default)]
    pub(super) directive: String,
    #[serde(default)]
    pub(super) retrieved_at: String,
    #[serde(default)]
    pub(super) candidates: Vec<CandidateFact>,
    /// The deep pass's funnel (deep-discovery design Part 1,
    /// §P1.10): absent on files written before the deep pass
    /// existed — old files load unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) funnel: Option<DiscoveryFunnel>,
}

pub(super) fn candidates_path(state: &AppState) -> PathBuf {
    state.data_dir.join("optimize-candidates.json")
}

pub(super) fn load_candidates_file(state: &AppState) -> CandidatesFile {
    std::fs::read_to_string(candidates_path(state))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub(super) fn save_candidates_file(state: &AppState, file: &CandidatesFile) -> Result<(), String> {
    let text = serde_json::to_string_pretty(file).map_err(|e| e.to_string())?;
    std::fs::write(candidates_path(state), text).map_err(|e| e.to_string())
}

/// The payload form of the stored candidates: the list with the
/// directive it belongs to, its retrieval time, and the
/// pass's funnel (§P1.10), or `null` when no discovery has
/// run (or a pre-deep-pass run found nothing). A deep pass
/// that kept nothing still reports — its funnel, partial
/// marker included, is the result.
pub(super) fn candidates_json(file: &CandidatesFile) -> Value {
    if file.candidates.is_empty() && file.funnel.is_none() {
        Value::Null
    } else {
        json!({
            "directive": file.directive,
            "retrieved_at": file.retrieved_at,
            "items": file.candidates,
            "funnel": file.funnel,
        })
    }
}

/// One-line funnel summary for the optimize.log (§P1.10):
/// the log is how a run gets reviewed after the fact.
pub(super) fn funnel_log_line(funnel: &DiscoveryFunnel) -> String {
    format!(
        "discovery funnel: queries_run={} pool_repos={} repos_fetched={} repos_failed={} dropped_unpopular={} dropped_stale={} dropped_family={} dropped_fit={} quants_considered={} quants_collapsed={} kept={} partial={} partial_reason={} stopped_early={}",
        funnel.queries_run,
        funnel.pool_repos,
        funnel.repos_fetched,
        funnel.repos_failed,
        funnel.dropped_unpopular,
        funnel.dropped_stale,
        funnel.dropped_family,
        funnel.dropped_fit,
        funnel.quants_considered,
        funnel.quants_collapsed,
        funnel.kept,
        funnel.partial,
        funnel.partial_reason.as_deref().unwrap_or("none"),
        funnel.stopped_early,
    )
}

/// The measurement-fingerprint inputs for one backend (§A.1),
/// resolved from the live configuration: the model's verified
/// stamp or on-disk size, the resolved offload, and the GPU set
/// the backend is placed on (indices + names from the telemetry
/// snapshot). An undetermined placement fingerprints against
/// the whole GPU set — the launch is unrestricted then, so any
/// GPU could be doing the work.
pub(crate) fn fingerprint_inputs(
    cfg: &AppConfig,
    b: &manager_config::BackendConfig,
    gpus: &[(u32, String)],
) -> MeasurementFingerprintInputs {
    let model = crate::model_for_backend(cfg, b);
    let file_size_bytes = model
        .map(|m| crate::resolve_under_data_dir(&cfg.data_dir, &m.gguf_path))
        .or_else(|| Some(crate::resolve_under_data_dir(&cfg.data_dir, &b.model_file)))
        .and_then(|p| std::fs::metadata(p).ok())
        .filter(|m| m.is_file())
        .map(|m| m.len());
    let gpu_set: Vec<(u32, String)> = match crate::backend_gpu_visibility(b) {
        crate::GpuVisibility::CpuOnly => Vec::new(),
        crate::GpuVisibility::Gpus(indices) => indices
            .iter()
            .map(|i| {
                (
                    *i,
                    gpus.iter()
                        .find(|(idx, _)| idx == i)
                        .map(|(_, name)| name.clone())
                        .unwrap_or_default(),
                )
            })
            .collect(),
        crate::GpuVisibility::Undetermined => gpus.to_vec(),
    };
    // Resolved offload: the offload info's resolved layer count
    // when the backend's model is offload-capable, else the raw
    // flags' own resolution.
    let offload = match crate::backend_offload_info(cfg, b) {
        Some(info) => info.offload_for(info.layers),
        None => manager_models::expert_offload(b.raw_flags.as_deref()),
    };
    MeasurementFingerprintInputs {
        verified_sha256: model.and_then(|m| m.verified_sha256.clone()),
        file_size_bytes,
        quant: model.map(|m| m.quant.clone()).unwrap_or_default(),
        n_ctx: b.flags.n_ctx,
        n_batch: b.flags.n_batch,
        offload,
        tensor_split: b.flags.tensor_split.clone(),
        split_mode: b.flags.split_mode,
        raw_flags: b.raw_flags.clone(),
        gpus: gpu_set,
    }
}

/// The hash-or-size identity a v2 measurement entry records
/// (design §4.2): the verified SHA-256 (trimmed, lowercased —
/// the form the fingerprint hashes) when the fingerprint used
/// it, else the file size. Exactly one of the two is `Some`:
/// the identity the fingerprint actually used.
pub(crate) fn entry_identity(
    inputs: &MeasurementFingerprintInputs,
) -> (Option<String>, Option<u64>) {
    match inputs
        .verified_sha256
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(sha) => (Some(sha.to_lowercase()), None),
        None => (None, inputs.file_size_bytes),
    }
}

/// The driver versions of an input set's GPU set (design
/// §4.2), deduplicated, in GPU-index order.
pub(crate) fn driver_versions_for(
    inputs: &MeasurementFingerprintInputs,
    drivers: &HashMap<u32, String>,
) -> Vec<String> {
    let mut indices: Vec<u32> = inputs.gpus.iter().map(|(i, _)| *i).collect();
    indices.sort_unstable();
    let mut out: Vec<String> = Vec::new();
    for i in indices {
        if let Some(v) = drivers.get(&i) {
            if !v.is_empty() && !out.contains(v) {
                out.push(v.clone());
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Input gathering: everything the pure core needs from the live app
// ---------------------------------------------------------------------------

pub(crate) struct Gathered {
    pub(crate) measure: MeasureInputs,
    pub(crate) live: LiveSnapshot,
    pub(super) candidates: Vec<ConsultCandidate>,
    pub(crate) running: HashMap<String, bool>,
    /// Each configured backend's current measurement
    /// fingerprint (§A.1), computed from the same snapshot the
    /// context is built from.
    pub(crate) fingerprints: HashMap<String, String>,
    /// Each configured backend's resolved fingerprint inputs —
    /// the readable snapshot a v2 measurement entry records
    /// (design §4.3), from the same resolution that produced
    /// `fingerprints`.
    pub(crate) input_sets: HashMap<String, MeasurementFingerprintInputs>,
    /// GPU driver version by GPU index, from the telemetry
    /// inventory — the environment a new measurement records.
    pub(crate) gpu_driver_versions: HashMap<u32, String>,
}

pub(crate) async fn gather(state: &AppState, cfg: &AppConfig) -> Gathered {
    // Telemetry snapshot (GPU inventory, per-process VRAM, peaks).
    let store = state.telemetry().await;
    let (samples, processes, temp_peaks_c, gpu_driver_versions) = {
        let guard = crate::lock_telemetry(&store);
        let samples = guard.snapshot();
        let processes = guard.last_processes().to_vec();
        let peaks: Vec<(u32, f32)> = samples
            .iter()
            .filter_map(|s| {
                guard
                    .peaks(s.index)
                    .and_then(|p| p.temp_c)
                    .map(|m| (s.index, m.value))
            })
            .collect();
        let drivers: HashMap<u32, String> = guard
            .inventory()
            .iter()
            .map(|g| (g.index, g.driver_version.clone()))
            .collect();
        (samples, processes, peaks, drivers)
    };
    // Supervisor statuses (reap first so stale pids do not
    // attribute VRAM to recycled processes).
    let statuses = {
        let mut sup = state.supervisor().await.lock().await;
        sup.reap_exited();
        sup.status()
    };
    let pid_to_backend: HashMap<u32, String> = statuses
        .iter()
        .filter_map(|s| s.pid.map(|pid| (pid, s.id.clone())))
        .collect();
    let running: HashMap<String, bool> = statuses
        .iter()
        .map(|s| (s.id.clone(), s.state == ProcessState::Running))
        .collect();

    let probe = probe_hardware();
    let mut measure = MeasureInputs {
        hardware: HardwareContext {
            gpus: samples
                .iter()
                .map(|s| manager_optimize::context::GpuContext {
                    index: s.index,
                    name: s.name.clone(),
                    vram_total_mib: s.vram_total_mib,
                    vram_used_mib: s.vram_used_mib,
                    driver_model: s.driver_model.map(|d| format!("{d:?}").to_lowercase()),
                })
                .collect(),
            ram_total_mib: probe.ram_total_mib,
            cpu_model: probe.cpu_model.clone(),
            cpu_cores: probe.cpu_cores,
        },
        temp_peaks_c,
        ..Default::default()
    };
    for b in &cfg.backends {
        if let Some(req) = crate::backend_memory_requirement(cfg, b) {
            measure.requirements.insert(b.id.clone(), req);
        }
    }
    for s in &statuses {
        let mut vram: Vec<(u32, Option<u64>)> = processes
            .iter()
            .filter(|p| pid_to_backend.get(&p.pid) == Some(&s.id))
            .map(|p| (p.gpu_index, p.vram_mib))
            .collect();
        vram.sort_by_key(|(gpu, _)| *gpu);
        measure.backend_states.insert(
            s.id.clone(),
            BackendMeasured {
                state: serde_json::to_value(s.state)
                    .ok()
                    .and_then(|v| v.as_str().map(|x| x.to_string()))
                    .unwrap_or_else(|| "stopped".to_string()),
                uptime_secs: s.uptime.map(|u| u.as_secs()),
                process_vram: vram,
            },
        );
    }
    for m in &cfg.models {
        let path = crate::resolve_under_data_dir(&cfg.data_dir, &m.gguf_path);
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.is_file() {
                measure.downloaded_models.insert(m.id.clone());
                measure.model_sizes.insert(m.id.clone(), meta.len());
            }
        }
    }
    // Gateway traffic window (same store list_backends reads).
    let pairs: Vec<(String, u16)> = cfg
        .backends
        .iter()
        .map(|b| (b.id.clone(), b.port))
        .collect();
    let windows = state.gateway_stats.lock().await.window_stats(&pairs);
    let mut errors_1h: HashMap<String, u64> = HashMap::new();
    if let Some(windows) = &windows {
        let rows: Vec<(String, u64, u64, u64)> = cfg
            .backends
            .iter()
            .filter_map(|b| {
                windows.get(&b.id).map(|w| {
                    errors_1h.insert(b.id.clone(), w.errors);
                    (b.id.clone(), w.requests, w.errors, w.tokens)
                })
            })
            .collect();
        if !rows.is_empty() {
            let window_secs = windows.values().map(|w| w.window_secs).fold(0.0, f64::max);
            measure.traffic = Some(TrafficWindow {
                window_secs,
                per_backend: rows,
            });
        }
    }

    // Measured throughput (§A.1): every configured backend's
    // current fingerprint, then the store's facts against them —
    // current entries plus superseded ones labeled as such.
    let gpu_pairs: Vec<(u32, String)> = samples.iter().map(|s| (s.index, s.name.clone())).collect();
    let mut fingerprints: HashMap<String, String> = HashMap::new();
    let mut input_sets: HashMap<String, MeasurementFingerprintInputs> = HashMap::new();
    for b in &cfg.backends {
        let inputs = fingerprint_inputs(cfg, b, &gpu_pairs);
        fingerprints.insert(b.id.clone(), measurement_fingerprint(&inputs));
        input_sets.insert(b.id.clone(), inputs);
    }
    let store = MeasurementStore::load(&measurements_path(state));
    measure.throughput = measurement_facts(&store, &fingerprints, chrono::Utc::now());

    // Experiment results (§A.3 iteration): the journal's latest
    // batch, as context facts for a follow-up consultation.
    let journal = manager_optimize::ExperimentJournal::load(
        &state.data_dir.join("optimize-experiments.json"),
    );
    measure.experiment_results = journal
        .latest_batch()
        .into_iter()
        .map(manager_optimize::experiment_result_fact)
        .collect();

    // Layout facts per model: from serving backends' resolved
    // layouts; catalog-only facts for models no backend serves.
    let mut layouts: HashMap<String, LayoutFacts> = HashMap::new();
    for b in &cfg.backends {
        if let (Some(model), Some(l)) = (
            crate::model_for_backend(cfg, b),
            crate::resolve_weight_layout(cfg, b),
        ) {
            layouts.insert(
                model.id.clone(),
                LayoutFacts {
                    total_bytes: l.total_bytes,
                    expert_bytes: l.expert_bytes,
                    expert_bytes_by_block: l.expert_bytes_by_block.clone(),
                    moe_layers: l.moe_layers,
                    kv_per_token: l.kv_per_token,
                    kv_geometry: l.kv_geometry,
                    rs_bytes_per_seq: l.rs_bytes_per_seq,
                    basis: l.basis,
                    ctx_max: l.catalog.as_ref().map(|c| c.ctx_max).unwrap_or(0),
                    params_b: model.params_b,
                    // The parsed layout's MoE block map: the plan
                    // model converts MoE layers to `--n-cpu-moe`
                    // bounds through it (review item 11).
                    moe_block_map: l.moe_block_map(),
                },
            );
        }
    }
    // The consult context reports offload in MoE layers through the
    // same block maps (review item 11).
    measure.moe_block_maps = layouts
        .iter()
        .filter_map(|(id, f)| f.moe_block_map.clone().map(|m| (id.clone(), m)))
        .collect();
    for spec in manager_models::curated_models() {
        layouts
            .entry(spec.id.clone())
            .or_insert_with(|| LayoutFacts {
                total_bytes: spec.size_bytes,
                expert_bytes: spec.expert_bytes,
                expert_bytes_by_block: None,
                moe_layers: spec.moe_layers,
                kv_per_token: spec.kv_bytes_per_token,
                kv_geometry: None,
                rs_bytes_per_seq: None,
                basis: manager_models::MemoryBasis::Catalog,
                ctx_max: spec.ctx_max,
                params_b: spec.params_b,
                moe_block_map: None,
            });
    }
    let mut free: Vec<(u32, u64)> = samples
        .iter()
        .map(|s| (s.index, s.vram_total_mib.saturating_sub(s.vram_used_mib)))
        .collect();
    free.sort_by_key(|(i, _)| *i);
    let live = LiveSnapshot {
        layouts: layouts.clone(),
        free_vram_mib_per_gpu: free.into_iter().map(|(_, f)| f).collect(),
        backend_running: running.clone(),
    };

    let candidates: Vec<ConsultCandidate> = cfg
        .backends
        .iter()
        .map(|b| {
            let model = crate::model_for_backend(cfg, b);
            let ctx_rank = model
                .and_then(|m| layouts.get(&m.id))
                .map(|f| f.ctx_max)
                .filter(|c| *c > 0)
                .map(|c| c as u64)
                .unwrap_or(b.flags.n_ctx as u64);
            ConsultCandidate {
                id: b.id.clone(),
                running: running.get(&b.id).copied().unwrap_or(false),
                ctx_rank,
                params_b: model.map(|m| m.params_b).unwrap_or(0.0),
                errors_1h: errors_1h.get(&b.id).copied().unwrap_or(0),
            }
        })
        .collect();

    Gathered {
        measure,
        live,
        candidates,
        running,
        fingerprints,
        input_sets,
        gpu_driver_versions,
    }
}

pub(crate) fn current_config_hash(cfg: &AppConfig) -> String {
    let bytes = serde_json::to_vec(cfg).unwrap_or_default();
    config_fingerprint(&bytes)
}
