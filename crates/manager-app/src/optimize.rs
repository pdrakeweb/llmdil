//! Optimize page commands (design: docs/optimize-design.md §7).
//!
//! The page's server side: state assembly (config + telemetry +
//! supervisor + gateway stats, fed to `manager-optimize`'s pure
//! core), the consultation task with real scoped cancellation,
//! plan persistence and decisions. Apply has no command here by
//! design — the UI orchestrates the existing mutation commands
//! (§5.2), and this module's commands never mutate configuration
//! except purpose text (`optimize_save_purpose`, a scoped save
//! through `manager_config::save` like every other config write).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use manager_config::AppConfig;
use manager_optimize::{
    benchmark_prompts, build_context, build_prompts, config_fingerprint, consult_default,
    default_purpose_seed, estimate_prompts, estimate_tokens, headline_figures, measurement_facts,
    measurement_fingerprint, parse_plan, probe_hardware, validate_plan, BackendMeasured,
    BenchResult, BenchmarkPrompt, CandidateFact, ConsultCandidate, Decision, DiscoveryFunnel,
    EstimateEntry, HardwareContext, HfClient, ItemValidation, LayoutFacts, LiveSnapshot,
    MeasureInputs, MeasurementEntry, MeasurementFingerprintInputs, MeasurementInputs,
    MeasurementSource, MeasurementStore, Plan, PlanItem, PlanKind, TrafficWindow, ValidationStatus,
    BENCHMARK_TEMPERATURE, ESTIMATE_RUNS, MEASURED_RUNS,
};
use manager_supervisor::ProcessState;

use crate::{ApiError, ApiResult, AppState, TaskStatus};

mod candidates;
mod consult;
mod jobs;
mod measure;
mod payload;

pub(crate) use candidates::*;
pub use consult::*;
pub(crate) use jobs::*;
pub use measure::*;
pub use payload::*;

/// Consultation generation budget (design §3).
const CONSULT_MAX_TOKENS: u32 = 4096;
const CONSULT_TEMPERATURE: f32 = 0.2;
const CONSULT_TIMEOUT: Duration = Duration::from_secs(900);
/// Per-run benchmark timeout (Phase 5a): generous, because the
/// long prompt on a slow (heavily offloaded) backend is the
/// slowest legitimate run — prompt processing alone can take
/// minutes there.
const MEASURE_RUN_TIMEOUT: Duration = Duration::from_secs(600);

// ---------------------------------------------------------------------------
// Optimize log (tracker #52): very verbose records of the whole
// pipeline in <data_dir>/logs/optimize.log, served by get_logs as
// the "optimize" scope. Written through crate::append_optimize_log
// (the wizard/gateway file pattern); see that function for the
// secret-free argument. Every line is stamped the way the wizard
// log stamps its lines — the reader stamps records with the file
// mtime, so the in-line stamp is what keeps the file readable on
// its own.
// ---------------------------------------------------------------------------

fn opt_log_line(text: &str) -> String {
    format!("[{}] {text}", chrono::Utc::now().to_rfc3339())
}

pub(crate) fn opt_log(state: &AppState, lines: &[String]) {
    crate::append_optimize_log(&state.data_dir, lines);
}

pub(crate) fn opt_log1(state: &AppState, text: &str) {
    opt_log(state, &[opt_log_line(text)]);
}

/// The context manifest for the log: the sections assembled with
/// their counts, the byte sizes of what was sent, and the model
/// sets the consultation was grounded in. Rendered ONLY from the
/// context document itself — the manifest records exactly what
/// the consulted model was told, and the document is
/// allowlist-built (manager-optimize's context.rs sentinel test),
/// so nothing outside it can reach the log through here.
fn context_manifest_lines(context: &Value, system_prompt: &str, user_prompt: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut sections: Vec<String> = Vec::new();
    if let Some(obj) = context.as_object() {
        for (key, value) in obj {
            let size = match value {
                Value::Array(a) => format!("{} entries", a.len()),
                Value::Object(o) => format!("{} fields", o.len()),
                _ => "scalar".to_string(),
            };
            sections.push(format!("{key} ({size})"));
        }
    }
    lines.push(opt_log_line(&format!(
        "context manifest: sections: {}",
        sections.join(", ")
    )));
    lines.push(opt_log_line(&format!(
        "context manifest: sizes: system prompt {} bytes, user prompt {} bytes, context document {} bytes",
        system_prompt.len(),
        user_prompt.len(),
        serde_json::to_string(context).map(|s| s.len()).unwrap_or(0)
    )));
    let ids = |key: &str| -> Vec<String> {
        context
            .get(key)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| e.get("id").and_then(|v| v.as_str()).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    lines.push(opt_log_line(&format!(
        "context manifest: registered models sent: [{}]",
        ids("models").join(", ")
    )));
    lines.push(opt_log_line(&format!(
        "context manifest: catalog candidates sent: [{}]",
        ids("catalog_candidates").join(", ")
    )));
    let candidates: Vec<String> = context
        .get("candidates")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    let repo = c.get("repo_id")?.as_str()?;
                    let file = c.get("file")?.as_str()?;
                    let params = c.get("params_b").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    Some(format!("{repo}/{file} (params_b {params})"))
                })
                .collect()
        })
        .unwrap_or_default();
    lines.push(opt_log_line(&format!(
        "context manifest: discovery candidates sent: [{}]",
        candidates.join(", ")
    )));
    lines
}

use crate::{OptimizeOpFamily, OptimizeRunningOp};

// ---------------------------------------------------------------------------
// Sample mode (design Phase 2): a locally generated plan against
// the real context and real ids — no backend is consulted.
// ---------------------------------------------------------------------------

fn sample_response(cfg: &AppConfig) -> String {
    let mut items: Vec<Value> = Vec::new();
    if let Some(b) = cfg.backends.first() {
        items.push(json!({
            "kind": "backend.set_context", "target": b.id,
            "change": { "n_ctx": b.flags.n_ctx.saturating_mul(2).min(262144) },
            "rationale": "Sample item: doubling the context doubles the KV cache — the expected effect on the row is computed from the model's real geometry.",
            "expected_effect": "Larger context window", "confidence": "medium",
        }));
        items.push(json!({
            "kind": "backend.set_flags", "target": b.id,
            "change": { "n_batch": 1024 },
            "rationale": "Sample item: a larger batch raises prompt-processing throughput at some memory cost.",
            "expected_effect": "Faster prompt processing", "confidence": "low",
        }));
    }
    if let Some(g) = cfg.gateway.groups.first() {
        items.push(json!({
            "kind": "group.set_strategy", "target": g.name,
            "change": { "strategy": "least-busy" },
            "rationale": "Sample item: least-busy spreads requests across the group's members instead of shuffling.",
            "expected_effect": "Better load spread", "confidence": "medium",
        }));
    }
    if let Some(b) = cfg.backends.iter().find(|b| !b.enabled) {
        items.push(json!({
            "kind": "backend.set_enabled", "target": b.id,
            "change": { "enabled": true },
            "rationale": "Sample item: this backend is disabled; enabling it adds serving capacity to its group.",
            "expected_effect": "More capacity", "confidence": "low",
        }));
    }
    items.push(json!({
        "kind": "model.add", "target": "sample-model",
        "change": { "source_url": "https://example.com/models/sample-model-Q4_K_M.gguf", "quant": "Q4_K_M", "params_b": 8.0 },
        "rationale": "Sample item: model.add registers a model without downloading it — the warning on the row is standing, and the download stays a Models-view act.",
        "expected_effect": "Model registered", "confidence": "low",
    }));
    if let Some(b) = cfg.backends.last() {
        items.push(json!({
            "kind": "backend.delete", "target": b.id, "change": {},
            "rationale": "Sample item: deletes start denied. Flip to Accepted only if you mean it; applying stops the backend first, then deletes it.",
            "expected_effect": "Backend removed", "confidence": "low",
        }));
    }
    items.push(json!({
        "kind": "advice", "target": null,
        "change": { "text": "Sample advice item: advice renders as text only and never executes. Items whose payload the app cannot validate are demoted to advice with their reasoning preserved." },
        "rationale": "", "expected_effect": "", "confidence": "low",
    }));
    let experiments: Vec<Value> = cfg
        .backends
        .first()
        .and_then(|b| crate::model_for_backend(cfg, b))
        .map(|m| {
            vec![json!({
                "candidate_model_id": m.id,
                "flags": { "n_ctx": 32768 },
                "serves": "Sample experiment: re-measure the consulted backend's own model under a temporary backend to show the experiment flow.",
                "hypothesis": "Sample hypothesis: the temporary backend should measure close to the serving backend's rates.",
                "predicted": { "predicted_per_second": 20.0, "fit": "alongside running backends" },
                "requires_stopping": [],
                "run_eval": true,
            })]
        })
        .unwrap_or_default();
    json!({
        "summary": "Sample plan — generated locally; no backend was consulted.",
        "items": items,
        "experiments": experiments,
    })
    .to_string()
}

#[cfg(test)]
mod tests;
