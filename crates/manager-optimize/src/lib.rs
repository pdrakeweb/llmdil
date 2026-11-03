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
pub mod tuning;
pub mod validate;

pub use context::{
    build_context, build_prompts, consult_default, default_purpose_seed, estimate_tokens,
    model_short_name, resolve_model, BackendMeasured, ConsultCandidate, ContextMeta,
    ExperimentResultFact, GpuContext, HardwareContext, MeasureInputs, ThroughputFact,
    TrafficWindow,
};
pub use experiment::{
    apply_experiment_decision, experiment_fingerprint, experiment_result_fact, fit_verdict_text,
    is_hf_form, is_temp_backend_id, lowest_free_port, resolve_candidate, temp_backend_config,
    temp_backend_id, validate_experiment, validate_experiment_for_plan, ActiveExperiment,
    CandidateResolution, ExperimentEstimate, ExperimentFlags, ExperimentJournal,
    ExperimentMeasured, ExperimentPredicted, ExperimentProposal, ExperimentProvisioning,
    ExperimentResult, ExperimentStatus, ExperimentValidation, RegisteredModelFile,
    BATCH_TIMEOUT_SECS, EXPERIMENT_TIMEOUT_SECS, MAX_EXPERIMENTS, TEMP_BACKEND_ID_PREFIX,
};
pub use discover::{
    candidates_from_model, collapse_candidates, family_filter_keeps, family_tokens_for_terms,
    fit_verdict, id_slug_from_file, is_stale, lookup_repo, merge_pool_hits, params_b_rounded,
    parse_link_next, parse_model_record, parse_rate_limit_remaining, parse_search_results,
    pool_order, quant_files, quant_token_from_file_name, query_variants_for_directive,
    rank_and_admit, search_layer_filter, search_query_for_directive, search_terms_for_directive,
    seat_for_directive, shortlist_is_full, CandidateFact, DeepDiscovery, DiscoverError,
    DiscoveryFunnel, HfClient, HfModelFacts, HfSearchHit, HfSibling, LookupQuant, LookupRepo,
    PoolRepo, QuantFile, SearchPage, DISCOVERY_CONCURRENCY, DISCOVERY_DEADLINE,
    DISCOVERY_REQUEST_TIMEOUT, EARLY_EXIT_MIN_REPOS, FAMILY_TOKENS, FIT_DOES_NOT_FIT, FIT_FITS,
    FIT_FITS_WITH_SPLIT, FIT_UNCHECKED, HF_DEFAULT_BASE_URL, HF_HEADER_RANGE_CAP_BYTES,
    HF_HEADER_RANGE_INITIAL_BYTES, MAX_CANDIDATES_PER_REPO, MAX_LOOKUP_REPOS, MAX_PAGES_PER_VARIANT,
    MAX_POOL_REPOS, MAX_QUERY_TERMS, MAX_QUERY_VARIANTS, MAX_RECORD_FETCHES, MAX_SEARCH_REPOS,
    MIN_POOL_DOWNLOADS, SEARCH_PAGE_LIMIT, SHORTLIST_CAP, STALE_DAYS, STALE_DOWNLOADS,
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
    benchmark_prompts, estimate_prompts, facts as measurement_facts, headline_figures,
    measurement_fingerprint, median, normalize_raw_flags, BenchResult, BenchRun, BenchmarkPrompt,
    EstimateEntry, HeadlineFigures, MeasurementEntry, MeasurementFingerprintInputs,
    MeasurementInputs, MeasurementSource, MeasurementStore, BENCHMARK_TEMPERATURE,
    ESTIMATE_LONG_MAX_TOKENS, ESTIMATE_RUNS, ESTIMATE_SHORT_MAX_TOKENS, LONG_MAX_TOKENS, LONG_PROMPT,
    MEASURED_RUNS, SHORT_MAX_TOKENS, SHORT_PROMPT,
};
pub use parse::{parse_plan, reground_experiment_provisioning, ParseError};
pub use plan::{
    config_fingerprint, ApplyOutcome, Confidence, Decision, ExcludedItem, Plan, PlanItem, PlanKind,
};
pub use probe::{probe_hardware, HardwareProbe};
pub use tuning::{
    adopt_is_default, cache_fa_offered, cache_ladder, context_cache_values, context_values,
    moe_cache_values, offload_values, placement_values, plan_filter, planned_arm_count,
    rank_arms, slot_values, spec_values, thermal_tripped, tuning_temp_backend, winner_index,
    ArmPricing, ArmStatus, Descent, ResidencyVerdict, SweepPlan, SweepState, TuningArm,
    TuningConfirmation, TuningCoords, TuningExclusion, TuningGpu, TuningInputs, TuningStore,
    TuningSweep, AXIS_ORDER, THERMAL_ABORT_C, THERMAL_ABORT_STREAK, TUNING_ARM_TIMEOUT_SECS,
    TUNING_MAX_ARMS, TUNING_SWEEP_TIMEOUT_SECS,
};
pub use validate::{validate_plan, ItemValidation, LiveSnapshot, ValidationStatus};
