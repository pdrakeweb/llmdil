//! The consultation context (design §2) and prompts (§3).
//!
//! The context document is assembled field by field from typed
//! sources — an allowlist, never a serialization of `AppConfig` —
//! so secrets are excluded by construction (§2.4): the LiteLLM
//! master key is never read, the OpenRouter credential appears
//! neither as value nor as its Credential Manager target name, and
//! no absolute data-dir path is included (model paths travel in
//! their configured relative form). Backend `raw_flags` never
//! travel verbatim either: the string is unconstrained operator
//! input and can carry secrets (an `--api-key` value, a token
//! flag); only facts decoded from it (offload) are included. The
//! redaction test in this module is a release gate for the
//! feature.

use std::collections::{HashMap, HashSet};

use manager_config::{AppConfig, BackendConfig, ModelConfig};
use manager_models::MemoryRequirement;
use serde_json::{json, Value};

use crate::discover::CandidateFact;

/// One GPU's inventory + latest readings, from the telemetry
/// snapshot (the same data `get_gpu_telemetry` returns).
#[derive(Debug, Clone)]
pub struct GpuContext {
    pub index: u32,
    pub name: String,
    pub vram_total_mib: u64,
    pub vram_used_mib: u64,
    /// "wddm" / "tcc" when the source reports it.
    pub driver_model: Option<String>,
}

/// Hardware inventory for the context (§2.1). RAM total and CPU
/// come from [`crate::probe`]; both are `None` when the probe
/// could not read them — the block then omits them rather than
/// estimating (design §2.1).
#[derive(Debug, Clone, Default)]
pub struct HardwareContext {
    pub gpus: Vec<GpuContext>,
    pub ram_total_mib: Option<u64>,
    pub cpu_model: Option<String>,
    pub cpu_cores: Option<usize>,
}

/// One running backend's measured state (§2.3).
#[derive(Debug, Clone)]
pub struct BackendMeasured {
    /// Supervisor state string ("running", "starting", ...).
    pub state: String,
    pub uptime_secs: Option<u64>,
    /// Per-GPU VRAM attribution: (gpu index, MiB or `None` when
    /// not measured — preserved as `null`, never 0; §2.3).
    pub process_vram: Vec<(u32, Option<u64>)>,
}

/// Trailing-window gateway traffic (§2.3), present only when the
/// stats store has a window.
#[derive(Debug, Clone)]
pub struct TrafficWindow {
    pub window_secs: f64,
    /// (backend id, requests, errors, total tokens).
    pub per_backend: Vec<(String, u64, u64, u64)>,
}

/// One backend configuration's measured throughput (design
/// addendum A.1, tracker #28): speeds actually measured on this
/// machine by the benchmark runner, keyed in the measurement
/// store by config fingerprint. Reserved in the context schema
/// now; the store and runner arrive in Phase 5, so callers pass
/// none until then and the section stays absent.
#[derive(Debug, Clone)]
pub struct ThroughputFact {
    pub backend_id: String,
    /// The measurement store's key: a hash of the backend's
    /// configuration, so a config change invalidates the match.
    pub config_fingerprint: String,
    pub prompt_per_second: Option<f64>,
    pub predicted_per_second: Option<f64>,
    pub ttft_ms: Option<f64>,
    /// Seconds since the measurement was taken.
    pub age_secs: u64,
    /// True when this entry's fingerprint is not the backend's
    /// current configuration fingerprint: a retained older
    /// measurement, surfaced as delta history (§A.1). The
    /// context JSON carries `"superseded": true` only when this
    /// is set, so a current fact's shape is unchanged.
    pub superseded: bool,
}

/// One finished experiment's results, for the follow-up
/// consultation (§A.3 iteration): the candidate, its measured
/// rates (headline composition as in [`ThroughputFact`], plus
/// the short/long generation split the §A.4 table reports), and
/// its screening scores with the delta-vs-baseline summary.
/// Built from the experiment journal's latest batch by the
/// caller; absent (empty) until an experiment has run.
#[derive(Debug, Clone)]
pub struct ExperimentResultFact {
    pub candidate_model_id: String,
    /// "completed" | "failed" | "timeout" | "cancelled".
    pub status: String,
    pub prompt_per_second: Option<f64>,
    pub predicted_per_second: Option<f64>,
    pub short_predicted_per_second: Option<f64>,
    pub long_predicted_per_second: Option<f64>,
    pub ttft_ms: Option<f64>,
    pub screen_overall: Option<crate::eval::Score>,
    pub screen_per_category: std::collections::BTreeMap<String, crate::eval::Score>,
    /// One-line delta-vs-baseline summary (screen movement and
    /// the §A.4 replacement predicates), or `None` when the run
    /// had no baseline to compare against.
    pub delta_vs_baseline: Option<String>,
}

/// Everything measured or computed that the builder cannot derive
/// from the config alone, gathered by the caller (src-tauri).
#[derive(Debug, Clone, Default)]
pub struct MeasureInputs {
    pub hardware: HardwareContext,
    /// Per-backend computed memory requirement under the current
    /// configuration, keyed by backend id (the same figures
    /// `list_backends` shows).
    pub requirements: HashMap<String, MemoryRequirement>,
    /// Measured state for backends the supervisor knows about.
    pub backend_states: HashMap<String, BackendMeasured>,
    /// Model ids whose GGUF file is on disk.
    pub downloaded_models: HashSet<String>,
    /// On-disk sizes by model id, when downloaded.
    pub model_sizes: HashMap<String, u64>,
    pub traffic: Option<TrafficWindow>,
    /// (gpu index, peak temperature C) where the store tracks one.
    pub temp_peaks_c: Vec<(u32, f32)>,
    /// Measured throughput facts whose config fingerprint matches
    /// the backend's current configuration (addendum A.1).
    pub throughput: Vec<ThroughputFact>,
    /// Discovered Hugging Face candidates from the discovery
    /// pre-pass (addendum A.5 option (b), Phase 5c), gathered by
    /// the caller for the consultation's directive and already
    /// pruned by the operator. Empty — and the `candidates`
    /// block absent — for directive-less consultations and
    /// whenever no discovery has run.
    pub candidates: Vec<CandidateFact>,
    /// Finished experiment results from the journal's latest
    /// batch (addendum A.3 iteration, Phase 5e), capped at 3 by
    /// the caller. Empty — and the `experiment_results` block
    /// absent — until an experiment batch has run.
    pub experiment_results: Vec<ExperimentResultFact>,
}

/// Counts and provenance for the page's context summary line and
/// the prompt's measured-state statement.
#[derive(Debug, Clone, Default)]
pub struct ContextMeta {
    pub gpu_count: usize,
    pub backend_count: usize,
    pub group_count: usize,
    pub model_count: usize,
    /// Names of the context blocks that carry measurements rather
    /// than configuration ("running state", "gateway traffic"...).
    pub measured_blocks: Vec<String>,
    /// "1 h" / "37 min" when a traffic window is present.
    pub traffic_window_label: Option<String>,
}

/// Resolve a backend's model: exact configured-path equality,
/// else a unique file-name match (the same resolution
/// `model_for_backend` performs in src-tauri, over the relative
/// forms the config stores).
pub fn resolve_model<'a>(cfg: &'a AppConfig, backend: &BackendConfig) -> Option<&'a ModelConfig> {
    if let Some(m) = cfg
        .models
        .iter()
        .find(|m| m.gguf_path == backend.model_file)
    {
        return Some(m);
    }
    let name = backend.model_file.file_name()?;
    let mut matches = cfg
        .models
        .iter()
        .filter(|m| m.gguf_path.file_name() == Some(name));
    let first = matches.next()?;
    if matches.next().is_some() {
        None
    } else {
        Some(first)
    }
}

/// The model's short display name: the operator's rename, else
/// the curated catalog name, else the id.
pub fn model_short_name(cfg: &AppConfig, model: &ModelConfig) -> String {
    let _ = cfg;
    if let Some(d) = model.display_name.as_deref() {
        if !d.trim().is_empty() {
            return d.to_string();
        }
    }
    manager_models::curated_models()
        .into_iter()
        .find(|s| s.id == model.id)
        .map(|s| s.name)
        .unwrap_or_else(|| model.id.clone())
}

fn offload_json(backend: &BackendConfig, moe_layers: Option<u32>) -> Value {
    let offload = manager_models::expert_offload(backend.raw_flags.as_deref());
    let mut v = match offload {
        manager_models::ExpertOffload::None => json!({ "setting": "none" }),
        manager_models::ExpertOffload::AllExperts => json!({ "setting": "all_experts" }),
        manager_models::ExpertOffload::CpuMoeLayers(n) => {
            json!({ "setting": "cpu_moe_layers", "layers": n })
        }
    };
    if let Some(moe) = moe_layers {
        v["offloaded_layers"] = json!(offload.layers(moe));
        v["moe_layers"] = json!(moe);
    }
    v
}

/// Assemble the context document (§2). Returns the document and
/// its meta (counts + which blocks are measured).
pub fn build_context(
    cfg: &AppConfig,
    measure: &MeasureInputs,
    app_version: &str,
    consulted_backend: &str,
) -> (Value, ContextMeta) {
    let catalog = manager_models::curated_models();
    let mut meta = ContextMeta {
        gpu_count: measure.hardware.gpus.len(),
        backend_count: cfg.backends.len(),
        group_count: cfg.gateway.groups.len(),
        model_count: cfg.models.len(),
        ..Default::default()
    };

    let gpus: Vec<Value> = measure
        .hardware
        .gpus
        .iter()
        .map(|g| {
            let mut v = json!({
                "index": g.index,
                "name": g.name,
                "vram_total_mib": g.vram_total_mib,
                "vram_used_mib": g.vram_used_mib,
            });
            if let Some(dm) = &g.driver_model {
                v["driver_model"] = json!(dm);
            }
            v
        })
        .collect();
    let mut hardware = json!({ "gpus": gpus });
    if let Some(ram) = measure.hardware.ram_total_mib {
        hardware["ram_total_mib"] = json!(ram);
    }
    if let Some(cpu) = &measure.hardware.cpu_model {
        hardware["cpu_model"] = json!(cpu);
    }
    if let Some(cores) = measure.hardware.cpu_cores {
        hardware["cpu_cores"] = json!(cores);
    }

    let backends: Vec<Value> = cfg
        .backends
        .iter()
        .map(|b| {
            let model = resolve_model(cfg, b);
            let mut v = json!({
                "id": b.id,
                "enabled": b.enabled,
                "model_file": b.model_file.to_string_lossy(),
                "port": b.port,
                "n_ctx": b.flags.n_ctx,
                "n_batch": b.flags.n_batch,
                "tensor_split": b.flags.tensor_split,
                "split_mode": serde_json::to_value(b.flags.split_mode).unwrap_or(Value::Null),
            });
            if let Some(m) = model {
                v["model_id"] = json!(m.id);
            }
            // `raw_flags` is deliberately NOT emitted (code
            // review B-1): it is an unconstrained operator string
            // tokenized into the llama-server command line, so a
            // secret-bearing flag (`--api-key <value>`, a token
            // flag, a credential path) would otherwise travel to
            // the consulted backend, persist in the plan file, and
            // return to the UI. Only the decoded, non-secret facts
            // derived from it (the `offload` block below) travel.
            let moe_layers = model.and_then(|m| {
                catalog
                    .iter()
                    .find(|s| s.id == m.id)
                    .and_then(|s| s.moe_layers)
            });
            v["offload"] = offload_json(b, moe_layers);
            if let Some(req) = measure.requirements.get(&b.id) {
                let mut r = json!({
                    "gpu_ram_mib": req.gpu_weights_mib,
                    "sys_ram_mib": req.sys_weights_mib,
                    "basis": serde_json::to_value(req.basis).unwrap_or(Value::Null),
                });
                if let Some(kv) = req.kv_mib {
                    r["kv_ram_mib"] = json!(kv);
                }
                if let Some(rs) = req.rs_mib {
                    r["rs_ram_mib"] = json!(rs);
                }
                v["memory_requirement"] = r;
            }
            if !b.purpose.trim().is_empty() {
                v["purpose"] = json!(b.purpose);
            }
            v
        })
        .collect();

    let groups: Vec<Value> = cfg
        .gateway
        .groups
        .iter()
        .map(|g| {
            let mut v = json!({
                "name": g.name,
                "members": g.members,
                "strategy": serde_json::to_value(g.strategy).unwrap_or(Value::Null),
                "fallbacks": g.fallbacks,
            });
            if !g.purpose.trim().is_empty() {
                v["purpose"] = json!(g.purpose);
            }
            v
        })
        .collect();

    let models: Vec<Value> = cfg
        .models
        .iter()
        .map(|m| {
            let spec = catalog.iter().find(|s| s.id == m.id);
            let mut v = json!({
                "id": m.id,
                "short_name": model_short_name(cfg, m),
                "quant": m.quant,
                "params_b": m.params_b,
                "downloaded": measure.downloaded_models.contains(&m.id),
                "verified": m.verified_at.is_some(),
                "assigned_backends": m.assigned_backends,
            });
            if let Some(size) = measure.model_sizes.get(&m.id) {
                v["size_bytes"] = json!(size);
            } else if let Some(spec) = spec {
                v["size_bytes"] = json!(spec.size_bytes);
            }
            if let Some(spec) = spec {
                if spec.ctx_max > 0 {
                    v["ctx_max"] = json!(spec.ctx_max);
                }
            }
            v
        })
        .collect();

    let catalog_candidates: Vec<Value> = catalog
        .iter()
        .filter(|s| !cfg.models.iter().any(|m| m.id == s.id))
        .map(|s| {
            json!({
                "id": s.id,
                "name": s.name,
                "quant": s.quant,
                "size_bytes": s.size_bytes,
                "ctx_max": s.ctx_max,
                "params_b": s.params_b,
            })
        })
        .collect();

    let mut doc = json!({
        "app": { "version": app_version, "consulted_backend": consulted_backend },
        "hardware": hardware,
        "backends": backends,
        "groups": groups,
        "models": models,
        "catalog_candidates": catalog_candidates,
        "gateway": {
            "port": cfg.gateway.port,
            "bind_host": cfg.gateway.bind_host,
            "group_count": cfg.gateway.groups.len(),
            "openrouter": {
                "enabled": cfg.gateway.openrouter.enabled,
                "daily_cap_usd": cfg.gateway.openrouter.daily_cap_usd,
                "model_allowlist": cfg.gateway.openrouter.model_allowlist,
            },
        },
        "postgres": { "managed": true, "port": cfg.postgres.port },
    });

    // Measured state (§2.3): present-or-absent blocks, never
    // fabricated. Nothing else is measured: no request contents,
    // no served prompts, no log lines.
    let mut measured = json!({});
    if !measure.backend_states.is_empty() {
        let mut rows: Vec<(&String, &BackendMeasured)> =
            measure.backend_states.iter().collect();
        rows.sort_by(|a, b| a.0.cmp(b.0));
        measured["backends"] = Value::Array(
            rows.iter()
                .map(|(id, m)| {
                    let mut v = json!({ "id": id, "state": m.state });
                    if let Some(up) = m.uptime_secs {
                        v["uptime_secs"] = json!(up);
                    }
                    v["process_vram"] = Value::Array(
                        m.process_vram
                            .iter()
                            .map(|(gpu, mib)| json!({ "gpu_index": gpu, "vram_mib": mib }))
                            .collect(),
                    );
                    v
                })
                .collect(),
        );
        meta.measured_blocks
            .push("backend running state and per-process VRAM".to_string());
    }
    if let Some(traffic) = &measure.traffic {
        measured["gateway_traffic"] = json!({
            "window_secs": traffic.window_secs,
            "backends": traffic.per_backend.iter().map(|(id, req, err, tok)| {
                json!({ "id": id, "requests": req, "errors": err, "tokens": tok })
            }).collect::<Vec<_>>(),
        });
        meta.measured_blocks.push("gateway traffic".to_string());
        meta.traffic_window_label = Some(if traffic.window_secs >= 3599.0 {
            "1 h".to_string()
        } else {
            format!("{} min", (traffic.window_secs / 60.0).round() as u64)
        });
    }
    if !measure.temp_peaks_c.is_empty() {
        measured["gpu_temp_peaks_c"] = Value::Array(
            measure
                .temp_peaks_c
                .iter()
                .map(|(index, peak)| json!({ "index": index, "peak_c": peak }))
                .collect(),
        );
        meta.measured_blocks
            .push("GPU temperature peaks".to_string());
    }
    // Measured throughput (addendum A.1): its own top-level
    // section rather than part of `measured`, because entries are
    // keyed facts about a configuration — with provenance ("basis"
    // is "measured", the strongest label in the app's discipline)
    // and age — not a snapshot of current state. Absent until the
    // Phase 5 measurement store produces facts.
    if !measure.throughput.is_empty() {
        doc["measurements"] = json!({
            "throughput": measure.throughput.iter().map(|t| {
                let mut v = json!({
                    "backend": t.backend_id,
                    "config_fingerprint": t.config_fingerprint,
                    "basis": "measured",
                    "age_secs": t.age_secs,
                    "prompt_per_second": t.prompt_per_second,
                    "predicted_per_second": t.predicted_per_second,
                    "ttft_ms": t.ttft_ms,
                });
                if t.superseded {
                    v["superseded"] = json!(true);
                }
                v
            }).collect::<Vec<_>>(),
        });
        meta.measured_blocks
            .push("measured throughput".to_string());
    }
    // Discovered candidates (addendum A.5, Phase 5c): the
    // pre-pass's vetted, fit-checked HF facts, distinct from
    // `catalog_candidates` (curated, local). Provenance rides
    // per entry — the HF repo id and the retrieved-at
    // timestamp. Absent when no discovery ran for this
    // consultation's directive; never fabricated.
    if !measure.candidates.is_empty() {
        doc["candidates"] = Value::Array(
            measure
                .candidates
                .iter()
                .map(|c| {
                    json!({
                        "repo_id": c.repo_id,
                        "file": c.file,
                        "architecture": c.architecture,
                        "params_b": c.params_b,
                        "ctx_max": c.ctx_max,
                        "size_bytes": c.size_bytes,
                        "downloads": c.downloads,
                        "fit": c.fit,
                        "retrieved_at": c.retrieved_at,
                    })
                })
                .collect(),
        );
    }
    // Experiment results (addendum A.3 iteration): the latest
    // batch's measured outcomes, so a follow-up consultation
    // reasons over what actually ran on this machine — it can
    // propose adopting a winner as a real plan item, or the
    // next experiment. Absent until a batch has run.
    if !measure.experiment_results.is_empty() {
        doc["experiment_results"] = Value::Array(
            measure
                .experiment_results
                .iter()
                .map(|r| {
                    let mut v = json!({
                        "candidate_model_id": r.candidate_model_id,
                        "status": r.status,
                        "basis": "measured",
                        "prompt_per_second": r.prompt_per_second,
                        "predicted_per_second": r.predicted_per_second,
                        "generation_per_second": {
                            "short": r.short_predicted_per_second,
                            "long": r.long_predicted_per_second,
                        },
                        "ttft_ms": r.ttft_ms,
                    });
                    if let Some(overall) = &r.screen_overall {
                        v["screen"] = json!({
                            "overall": overall,
                            "per_category": r.screen_per_category,
                        });
                    }
                    if let Some(delta) = &r.delta_vs_baseline {
                        v["delta_vs_baseline"] = json!(delta);
                    }
                    v
                })
                .collect(),
        );
        meta.measured_blocks
            .push("experiment results".to_string());
    }
    if measured
        .as_object()
        .map(|o| !o.is_empty())
        .unwrap_or(false)
    {
        doc["measured"] = measured;
    }

    (doc, meta)
}

/// The consultation prompts (§3). The system prompt states the
/// output contract exactly; the user prompt carries the purpose
/// verbatim at its head, then — when one is present — the
/// operator directive verbatim under its own label (addendum
/// A.2), then which blocks are measured, then the context
/// document in a fenced block. A directive that is empty after
/// trimming produces no block at all.
pub fn build_prompts(
    purpose: &str,
    directive: &str,
    context: &Value,
    meta: &ContextMeta,
) -> (String, String) {
    let system = SYSTEM_PROMPT.to_string();
    let measured_line = if meta.measured_blocks.is_empty() {
        "Measured state: none — everything in the document is configuration, not measurement."
            .to_string()
    } else {
        format!(
            "Measured state in the document: {}. Everything else in the document is configuration, not measurement.",
            meta.measured_blocks.join(", ")
        )
    };
    let directive_block = if directive.trim().is_empty() {
        String::new()
    } else {
        format!("\n\nOperator directive for this consultation:\n{directive}")
    };
    let user = format!(
        "{purpose}{directive_block}\n\n{measured_line}\n\nContext:\n```json\n{}\n```",
        serde_json::to_string_pretty(context).unwrap_or_default()
    );
    (system, user)
}

const SYSTEM_PROMPT: &str = "\
You are the layout consultant for a local LLM service manager. You are shown \
the deployment's purpose and a JSON document describing the machine (GPUs, \
system RAM, CPU), the configured backends (llama-server instances), the \
gateway's model groups, the registered models, catalog models that could be \
added, and measured state (running state, VRAM attribution, gateway traffic) \
where it exists.

Propose optimizations to the layout: which models, which settings per model, \
which backends to create, and how backends should be grouped. Answer with \
exactly one JSON object and no other text:

{\"summary\": \"one line\", \"items\": [{\"kind\": \"...\", \"target\": \"...\", \
\"change\": {...}, \"rationale\": \"...\", \"expected_effect\": \"...\", \
\"confidence\": \"high|medium|low\"}], \"experiments\": [{\"candidate_model_id\": \"...\", \
\"flags\": {\"n_ctx\": N, \"offload_layers\": N, \"tensor_split\": [fractions], \"moe_cache_mib\": N}, \
\"serves\": \"the directive or purpose need this serves\", \"hypothesis\": \"...\", \
\"predicted\": {\"predicted_per_second\": N, \"fit\": \"...\"}, \
\"requires_stopping\": [\"backend-id\", ...], \"run_eval\": true|false}]}

Allowed kinds and their change payloads:
- backend.create — target: the new backend's id. change: {\"model_id\": \"...\", optional \"group\", \"port\", \"n_ctx\", \"n_batch\", \"tensor_split\", \"offload_layers\", \"moe_cache_mib\"}
- backend.delete — target: backend id. change: {}
- backend.set_context — change: {\"n_ctx\": N}
- backend.set_flags — change: {\"n_ctx\": N, \"n_batch\": N} (either or both)
- backend.set_offload — change: {\"offload_layers\": N, optional \"moe_cache_mib\": N} (MoE layers whose experts move to system RAM; moe_cache_mib is the GPU-resident expert cache for the offloaded experts — nonzero requires offload_layers above 0 and a model whose MoE layer count is known; omit it to leave the cache unchanged, 0 turns it off)
- backend.set_placement — change: {\"tensor_split\": [fractions], optional \"split_mode\": \"none|layer|row\"}
- backend.set_model — change: {\"model_id\": \"...\"}
- backend.set_enabled — change: {\"enabled\": true|false}
- group.set_members — target: group name. change: {\"members\": [\"backend-id\", ...]}
- group.set_strategy — change: {\"strategy\": \"simple-shuffle|least-busy|latency-based\"}
- group.set_fallbacks — change: {\"fallbacks\": [\"group-name-or-openrouter\", ...]}
- group.create — target: the new group's name. change: {\"members\": [...], \"strategy\": \"...\", \"fallbacks\": [...]}
- group.delete — target: group name. change: {}
- model.add — target: the new model's id. change: {\"source_url\": \"https://....gguf\", optional \"quant\", \"sha256\", \"params_b\"} — registers the model only; it is not downloaded. \"params_b\" is the model's total parameter count in billions: include it whenever the model is not in the catalog candidates (use the \"candidates\" section's figure for a discovered candidate); a non-catalog model.add without a known params_b is invalid and cannot be applied
- advice — change: {\"text\": \"...\"} for anything worth saying that the kinds above cannot express

Constraints:
- When the user prompt carries an operator directive for this consultation, proposals must serve that directive; layout changes unrelated to it belong in an advice item at most, not as executable changes.
- Only these kinds. Only ids, group names, and model ids that appear in the context document (catalog candidates appear there too). A model that is neither registered nor in the catalog can only be proposed via model.add.
- A backend.create's model_id must be one of: a registered model id, a catalog candidate id, or a repo id (or repo/file) from the \"candidates\" section. When it names a catalog candidate or a discovered candidate, the app inserts the registering model.add ahead of the create itself, so you do not need to propose that add. Any other model name is dropped from the plan at build time — never invent a model id.
- Never propose raw llama-server flags. Flag changes are expressed only through the structured fields above; anything that needs a raw flag belongs in an advice item.
- tensor_split fractions must sum to 1.0 and have one entry per GPU.
- At most 12 items. Prefer a few high-value changes over a wholesale rewrite. Proposing no changes (an empty items list) is a valid answer.
- Every item carries a rationale and an expected_effect. confidence is your honest self-assessment: high, medium, or low.
- Per-group and per-backend purpose text, when present, states what that entity is for. When purpose is absent, infer cautiously from names rather than assuming.
- A \"measurements\" section, when present, carries speeds actually measured on this machine for the named backend's current configuration, with their age. Prefer measured figures over any estimate when both bear on a proposal.
- A \"candidates\" section, when present, carries models discovered on Hugging Face for this consultation's directive, gathered and fit-checked by the app before the consultation: the sizes, parameter counts, and rated contexts are the real figures Hugging Face reports, and each entry's fit verdict was computed by the app against this machine's free VRAM. These candidates are distinct from the curated catalog candidates, and each carries its Hugging Face repo id and retrieval time as provenance.
- Gateway port and bind address, downloads, installs, and updates are out of scope; use advice if they matter.
- Experiments are a sibling top-level \"experiments\" array, never plan items: an experiment tests a candidate model/configuration on this machine ephemerally and is cleaned up afterwards. At most 3 experiments. Each experiment's candidate_model_id must be a registered model id, a catalog model id, or an HF repo/file from the \"candidates\" section. An experiment runs its candidate from disk — the run itself never downloads — but an unregistered candidate that resolves (registered by a model.add the plan itself carries, a catalog id, a discovered candidate, or an HF repo/file) is provisioned through register/download plan steps the app chains and the operator applies first, so such candidates are proposable; a candidate no source can resolve will be excluded and should not be proposed. Every experiment must name, in \"serves\", the operator directive (or the purpose need) it serves; an experiment serving neither is advice, not an experiment. \"requires_stopping\" names backend ids the run may need stopped to fit; naming one is a consequential claim the operator must explicitly accept. Set \"run_eval\" when the candidate should also run the screening suite against the incumbent baseline.
- An \"experiment_results\" section, when present, carries results measured on this machine in a previous experiment batch: the candidate's measured speeds and screening scores. Experiment results are measured on this machine; a replacement claim requires screen at/above baseline, no baseline-passing category at zero, and higher measured speed.";

/// Rough token estimate for the pre-send fit check (§3): ~4
/// characters per token, rounded up. An estimate, and the refusal
/// message that uses it says so.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// The first-use seed for the deployment purpose (§2.3), assembled
/// from the install itself. The operator owns the text afterwards.
pub fn default_purpose_seed(cfg: &AppConfig) -> String {
    if cfg.gateway.groups.is_empty() {
        return "Local inference served to the LAN through one gateway.".to_string();
    }
    let parts: Vec<String> = cfg
        .gateway
        .groups
        .iter()
        .map(|g| {
            let models: Vec<String> = g
                .members
                .iter()
                .filter_map(|m| cfg.backends.iter().find(|b| &b.id == m))
                .filter_map(|b| resolve_model(cfg, b))
                .map(|m| model_short_name(cfg, m))
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            if models.is_empty() {
                g.name.clone()
            } else {
                format!("{} ({})", g.name, models.join(", "))
            }
        })
        .collect();
    format!(
        "Local inference served to the LAN through one gateway. Groups: {}.",
        parts.join("; ")
    )
}

/// One consult-candidate backend for the default ranking (§3).
#[derive(Debug, Clone)]
pub struct ConsultCandidate {
    pub id: String,
    pub running: bool,
    /// Rated context (catalog `ctx_max`), or the configured
    /// `n_ctx` for models outside the catalog.
    pub ctx_rank: u64,
    pub params_b: f32,
    /// Trailing-hour errors when a traffic window exists.
    pub errors_1h: u64,
}

/// The default consultation choice (§3): the running backend with
/// the largest rated context, tie-broken by parameter count, then
/// by fewest trailing-hour errors. Returns the best running
/// backend's id and the best backend overall (when they differ and
/// the overall best is stopped, the page names it — §1.2).
pub fn consult_default(candidates: &[ConsultCandidate]) -> (Option<String>, Option<String>) {
    let rank = |c: &ConsultCandidate| {
        (
            std::cmp::Reverse(c.ctx_rank),
            std::cmp::Reverse((c.params_b * 1000.0) as i64),
            c.errors_1h,
            c.id.clone(),
        )
    };
    let best_overall = candidates.iter().min_by_key(|c| rank(c));
    let best_running = candidates
        .iter()
        .filter(|c| c.running)
        .min_by_key(|c| rank(c));
    (
        best_running.map(|c| c.id.clone()),
        best_overall.map(|c| c.id.clone()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use manager_config::AppConfig;
    use std::path::PathBuf;

    fn sentinel_config() -> AppConfig {
        let mut cfg = AppConfig::default_config();
        // Secret-shaped raw flags (code review B-1): an
        // unconstrained operator string can carry an API key or a
        // token as a flag value. The benign `-ot exps=CPU` rides
        // along so the test also proves the decoded offload fact
        // still travels while the verbatim string does not.
        cfg.backends[0].raw_flags = Some(
            "--api-key SENTINEL-FLAG-KEY-63d1 --hf-token SENTINEL-HF-TOKEN-28be -ot exps=CPU"
                .to_string(),
        );
        cfg.data_dir = PathBuf::from("D:\\SENTINEL-DATA-DIR-7f3a");
        cfg.gateway.openrouter.enabled = true;
        cfg.gateway.openrouter.cred_ref = Some("SENTINEL-CRED-REF-9b1c".to_string());
        cfg.gateway.routing_rules.push(manager_config::RoutingRule {
            id: "r1".to_string(),
            description: "SENTINEL-RULE-DESC-44de".to_string(),
            enabled: true,
            kind: None,
            litellm_fragment: Some(json!({ "api_key": "SENTINEL-FRAGMENT-KEY-02fa" })),
            source: "user".to_string(),
        });
        cfg
    }

    /// The release-gate redaction test (design §2.4): a context
    /// assembled from a config seeded with sentinel secret-shaped
    /// values contains none of them, and none of the key names a
    /// secret could hide behind.
    #[test]
    fn context_contains_no_sentinel_secrets() {
        let cfg = sentinel_config();
        let (doc, _meta) = build_context(&cfg, &MeasureInputs::default(), "9.9.9", "planner");
        let text = serde_json::to_string(&doc).unwrap();
        for sentinel in [
            "SENTINEL-CRED-REF-9b1c",
            "SENTINEL-DATA-DIR-7f3a",
            "SENTINEL-RULE-DESC-44de",
            "SENTINEL-FRAGMENT-KEY-02fa",
            "SENTINEL-FLAG-KEY-63d1",
            "SENTINEL-HF-TOKEN-28be",
        ] {
            assert!(
                !text.contains(sentinel),
                "context leaked sentinel {sentinel}: {text}"
            );
        }
        let lower = text.to_lowercase();
        for key in [
            "cred_ref",
            "master_key",
            "masterkey",
            "password",
            "database_url",
            "api_key",
            "api-key",
            "routing_rules",
            "data_dir",
            // The verbatim flags string itself is not a context
            // field any more (B-1) — only facts decoded from it.
            "raw_flags",
        ] {
            assert!(
                !lower.contains(key),
                "context contains forbidden key {key}: {text}"
            );
        }
        // The decoded offload fact derived from the seeded flags
        // DOES travel (B-1 removes the verbatim string, not the
        // planner's legitimate derived facts).
        let seeded_id = cfg.backends[0].id.clone();
        let entry = doc["backends"]
            .as_array()
            .and_then(|arr| arr.iter().find(|b| b["id"] == json!(seeded_id)))
            .expect("seeded backend present in context");
        assert_eq!(entry["offload"]["setting"], json!("all_experts"));
        // The non-secret OpenRouter facts DO travel (§2.2).
        assert_eq!(doc["gateway"]["openrouter"]["enabled"], json!(true));
        assert_eq!(doc["gateway"]["openrouter"]["daily_cap_usd"], json!(5.0));
        // Postgres is a port, never a URL (§2.4).
        assert_eq!(doc["postgres"]["port"], json!(54329));
    }

    /// B-1, surfaces 2 and 3: the context document is persisted
    /// verbatim as `Plan.context` in optimize-plan.json and the
    /// plan is serialized whole into the Optimize `state_payload`
    /// for the UI. A plan built from the sentinel-seeded context
    /// must therefore carry no secret-shaped flag content on
    /// either surface.
    #[test]
    fn plan_surfaces_carry_no_sentinel_flag_secrets() {
        let cfg = sentinel_config();
        let (doc, _meta) = build_context(&cfg, &MeasureInputs::default(), "9.9.9", "planner");
        let plan = crate::parse::parse_plan(
            "{\"summary\":\"ok\",\"items\":[]}",
            &cfg,
            "planner",
            "hash",
            doc,
            &HashSet::new(),
            "",
            chrono::Utc::now(),
        )
        .expect("minimal plan parses");
        // Surface 2: the persisted plan (optimize-plan.json is
        // this struct serialized); surface 3: state_payload
        // embeds `serde_json::to_value(plan)`.
        let text = serde_json::to_string(&plan).unwrap();
        for sentinel in ["SENTINEL-FLAG-KEY-63d1", "SENTINEL-HF-TOKEN-28be"] {
            assert!(
                !text.contains(sentinel),
                "plan leaked sentinel {sentinel}: {text}"
            );
        }
        assert!(
            !text.to_lowercase().contains("raw_flags"),
            "plan carries a raw_flags field: {text}"
        );
    }

    #[test]
    fn context_carries_config_and_requirement_figures() {
        let mut cfg = AppConfig::default_config();
        cfg.models.push(ModelConfig {
            id: "qwen3-8b".to_string(),
            gguf_path: PathBuf::from("models/qwen3-8b.gguf"),
            quant: "Q4_K_M".to_string(),
            params_b: 8.0,
            sha256: "ab".repeat(32),
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
        let b = cfg
            .backends
            .iter_mut()
            .find(|b| b.id == "tool-runner")
            .unwrap();
        b.model_file = PathBuf::from("models/qwen3-8b.gguf");
        b.purpose = "tool-use support".to_string();
        let mut measure = MeasureInputs::default();
        measure.requirements.insert(
            "tool-runner".to_string(),
            MemoryRequirement {
                gpu_weights_mib: 5000,
                sys_weights_mib: 0,
                kv_mib: Some(512),
                rs_mib: None,
                moe_cache_mib: 0,
                moe_cache_offload_layers: 0,
                moe_cache_moe_layers: 0,
                basis: manager_models::MemoryBasis::Estimate,
            },
        );
        measure
            .backend_states
            .insert(
                "tool-runner".to_string(),
                BackendMeasured {
                    state: "running".to_string(),
                    uptime_secs: Some(60),
                    process_vram: vec![(0, None), (1, Some(4200))],
                },
            );
        let (doc, meta) = build_context(&cfg, &measure, "0.1.33", "tool-runner");
        let backend = doc["backends"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["id"] == "tool-runner")
            .unwrap()
            .clone();
        assert_eq!(backend["model_id"], json!("qwen3-8b"));
        assert_eq!(backend["purpose"], json!("tool-use support"));
        assert_eq!(backend["memory_requirement"]["gpu_ram_mib"], json!(5000));
        assert_eq!(backend["memory_requirement"]["kv_ram_mib"], json!(512));
        // Not-measured VRAM stays null, never 0 (§2.3).
        assert_eq!(
            doc["measured"]["backends"][0]["process_vram"][0]["vram_mib"],
            Value::Null
        );
        assert_eq!(
            doc["measured"]["backends"][0]["process_vram"][1]["vram_mib"],
            json!(4200)
        );
        assert_eq!(meta.backend_count, 4);
        // A catalog candidate not in the config is offered (§2.2).
        assert!(doc["catalog_candidates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "qwen3-coder-30b"));
    }

    #[test]
    fn measurements_section_present_only_with_facts() {
        // Addendum A.1 reservation: the schema carries measured
        // throughput when the store supplies it, and stays absent
        // (never fabricated, never zero) until then.
        let cfg = AppConfig::default_config();
        let (doc, _) = build_context(&cfg, &MeasureInputs::default(), "1", "planner");
        assert!(doc.get("measurements").is_none());
        let mut measure = MeasureInputs::default();
        measure.throughput.push(ThroughputFact {
            backend_id: "planner".to_string(),
            config_fingerprint: "abc123".to_string(),
            prompt_per_second: Some(812.5),
            predicted_per_second: Some(21.8),
            ttft_ms: Some(240.0),
            age_secs: 3600,
            superseded: false,
        });
        let (doc, meta) = build_context(&cfg, &measure, "1", "planner");
        let t = &doc["measurements"]["throughput"][0];
        assert_eq!(t["backend"], json!("planner"));
        assert_eq!(t["basis"], json!("measured"));
        assert_eq!(t["predicted_per_second"], json!(21.8));
        assert_eq!(t["age_secs"], json!(3600));
        // A current fact carries no superseded key at all.
        assert!(t.get("superseded").is_none());
        assert!(meta
            .measured_blocks
            .iter()
            .any(|b| b == "measured throughput"));
    }

    #[test]
    fn measurements_section_labels_superseded_facts() {
        // Addendum A.1: an entry under a superseded fingerprint
        // may ride along as delta history, labeled as such.
        let cfg = AppConfig::default_config();
        let mut measure = MeasureInputs::default();
        measure.throughput.push(ThroughputFact {
            backend_id: "planner".to_string(),
            config_fingerprint: "old456".to_string(),
            prompt_per_second: Some(700.0),
            predicted_per_second: Some(19.9),
            ttft_ms: Some(260.0),
            age_secs: 7200,
            superseded: true,
        });
        let (doc, _) = build_context(&cfg, &measure, "1", "planner");
        let t = &doc["measurements"]["throughput"][0];
        assert_eq!(t["superseded"], json!(true));
        assert_eq!(t["basis"], json!("measured"));
    }

    #[test]
    fn experiment_results_block_present_only_with_facts() {
        // Addendum A.3 iteration: finished experiment results
        // ride as their own block for the follow-up consultation.
        let cfg = AppConfig::default_config();
        let (doc, _) = build_context(&cfg, &MeasureInputs::default(), "1", "planner");
        assert!(doc.get("experiment_results").is_none());
        let mut measure = MeasureInputs::default();
        measure.experiment_results.push(ExperimentResultFact {
            candidate_model_id: "qwen3-coder-30b".to_string(),
            status: "completed".to_string(),
            prompt_per_second: Some(900.0),
            predicted_per_second: Some(35.0),
            short_predicted_per_second: Some(35.0),
            long_predicted_per_second: Some(33.0),
            ttft_ms: Some(180.0),
            screen_overall: Some(crate::eval::Score {
                passed: 11,
                total: 12,
            }),
            screen_per_category: Default::default(),
            delta_vs_baseline: Some("screen overall 10 -> 11 (delta +1 passed)".to_string()),
        });
        let (doc, meta) = build_context(&cfg, &measure, "1", "planner");
        let r = &doc["experiment_results"][0];
        assert_eq!(r["candidate_model_id"], json!("qwen3-coder-30b"));
        assert_eq!(r["basis"], json!("measured"));
        assert_eq!(r["generation_per_second"]["short"], json!(35.0));
        assert_eq!(r["screen"]["overall"]["passed"], json!(11));
        assert!(meta.measured_blocks.iter().any(|b| b == "experiment results"));
        // The system prompt states the experiments contract and
        // the §A.4 replacement-claim instruction verbatim-ish.
        let (system, _user) = build_prompts("P", "", &doc, &meta);
        assert!(system.contains("\"experiments\" array"), "{system}");
        assert!(system.contains("the run itself never downloads"), "{system}");
        assert!(system.contains("screen at/above baseline, no baseline-passing category at zero, and higher measured speed"), "{system}");
    }

    #[test]
    fn candidates_block_present_only_with_facts_and_distinct_from_catalog() {
        // Addendum A.5 (Phase 5c): discovered HF candidates ride
        // as their own top-level block with per-entry provenance,
        // separate from the curated catalog_candidates.
        let cfg = AppConfig::default_config();
        let (doc, _) = build_context(&cfg, &MeasureInputs::default(), "1", "planner");
        assert!(doc.get("candidates").is_none());
        assert!(doc.get("catalog_candidates").is_some());

        let mut measure = MeasureInputs::default();
        measure.candidates.push(CandidateFact {
            repo_id: "Qwen/Qwen3-30B-A3B-GGUF".to_string(),
            file: "Qwen3-30B-A3B-Q4_K_M.gguf".to_string(),
            architecture: "qwen3moe".to_string(),
            params_b: 30.5,
            ctx_max: 40960,
            size_bytes: 18_600_000_000,
            downloads: 123456,
            fit: "fits_with_split".to_string(),
            retrieved_at: "2026-10-17T12:00:00+00:00".to_string(),
        });
        let (doc, _meta) = build_context(&cfg, &measure, "1", "planner");
        let c = &doc["candidates"][0];
        assert_eq!(c["repo_id"], json!("Qwen/Qwen3-30B-A3B-GGUF"));
        assert_eq!(c["file"], json!("Qwen3-30B-A3B-Q4_K_M.gguf"));
        assert_eq!(c["architecture"], json!("qwen3moe"));
        assert_eq!(c["ctx_max"], json!(40960));
        assert_eq!(c["size_bytes"], json!(18_600_000_000u64));
        assert_eq!(c["fit"], json!("fits_with_split"));
        // Provenance: the retrieval timestamp rides per entry.
        assert_eq!(c["retrieved_at"], json!("2026-10-17T12:00:00+00:00"));
        // Distinct from the catalog block: different entries,
        // different key, and the catalog side is untouched.
        let catalog_ids: Vec<&str> = doc["catalog_candidates"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["id"].as_str())
            .collect();
        assert!(!catalog_ids.contains(&"Qwen/Qwen3-30B-A3B-GGUF"));
        assert!(catalog_ids.contains(&"qwen3-coder-30b"));
    }

    #[test]
    fn system_prompt_explains_the_candidates_block() {
        let cfg = AppConfig::default_config();
        let (doc, meta) = build_context(&cfg, &MeasureInputs::default(), "1", "planner");
        let (system, _user) = build_prompts("MY PURPOSE", "", &doc, &meta);
        assert!(system.contains("\"candidates\" section"), "{system}");
        assert!(system.contains("Hugging Face"), "{system}");
        assert!(system.contains("distinct from the curated catalog"), "{system}");
        assert!(system.contains("fit verdict"), "{system}");
    }

    #[test]
    fn purpose_seed_names_groups_and_models() {
        let cfg = AppConfig::default_config();
        let seed = default_purpose_seed(&cfg);
        assert!(seed.contains("planner"), "{seed}");
        assert!(seed.contains("gateway"), "{seed}");
    }

    #[test]
    fn consult_default_prefers_running_then_context_then_params() {
        let cands = vec![
            ConsultCandidate {
                id: "big-stopped".to_string(),
                running: false,
                ctx_rank: 262144,
                params_b: 80.0,
                errors_1h: 0,
            },
            ConsultCandidate {
                id: "small-running".to_string(),
                running: true,
                ctx_rank: 32768,
                params_b: 8.0,
                errors_1h: 0,
            },
            ConsultCandidate {
                id: "mid-running".to_string(),
                running: true,
                ctx_rank: 131072,
                params_b: 27.0,
                errors_1h: 3,
            },
        ];
        let (running, overall) = consult_default(&cands);
        assert_eq!(running.as_deref(), Some("mid-running"));
        assert_eq!(overall.as_deref(), Some("big-stopped"));
    }

    #[test]
    fn prompts_carry_purpose_and_measured_statement() {
        let cfg = AppConfig::default_config();
        let (doc, meta) = build_context(&cfg, &MeasureInputs::default(), "1", "planner");
        let (system, user) = build_prompts("MY PURPOSE", "", &doc, &meta);
        assert!(system.contains("backend.set_context"));
        assert!(user.starts_with("MY PURPOSE"));
        assert!(user.contains("configuration, not measurement"));
        assert!(user.contains("```json"));
        // No directive: no directive block anywhere (addendum A.2).
        assert!(!user.contains("Operator directive"));
    }

    #[test]
    fn prompts_carry_directive_verbatim_after_purpose() {
        let cfg = AppConfig::default_config();
        let (doc, meta) = build_context(&cfg, &MeasureInputs::default(), "1", "planner");
        let directive = "Explore better MoE capable models for the planner";
        let (system, user) = build_prompts("MY PURPOSE", directive, &doc, &meta);
        // The block is present, labeled, and carries the text verbatim.
        assert!(user.contains(&format!(
            "Operator directive for this consultation:\n{directive}"
        )));
        // Positioned directly after the purpose, before the
        // measured-state line and the context document.
        let purpose_at = user.find("MY PURPOSE").unwrap();
        let directive_at = user.find("Operator directive").unwrap();
        let measured_at = user.find("Measured state").unwrap();
        let context_at = user.find("Context:").unwrap();
        assert!(purpose_at < directive_at);
        assert!(directive_at < measured_at);
        assert!(measured_at < context_at);
        // The system prompt narrows its framing to the directive.
        assert!(system.contains("operator directive"));
        assert!(system.contains("advice item at most"));
    }

    #[test]
    fn prompts_omit_whitespace_only_directive() {
        let cfg = AppConfig::default_config();
        let (doc, meta) = build_context(&cfg, &MeasureInputs::default(), "1", "planner");
        let (_system, user) = build_prompts("MY PURPOSE", "  \n  ", &doc, &meta);
        assert!(!user.contains("Operator directive"));
        assert!(user.starts_with("MY PURPOSE\n\nMeasured state"));
    }
}
