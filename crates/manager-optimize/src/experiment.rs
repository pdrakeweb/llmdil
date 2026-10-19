//! Experiment proposals and results (design addendum A.3,
//! tracker #28 Phase 5e).
//!
//! An experiment is NOT a plan item: plan items mutate the
//! configuration permanently, while an experiment is ephemeral,
//! measured, and self-cleaning. Proposals ride on the plan as a
//! sibling top-level `experiments` array, parsed with the same
//! salvage discipline as items (per-entry drop, cap 3), and each
//! carries the same tri-state discipline — undecided never runs.
//! A proposal naming backends in `requires_stopping` starts
//! denied (the deletes' precedent, §A.3): naming a backend to
//! stop is a consequential claim, and the claim is stated on the
//! proposal itself.
//!
//! This module holds the pure parts of the phase: the proposal
//! and result types, light validation, candidate resolution
//! (experiments never download — a candidate whose file is not
//! already on disk fails preflight), the fit-verdict text, the
//! temporary backend's configuration, the lowest-free-port rule,
//! and the experiment journal's lifecycle. The I/O runner (spawn,
//! benchmark, eval, guaranteed cleanup) lives in the src-tauri
//! glue (`optimize_experiments.rs`), which cannot be unit-tested
//! on this platform; everything it decides that can be decided
//! purely is decided here.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use manager_config::{AppConfig, BackendConfig, RestartPolicy, ServerFlags, SplitMode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::eval::{EvalComparison, EvalRunRecord};
use crate::measure::BenchResult;
use crate::plan::{canonical_json, Decision};
use crate::validate::ValidationStatus;

/// Hard cap on experiment proposals per consultation (§A.3):
/// the parse cap and the per-batch execution cap are the same
/// number, enforced in both places (the runner re-checks).
pub const MAX_EXPERIMENTS: usize = 3;

/// Per-experiment wall-clock budget in seconds (§A.3, decision
/// A.8.2): 30 minutes, load and eval included. Enforced by the
/// runner; stated here so the UI and the runner agree.
pub const EXPERIMENT_TIMEOUT_SECS: u64 = 30 * 60;
/// Per-batch wall-clock budget in seconds (§A.3): 60 minutes.
pub const BATCH_TIMEOUT_SECS: u64 = 60 * 60;

// ---------------------------------------------------------------------------
// Proposal types
// ---------------------------------------------------------------------------

/// The structured flags an experiment runs its candidate under.
/// Every field is optional: an absent field is the temp
/// backend's default (the `backend.create` defaults — context
/// 32768, batch 512, no split, no offload), never an inherited
/// value from some other backend.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExperimentFlags {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n_ctx: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offload_layers: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tensor_split: Option<Vec<f32>>,
}

/// The model's prediction for one proposal: prose-grade numbers
/// the page shows beside the measured result afterwards. Both
/// fields are optional in the payload; a missing prediction is
/// absent, never zero-filled.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExperimentPredicted {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicted_per_second: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit: Option<String>,
}

/// Terminal status of one experiment run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentStatus {
    Completed,
    Failed,
    Timeout,
    Cancelled,
}

impl ExperimentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExperimentStatus::Completed => "completed",
            ExperimentStatus::Failed => "failed",
            ExperimentStatus::Timeout => "timeout",
            ExperimentStatus::Cancelled => "cancelled",
        }
    }
}

/// The measured rates of one completed benchmark: both prompts'
/// median figures (each `BenchResult` also retains every run).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentMeasured {
    pub short: BenchResult,
    pub long: BenchResult,
}

/// The recorded outcome of one experiment run. Stored on the
/// plan's proposal AND in the experiment journal's history, so
/// results survive both cleanup and plan replacement (§A.3
/// step 4). `measured` / `eval` are absent when the run ended
/// before that stage; `comparison` is present only when a
/// baseline was available to compare the eval against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentResult {
    /// 1-based proposal id within its plan.
    #[serde(default)]
    pub experiment_id: usize,
    #[serde(default)]
    pub candidate_model_id: String,
    /// The batch (one `optimize_run_experiments` invocation)
    /// that produced this result; the follow-up consultation
    /// surfaces the latest batch's results by this key.
    #[serde(default)]
    pub batch_id: String,
    pub status: ExperimentStatus,
    /// Operator-facing outcome text: what happened, in numbers
    /// where numbers exist (including the restart note for any
    /// `requires_stopping` backends the batch restored).
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured: Option<ExperimentMeasured>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval: Option<EvalRunRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparison: Option<EvalComparison>,
    /// The preflight verdict, stated in numbers (§A.3 step 1).
    /// Empty when the run never reached preflight.
    pub preflight: String,
    pub started_at: String,
    pub finished_at: String,
}

/// One experiment proposal (design §A.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentProposal {
    /// 1-based position among the plan's experiments, assigned
    /// at parse.
    pub id: usize,
    /// Hash of candidate + flags + serves; denial memory keys
    /// off it across consultations, as for plan items.
    pub fingerprint: String,
    /// A registered model id, a catalog id, or an HF repo/file
    /// from the candidates block (§A.3). Resolution to an
    /// on-disk file happens at preflight; experiments never
    /// download.
    pub candidate_model_id: String,
    #[serde(default)]
    pub flags: ExperimentFlags,
    /// The directive (or purpose need) this experiment serves,
    /// in words. Required: a proposal serving neither is advice,
    /// not an experiment — the parser drops entries whose
    /// `serves` is empty (counted in the plan's
    /// `experiments_dropped_invalid`).
    pub serves: String,
    #[serde(default)]
    pub hypothesis: String,
    #[serde(default)]
    pub predicted: ExperimentPredicted,
    /// Backend ids the run may stop to make room. Naming one
    /// starts the proposal denied (§A.3).
    #[serde(default)]
    pub requires_stopping: Vec<String>,
    /// Run the §A.4 screening suite against the candidate.
    #[serde(default)]
    pub run_eval: bool,
    pub decision: Decision,
    #[serde(default)]
    pub previously_denied: bool,
    /// The recorded outcome once a batch has run this proposal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ExperimentResult>,
}

/// An experiment's fingerprint: hash of candidate + flags +
/// serves, over canonical JSON so key order cannot matter.
pub fn experiment_fingerprint(
    candidate_model_id: &str,
    flags: &ExperimentFlags,
    serves: &str,
) -> String {
    let mut h = Sha256::new();
    h.update(b"experiment\n");
    h.update(candidate_model_id.as_bytes());
    h.update(b"\n");
    let flags_json = serde_json::to_value(flags).unwrap_or(serde_json::Value::Null);
    h.update(canonical_json(&flags_json).as_bytes());
    h.update(b"\n");
    h.update(serves.as_bytes());
    format!("{:x}", h.finalize())
}

// ---------------------------------------------------------------------------
// Light validation (state payload)
// ---------------------------------------------------------------------------

/// One experiment's light validation verdict — the same shape
/// the plan items' validations use (`ValidationStatus` plus an
/// optional reason), computed purely from the proposal and the
/// current configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExperimentValidation {
    pub status: ValidationStatus,
    pub reason: Option<String>,
}

/// Whether a candidate string has the HF repo/file form
/// (`owner/repo/file.gguf`, at least two slashes, `.gguf` tail).
pub fn is_hf_form(candidate: &str) -> bool {
    let parts: Vec<&str> = candidate.split('/').collect();
    parts.len() >= 3
        && parts.iter().all(|p| !p.trim().is_empty())
        && parts[parts.len() - 1].to_lowercase().ends_with(".gguf")
}

/// Light validation of one proposal (§A.3 state surface):
/// the candidate must be resolvable in principle (a registered
/// model id, a curated-catalog id, an HF repo/file form, or one
/// of `extra_candidate_ids` — the candidates-block entries the
/// consultation carried, as `repo_id/file`), the flags must be
/// sane (a named context is positive; a named tensor split sums
/// to 1.0), and every `requires_stopping` id must name a
/// configured backend. On-disk existence is NOT checked here —
/// that is preflight's job, stated in its own numbers.
pub fn validate_experiment(
    proposal: &ExperimentProposal,
    cfg: &AppConfig,
    extra_candidate_ids: &[String],
) -> ExperimentValidation {
    let mut problems: Vec<String> = Vec::new();
    let candidate = proposal.candidate_model_id.trim();
    if candidate.is_empty() {
        problems.push("candidate_model_id is empty".to_string());
    } else {
        let registered = cfg.models.iter().any(|m| m.id == candidate);
        let catalog = manager_models::curated_models()
            .iter()
            .any(|s| s.id == candidate);
        if !registered
            && !catalog
            && !is_hf_form(candidate)
            && !extra_candidate_ids.iter().any(|c| c == candidate)
        {
            problems.push(format!(
                "candidate '{candidate}' is not a registered model, a catalog model, or an HF repo/file"
            ));
        }
    }
    if let Some(n) = proposal.flags.n_ctx {
        if n == 0 {
            problems.push("n_ctx must be greater than 0".to_string());
        }
    }
    if let Some(split) = &proposal.flags.tensor_split {
        if split.is_empty() {
            problems.push("tensor_split is empty".to_string());
        } else {
            let sum: f32 = split.iter().sum();
            if (sum - 1.0).abs() > 0.01 {
                problems.push(format!("tensor split sums to {sum:.2}, not 1.0"));
            }
            if split.iter().any(|f| *f < 0.0) {
                problems.push("tensor split contains a negative fraction".to_string());
            }
        }
    }
    for id in &proposal.requires_stopping {
        if !cfg.backends.iter().any(|b| &b.id == id) {
            problems.push(format!("requires_stopping names unknown backend '{id}'"));
        }
    }
    if problems.is_empty() {
        ExperimentValidation {
            status: ValidationStatus::Valid,
            reason: None,
        }
    } else {
        ExperimentValidation {
            status: ValidationStatus::Invalid,
            reason: Some(problems.join("; ")),
        }
    }
}

// ---------------------------------------------------------------------------
// Candidate resolution (preflight; experiments never download)
// ---------------------------------------------------------------------------

/// One registered model as resolution sees it: its id and its
/// GGUF path, already resolved to an absolute on-disk path by
/// the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredModelFile {
    pub id: String,
    pub path: PathBuf,
}

/// Candidate resolution's verdict. `OnDisk` carries the model
/// id to record (the registered/catalog id, or the candidate
/// string itself for an HF-form file) and the file's path.
/// `NotOnDisk` carries the failure text — every not-on-disk
/// outcome names the same remedy (§A.3: "download in Models
/// first"), because experiments never download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateResolution {
    OnDisk { model_id: String, path: PathBuf },
    NotOnDisk { message: String },
}

fn not_on_disk(candidate: &str, why: &str) -> CandidateResolution {
    CandidateResolution::NotOnDisk {
        message: format!(
            "candidate '{candidate}' is not on disk ({why}) — experiments never download; download in Models first"
        ),
    }
}

/// Resolve a proposal's candidate to an on-disk GGUF file.
///
/// Order (§A.3): a registered model id resolves to its file
/// (which must exist); a catalog id resolves only through a
/// registered model of the same id (the catalog is metadata,
/// not files); an HF repo/file form (`owner/repo/file.gguf`)
/// resolves when the file exists under the models directory or
/// a registered model's file has the same file name. Anything
/// else — and any missing file — is `NotOnDisk`.
pub fn resolve_candidate(
    candidate: &str,
    registered: &[RegisteredModelFile],
    catalog_ids: &[String],
    models_dir: &Path,
    file_exists: impl Fn(&Path) -> bool,
) -> CandidateResolution {
    let candidate = candidate.trim();
    if let Some(m) = registered.iter().find(|m| m.id == candidate) {
        return if file_exists(&m.path) {
            CandidateResolution::OnDisk {
                model_id: m.id.clone(),
                path: m.path.clone(),
            }
        } else {
            not_on_disk(candidate, "its registered model file is missing")
        };
    }
    if catalog_ids.iter().any(|id| id == candidate) {
        return not_on_disk(
            candidate,
            "it is a catalog model with no registered, downloaded model",
        );
    }
    if is_hf_form(candidate) {
        let file_name = candidate.rsplit('/').next().unwrap_or_default();
        if let Some(m) = registered.iter().find(|m| {
            m.path.file_name().map(|n| n.to_string_lossy()) == Some(file_name.into())
        }) {
            return if file_exists(&m.path) {
                CandidateResolution::OnDisk {
                    model_id: candidate.to_string(),
                    path: m.path.clone(),
                }
            } else {
                not_on_disk(candidate, "the matching registered file is missing")
            };
        }
        let under_models = models_dir.join(file_name);
        return if file_exists(&under_models) {
            CandidateResolution::OnDisk {
                model_id: candidate.to_string(),
                path: under_models,
            }
        } else {
            not_on_disk(candidate, "no file with that name is in the models directory")
        };
    }
    not_on_disk(
        candidate,
        "it is not a registered model, a catalog model, or an HF repo/file",
    )
}

// ---------------------------------------------------------------------------
// Ports, fit text, and the temporary backend
// ---------------------------------------------------------------------------

/// The lowest free port from 8081 up, skipping every used port
/// (configured backends, this batch's earlier temp backends)
/// plus the gateway and Postgres ports — the `backend.create`
/// parser's rule, mirrored for temp backends.
pub fn lowest_free_port(used: &HashSet<u16>, gateway_port: u16, postgres_port: u16) -> u16 {
    let mut used = used.clone();
    used.insert(gateway_port);
    used.insert(postgres_port);
    (8081..=8999)
        .find(|p| !used.contains(p))
        .unwrap_or(8999)
}

/// The preflight fit verdict, stated in numbers (§A.3 step 1):
/// what the candidate needs, what is free after the residency
/// of the backends that stay running, and — when the fit only
/// works by stopping — exactly which backends those are.
pub fn fit_verdict_text(
    needs_mib: u64,
    free_after_staying_mib: u64,
    fits_alongside: bool,
    fits_after_stopping: bool,
    stopping_ids: &[String],
) -> String {
    if fits_alongside {
        format!(
            "fits alongside the backends staying running: needs {needs_mib} MiB GPU RAM, {free_after_staying_mib} MiB free after their residency"
        )
    } else if fits_after_stopping && !stopping_ids.is_empty() {
        format!(
            "fits only after stopping {}: needs {needs_mib} MiB GPU RAM, {free_after_staying_mib} MiB free after the residency of the backends staying running",
            stopping_ids.join(", ")
        )
    } else {
        format!(
            "does not fit: needs {needs_mib} MiB GPU RAM, {free_after_staying_mib} MiB free after the residency of the backends staying running"
        )
    }
}

/// The temporary backend's id for one proposal within a plan:
/// `opt-exp-<experiment id>` (§A.3 step 2). One live plan means
/// ids cannot collide across plans; a leftover from a crashed
/// run is removed by the startup sweep (and by cleanup, which
/// unregisters before a re-run can reuse the id).
pub fn temp_backend_id(experiment_id: usize) -> String {
    format!("opt-exp-{experiment_id}")
}

/// Build the in-memory temporary backend for one proposal. It
/// exists only in the runner's cloned configuration and in the
/// supervisor's spec table — never in `config.json`, never in
/// a gateway group. Defaults mirror `backend.create`'s parser:
/// context 32768, batch 512; a named tensor split implies
/// `SplitMode::Layer` (the create simulation's rule); offload
/// layers become the raw-flags text a configured backend would
/// carry, resolved through the canonical translation
/// (`manager_models::offload_for_layers`, the same one the
/// backend offload slider and the create simulation use): 0
/// layers writes no offload tokens, the model's full MoE layer
/// count writes the all-experts form (`-ot exps=CPU`, never
/// `--n-cpu-moe <max>`), and anything between writes
/// `--n-cpu-moe N`. When the model's MoE layer count is
/// unknown (`moe_layers` is `None`) the count cannot be
/// resolved canonically, so the proposal's layer count is
/// written directly as `ExpertOffload::CpuMoeLayers(N)`.
pub fn temp_backend_config(
    proposal: &ExperimentProposal,
    model_file: PathBuf,
    port: u16,
    moe_layers: Option<u32>,
) -> BackendConfig {
    let split = proposal.flags.tensor_split.clone().unwrap_or_default();
    let raw_flags = proposal.flags.offload_layers.and_then(|n| {
        let offload = match moe_layers {
            Some(moe) => manager_models::offload_for_layers(n, moe),
            None => manager_models::ExpertOffload::CpuMoeLayers(n),
        };
        manager_models::set_expert_offload(None, offload)
    });
    BackendConfig {
        id: temp_backend_id(proposal.id),
        enabled: true,
        model_file,
        port,
        flags: ServerFlags {
            n_ctx: proposal.flags.n_ctx.unwrap_or(32768),
            n_batch: 512,
            split_mode: if split.is_empty() {
                SplitMode::None
            } else {
                SplitMode::Layer
            },
            tensor_split: split,
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
    }
}

/// Convert one finished result into the context fact a
/// follow-up consultation receives (§A.3 iteration). Headline
/// composition matches the measurement facts (§A.1): prompt
/// rate from the long runs, generation + TTFT from the short
/// runs, each falling back to the other prompt. The delta
/// summary states the §A.4 replacement predicates as data.
pub fn experiment_result_fact(result: &ExperimentResult) -> crate::context::ExperimentResultFact {
    let (prompt, predicted, ttft, short_pred, long_pred) = match &result.measured {
        Some(m) => (
            m.long.prompt_per_second.or(m.short.prompt_per_second),
            m.short
                .predicted_per_second
                .or(m.long.predicted_per_second),
            m.short.ttft_ms.or(m.long.ttft_ms),
            m.short.predicted_per_second,
            m.long.predicted_per_second,
        ),
        None => (None, None, None, None, None),
    };
    let delta_vs_baseline = result.comparison.as_ref().map(|c| {
        format!(
            "screen overall {} -> {} (delta {:+} passed); screen at/above baseline: {}; categories at zero that the baseline passes: {}",
            c.overall.baseline.passed,
            c.overall.candidate.passed,
            c.overall.delta_passed,
            c.screen_at_or_above_baseline,
            if c.zero_categories_that_baseline_passes.is_empty() {
                "none".to_string()
            } else {
                c.zero_categories_that_baseline_passes.join(", ")
            },
        )
    });
    crate::context::ExperimentResultFact {
        candidate_model_id: result.candidate_model_id.clone(),
        status: result.status.as_str().to_string(),
        prompt_per_second: prompt,
        predicted_per_second: predicted,
        short_predicted_per_second: short_pred,
        long_predicted_per_second: long_pred,
        ttft_ms: ttft,
        screen_overall: result.eval.as_ref().map(|e| e.scores.overall),
        screen_per_category: result
            .eval
            .as_ref()
            .map(|e| e.scores.per_category.clone())
            .unwrap_or_default(),
        delta_vs_baseline,
    }
}

// ---------------------------------------------------------------------------
// The experiment journal (<data_dir>/optimize-experiments.json)
// ---------------------------------------------------------------------------

/// One in-flight experiment's record: everything the startup
/// sweep needs to terminate a leftover temp backend after an
/// app crash — its temp id, its pid (once spawned), and its
/// port. Written before the spawn, removed by cleanup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveExperiment {
    pub temp_id: String,
    #[serde(default)]
    pub pid: Option<u32>,
    pub port: u16,
    pub experiment_id: usize,
    pub started_at: String,
}

/// The on-disk journal: active temp records (for the startup
/// sweep) plus the full result history (the point of running
/// experiments at all — results survive cleanup, plan
/// replacement, and app restarts).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExperimentJournal {
    #[serde(default)]
    pub active: Vec<ActiveExperiment>,
    #[serde(default)]
    pub history: Vec<ExperimentResult>,
}

impl ExperimentJournal {
    /// Load the journal at `path`. A missing or unparseable
    /// file is an empty journal: history is evidence, never
    /// load-bearing, matching the measurement store's rule.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| e.to_string())
    }

    /// Record an in-flight experiment (replacing any stale
    /// record under the same temp id).
    pub fn add_active(&mut self, record: ActiveExperiment) {
        self.active.retain(|a| a.temp_id != record.temp_id);
        self.active.push(record);
    }

    /// Stamp the spawned pid onto an active record.
    pub fn set_active_pid(&mut self, temp_id: &str, pid: u32) {
        if let Some(a) = self.active.iter_mut().find(|a| a.temp_id == temp_id) {
            a.pid = Some(pid);
        }
    }

    /// Remove an active record (cleanup, and the startup
    /// sweep). Returns whether one was present.
    pub fn remove_active(&mut self, temp_id: &str) -> bool {
        let before = self.active.len();
        self.active.retain(|a| a.temp_id != temp_id);
        self.active.len() != before
    }

    /// Append a finished result to the history.
    pub fn record_result(&mut self, result: ExperimentResult) {
        self.history.push(result);
    }

    /// The latest batch's results: every history entry sharing
    /// the newest entry's batch id, in run order, capped at
    /// [`MAX_EXPERIMENTS`]. This is what a follow-up
    /// consultation's context carries (§A.3 iteration).
    pub fn latest_batch(&self) -> Vec<&ExperimentResult> {
        let Some(last) = self.history.last() else {
            return Vec::new();
        };
        let batch = last.batch_id.clone();
        let mut out: Vec<&ExperimentResult> = self
            .history
            .iter()
            .filter(|r| r.batch_id == batch)
            .collect();
        if out.len() > MAX_EXPERIMENTS {
            out = out.split_off(out.len() - MAX_EXPERIMENTS);
        }
        out
    }

    /// Append a note to the detail of every result in one batch
    /// (the batch runner's restart note: what was stopped for
    /// the batch and restored afterwards is part of each
    /// result's record). Returns how many results were updated.
    pub fn append_batch_detail(&mut self, batch_id: &str, note: &str) -> usize {
        let mut n = 0;
        for r in self.history.iter_mut().filter(|r| r.batch_id == batch_id) {
            if !r.detail.is_empty() {
                r.detail.push(' ');
            }
            r.detail.push_str(note);
            n += 1;
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::{aggregate_results, EvalTaskResult};

    fn proposal(candidate: &str) -> ExperimentProposal {
        ExperimentProposal {
            id: 1,
            fingerprint: experiment_fingerprint(candidate, &ExperimentFlags::default(), "s"),
            candidate_model_id: candidate.to_string(),
            flags: ExperimentFlags::default(),
            serves: "s".to_string(),
            hypothesis: String::new(),
            predicted: ExperimentPredicted::default(),
            requires_stopping: Vec::new(),
            run_eval: false,
            decision: Decision::Undecided,
            previously_denied: false,
            result: None,
        }
    }

    fn cfg() -> AppConfig {
        // Default config: backends planner/coder/coder-fast/
        // tool-runner, no registered models.
        AppConfig::default_config()
    }

    #[test]
    fn validation_accepts_registered_catalog_and_hf_forms() {
        let mut c = cfg();
        c.models.push(manager_config::ModelConfig {
            id: "my-model".to_string(),
            gguf_path: PathBuf::from("models/my-model.gguf"),
            quant: "Q4_K_M".to_string(),
            params_b: 8.0,
            sha256: String::new(),
            verified_at: None,
            verified_sha256: None,
            source_url: String::new(),
            assigned_backends: Vec::new(),
            enabled: true,
            display_name: None,
        });
        for cand in [
            "my-model",              // registered
            "qwen3-coder-30b",       // curated catalog id
            "Qwen/Qwen3-30B-A3B-GGUF/Qwen3-30B-A3B-Q4_K_M.gguf", // HF form
        ] {
            let v = validate_experiment(&proposal(cand), &c, &[]);
            assert_eq!(v.status, ValidationStatus::Valid, "{cand}: {:?}", v.reason);
        }
        // A candidates-block entry (repo/file) also resolves.
        let v = validate_experiment(
            &proposal("Qwen/Qwen3-30B-A3B-GGUF"),
            &c,
            &["Qwen/Qwen3-30B-A3B-GGUF".to_string()],
        );
        assert_eq!(v.status, ValidationStatus::Valid);
        // An invented id does not.
        let v = validate_experiment(&proposal("totally-invented"), &c, &[]);
        assert_eq!(v.status, ValidationStatus::Invalid);
    }

    #[test]
    fn validation_flags_bad_flags_and_unknown_stopping_ids() {
        let c = cfg();
        let mut p = proposal("qwen3-coder-30b");
        p.flags.n_ctx = Some(0);
        p.flags.tensor_split = Some(vec![0.5, 0.4]);
        p.requires_stopping = vec!["ghost".to_string()];
        let v = validate_experiment(&p, &c, &[]);
        assert_eq!(v.status, ValidationStatus::Invalid);
        let reason = v.reason.unwrap();
        assert!(reason.contains("n_ctx"), "{reason}");
        assert!(reason.contains("sums to 0.90"), "{reason}");
        assert!(reason.contains("ghost"), "{reason}");
        // A split summing to 1.0 and a real stopping id pass.
        let mut p = proposal("qwen3-coder-30b");
        p.flags.tensor_split = Some(vec![0.5, 0.5]);
        p.requires_stopping = vec!["coder".to_string()];
        assert_eq!(
            validate_experiment(&p, &c, &[]).status,
            ValidationStatus::Valid
        );
    }

    #[test]
    fn resolve_registered_catalog_and_hf_candidates() {
        let registered = vec![
            RegisteredModelFile {
                id: "my-model".to_string(),
                path: PathBuf::from("/data/models/my-model.gguf"),
            },
            RegisteredModelFile {
                id: "hf-twin".to_string(),
                path: PathBuf::from("/data/models/Shared-Q4_K_M.gguf"),
            },
        ];
        let catalog = vec!["catalog-only".to_string()];
        let models_dir = Path::new("/data/models");
        let exists = |p: &Path| {
            p == Path::new("/data/models/my-model.gguf")
                || p == Path::new("/data/models/Shared-Q4_K_M.gguf")
                || p == Path::new("/data/models/Loose-Q5_K_M.gguf")
        };
        // Registered, on disk.
        match resolve_candidate("my-model", &registered, &catalog, models_dir, exists) {
            CandidateResolution::OnDisk { model_id, path } => {
                assert_eq!(model_id, "my-model");
                assert_eq!(path, PathBuf::from("/data/models/my-model.gguf"));
            }
            other => panic!("{other:?}"),
        }
        // Catalog id with no registered model: not on disk.
        match resolve_candidate("catalog-only", &registered, &catalog, models_dir, exists) {
            CandidateResolution::NotOnDisk { message } => {
                assert!(message.contains("download in Models first"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        // HF form matching a registered file name.
        match resolve_candidate(
            "someone/Repo-GGUF/Shared-Q4_K_M.gguf",
            &registered,
            &catalog,
            models_dir,
            exists,
        ) {
            CandidateResolution::OnDisk { path, .. } => {
                assert_eq!(path, PathBuf::from("/data/models/Shared-Q4_K_M.gguf"));
            }
            other => panic!("{other:?}"),
        }
        // HF form whose file sits loose in the models directory.
        match resolve_candidate(
            "someone/Other-GGUF/Loose-Q5_K_M.gguf",
            &registered,
            &catalog,
            models_dir,
            exists,
        ) {
            CandidateResolution::OnDisk { path, .. } => {
                assert_eq!(path, PathBuf::from("/data/models/Loose-Q5_K_M.gguf"));
            }
            other => panic!("{other:?}"),
        }
        // HF form with nothing on disk, and a registered model
        // whose file vanished: both fail with the same remedy.
        for cand in ["someone/Repo-GGUF/Missing.gguf", "hf-twin-missing"] {
            let registered2 = vec![RegisteredModelFile {
                id: "hf-twin-missing".to_string(),
                path: PathBuf::from("/data/models/Gone.gguf"),
            }];
            match resolve_candidate(cand, &registered2, &catalog, models_dir, exists) {
                CandidateResolution::NotOnDisk { message } => {
                    assert!(message.contains("download in Models first"), "{cand}: {message}");
                }
                other => panic!("{cand}: {other:?}"),
            }
        }
    }

    #[test]
    fn lowest_free_port_skips_configured_gateway_and_postgres() {
        let used: HashSet<u16> = [8081, 8082, 8084].into_iter().collect();
        assert_eq!(lowest_free_port(&used, 4000, 54329), 8083);
        let used: HashSet<u16> = [8081].into_iter().collect();
        // Gateway on 8082 must be skipped even though unused.
        assert_eq!(lowest_free_port(&used, 8082, 54329), 8083);
    }

    #[test]
    fn fit_verdict_text_states_the_numbers() {
        let alongside = fit_verdict_text(9000, 12000, true, true, &[]);
        assert!(alongside.contains("fits alongside"), "{alongside}");
        assert!(alongside.contains("9000 MiB"), "{alongside}");
        assert!(alongside.contains("12000 MiB"), "{alongside}");
        let stopping = fit_verdict_text(
            9000,
            12000,
            false,
            true,
            &["coder".to_string()],
        );
        assert!(stopping.contains("fits only after stopping coder"), "{stopping}");
        let no = fit_verdict_text(24000, 12000, false, false, &["coder".to_string()]);
        assert!(no.contains("does not fit"), "{no}");
        assert!(no.contains("24000 MiB"), "{no}");
    }

    #[test]
    fn temp_backend_config_applies_create_defaults_and_offload_text() {
        let mut p = proposal("cand");
        p.id = 2;
        let b = temp_backend_config(&p, PathBuf::from("models/cand.gguf"), 8090, Some(24));
        assert_eq!(b.id, "opt-exp-2");
        assert_eq!(b.port, 8090);
        assert_eq!(b.flags.n_ctx, 32768);
        assert_eq!(b.flags.n_batch, 512);
        assert_eq!(b.flags.split_mode, SplitMode::None);
        assert_eq!(b.raw_flags, None);
        // Named flags land: context, split (Layer, as a create),
        // and the CpuMoeLayers raw-flags text.
        p.flags.n_ctx = Some(65536);
        p.flags.tensor_split = Some(vec![0.5, 0.5]);
        p.flags.offload_layers = Some(12);
        let b = temp_backend_config(&p, PathBuf::from("models/cand.gguf"), 8090, Some(24));
        assert_eq!(b.flags.n_ctx, 65536);
        assert_eq!(b.flags.tensor_split, vec![0.5, 0.5]);
        assert_eq!(b.flags.split_mode, SplitMode::Layer);
        assert_eq!(b.raw_flags.as_deref(), Some("--n-cpu-moe 12"));
        // The full MoE layer count resolves canonically to the
        // all-experts form, never `--n-cpu-moe <max>`.
        p.flags.offload_layers = Some(24);
        let b = temp_backend_config(&p, PathBuf::from("models/cand.gguf"), 8090, Some(24));
        assert_eq!(b.raw_flags.as_deref(), Some("-ot exps=CPU"));
        // Zero layers writes no offload tokens.
        p.flags.offload_layers = Some(0);
        let b = temp_backend_config(&p, PathBuf::from("models/cand.gguf"), 8090, Some(24));
        assert_eq!(b.raw_flags, None);
        // Unknown MoE layer count: fall back to writing the
        // proposal's layer count directly.
        p.flags.offload_layers = Some(12);
        let b = temp_backend_config(&p, PathBuf::from("models/cand.gguf"), 8090, None);
        assert_eq!(b.raw_flags.as_deref(), Some("--n-cpu-moe 12"));
    }

    fn result(batch: &str, id: usize, detail: &str) -> ExperimentResult {
        ExperimentResult {
            experiment_id: id,
            candidate_model_id: "cand".to_string(),
            batch_id: batch.to_string(),
            status: ExperimentStatus::Completed,
            detail: detail.to_string(),
            measured: None,
            eval: None,
            comparison: None,
            preflight: String::new(),
            started_at: "t0".to_string(),
            finished_at: "t1".to_string(),
        }
    }

    #[test]
    fn journal_lifecycle_active_history_and_latest_batch() {
        let dir = std::env::temp_dir().join(format!(
            "manager-optimize-journal-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("optimize-experiments.json");
        assert!(ExperimentJournal::load(&path).active.is_empty());
        let mut j = ExperimentJournal::default();
        j.add_active(ActiveExperiment {
            temp_id: "opt-exp-1".to_string(),
            pid: None,
            port: 8090,
            experiment_id: 1,
            started_at: "t0".to_string(),
        });
        j.set_active_pid("opt-exp-1", 4242);
        assert_eq!(j.active[0].pid, Some(4242));
        j.record_result(result("b1", 1, "done."));
        j.record_result(result("b1", 2, "done."));
        j.record_result(result("b2", 1, "done."));
        // Latest batch is the newest batch only, cap respected.
        let latest: Vec<usize> = j.latest_batch().iter().map(|r| r.experiment_id).collect();
        assert_eq!(latest, vec![1]);
        assert_eq!(j.append_batch_detail("b1", "Restarted: coder."), 2);
        assert!(j.history[0].detail.ends_with("Restarted: coder."));
        assert!(j.remove_active("opt-exp-1"));
        assert!(!j.remove_active("opt-exp-1"));
        j.save(&path).unwrap();
        let loaded = ExperimentJournal::load(&path);
        assert_eq!(loaded, j);
        // Eval aggregates survive the round trip untouched.
        let mut r = result("b3", 1, "with eval");
        let run = EvalRunRecord::new(
            "opt-exp-1".to_string(),
            "t".to_string(),
            vec![EvalTaskResult {
                task_id: "t".to_string(),
                category: crate::eval::EvalCategory::FactualRecall,
                passed: true,
                output_excerpt: String::new(),
                reason: None,
            }],
        );
        r.comparison = Some(crate::eval::compare(
            &run.scores,
            &aggregate_results(&[]),
        ));
        r.eval = Some(run);
        let text = serde_json::to_string(&r).unwrap();
        let back: ExperimentResult = serde_json::from_str(&text).unwrap();
        assert_eq!(back, r);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
