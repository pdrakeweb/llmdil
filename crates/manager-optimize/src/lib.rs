//! Core logic for the Optimize page (design: docs/optimize-design.md).
//!
//! Everything in this crate is Tauri-free and pure over its inputs, so
//! it is unit-tested on Linux CI — deliberately unlike the logic that
//! lives in the src-tauri binary crate. Contents:
//!
//! - [`plan`]: the plan document types (items, tri-state decisions,
//!   apply outcomes) and the item fingerprint.
//! - [`parse`]: the salvage parser turning a backend's raw response
//!   into a [`plan::Plan`] (§4 of the design).
//! - [`context`]: the allowlist context builder (§2) — the document
//!   sent to the consulted backend, assembled field by field so no
//!   secret can enter it by construction — plus the prompt builder
//!   (§3) and the default consultation-candidate ranking.
//! - [`fit`]: the fit evaluator (§5): memory-requirement
//!   recomputation and per-GPU residency accounting over a telemetry
//!   free-VRAM snapshot.
//! - [`validate`]: the validation simulation (§5): items applied in
//!   plan order to a cloned `AppConfig`, checked with
//!   `manager_config::validate` plus the fit evaluator.
//! - [`probe`]: the CPU / system-RAM inventory probe (§2.1) — data
//!   that exists nowhere else in the app.
//! - [`discover`]: the Hugging Face discovery pre-pass (addendum
//!   A.5 option (b)): directive-seeded HF search, per-quant facts
//!   from the model record, and local fit verdicts, assembled as
//!   the context document's `candidates` block.
//! - [`eval`]: the screening evaluation suite (addendum A.4):
//!   twelve fixed, machine-checkable tasks, their pure checkers,
//!   run records and aggregates, and the baseline comparison
//!   predicates (presented as data, never used as a gate).

pub mod context;
pub mod discover;
pub mod eval;
pub mod experiment;
pub mod fit;
pub mod measure;
pub mod parse;
pub mod plan;
pub mod probe;
pub mod validate;

pub use context::{
    build_context, build_prompts, consult_default, default_purpose_seed, estimate_tokens,
    model_short_name, resolve_model, BackendMeasured, ConsultCandidate, ContextMeta,
    ExperimentResultFact, GpuContext, HardwareContext, MeasureInputs, ThroughputFact,
    TrafficWindow,
};
pub use experiment::{
    experiment_fingerprint, experiment_result_fact, fit_verdict_text, is_hf_form, lowest_free_port,
    resolve_candidate, temp_backend_config, temp_backend_id, validate_experiment, ActiveExperiment,
    CandidateResolution, ExperimentEstimate, ExperimentFlags, ExperimentJournal, ExperimentMeasured,
    ExperimentPredicted, ExperimentProposal, ExperimentResult, ExperimentStatus,
    ExperimentValidation, RegisteredModelFile, BATCH_TIMEOUT_SECS, EXPERIMENT_TIMEOUT_SECS,
    MAX_EXPERIMENTS,
};
pub use discover::{
    candidates_from_model, fit_verdict, parse_model_record, parse_search_results, quant_files,
    search_query_for_directive, seat_for_directive, CandidateFact, DiscoverError, HfClient,
    HfModelFacts, HfSearchHit, HfSibling, QuantFile, FIT_DOES_NOT_FIT, FIT_FITS,
    FIT_FITS_WITH_SPLIT, FIT_UNCHECKED, HF_DEFAULT_BASE_URL, MAX_QUERY_TERMS, MAX_SEARCH_REPOS,
};
pub use eval::{
    aggregate_results, compare, eval_suite, failed_task, output_excerpt, passes, score_task,
    suite_worst_case_tokens, BaselineRecord, Checker, EvalAggregate, EvalCategory, EvalComparison,
    EvalRunRecord, EvalTask, EvalTaskResult, JsonFieldSpec, JsonType, LinePattern, Score,
    ScoreDelta, EVAL_SUITE_VERSION, EVAL_TEMPERATURE, EVAL_TIME_BOX_SECS, MAX_TASK_OUTPUT_TOKENS,
    SCREENING_CAVEAT, SUITE_TOKEN_BUDGET,
};
pub use fit::{evaluate_fit, layout_requirement, residency_per_gpu, FitOutcome, LayoutFacts};
pub use measure::{
    benchmark_prompts, estimate_prompts, facts as measurement_facts, measurement_fingerprint,
    median, normalize_raw_flags, BenchResult, BenchRun, BenchmarkPrompt, EstimateEntry,
    MeasurementEntry, MeasurementFingerprintInputs, MeasurementStore, BENCHMARK_TEMPERATURE,
    ESTIMATE_LONG_MAX_TOKENS, ESTIMATE_RUNS, ESTIMATE_SHORT_MAX_TOKENS, LONG_MAX_TOKENS, LONG_PROMPT,
    MEASURED_RUNS, SHORT_MAX_TOKENS, SHORT_PROMPT,
};
pub use parse::{parse_plan, ParseError};
pub use plan::{
    config_fingerprint, ApplyOutcome, Confidence, Decision, ExcludedItem, Plan, PlanItem, PlanKind,
};
pub use probe::{probe_hardware, HardwareProbe};
pub use validate::{validate_plan, ItemValidation, LiveSnapshot, ValidationStatus};
