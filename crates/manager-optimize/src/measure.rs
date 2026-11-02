//! Measured throughput (design addendum A.1, tracker #28
//! Phase 5a): the standardized benchmark definition, the
//! config fingerprint measurements are keyed by, and the
//! persistent measurement store.
//!
//! Numbers in this module come only from benchmark runs the app
//! itself performed against a backend (llama.cpp's `timings`),
//! never from catalog data or estimates — the context builder
//! labels them `"basis": "measured"`, the strongest label in
//! the app's discipline, and that label has to be earned here.

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use manager_config::SplitMode;
use manager_models::ExpertOffload;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::context::ThroughputFact;

// ---------------------------------------------------------------------------
// The standardized benchmark (§A.1)
// ---------------------------------------------------------------------------

/// Generation budget for the short prompt (§A.1).
pub const SHORT_MAX_TOKENS: u32 = 256;
/// Generation budget for the long prompt (§A.1).
pub const LONG_MAX_TOKENS: u32 = 128;
/// Benchmarks run at temperature 0, like everything measured in
/// this feature (§A.1).
pub const BENCHMARK_TEMPERATURE: f32 = 0.0;
/// Measured runs per prompt; one warm-up per prompt runs first
/// and is discarded (§A.1).
pub const MEASURED_RUNS: usize = 3;

/// The short prompt (~200 tokens at the app's rough
/// 4-characters-per-token rule): a fixed paragraph plus a
/// question. Fixed text, so numbers are comparable across
/// configurations and over time.
pub const SHORT_PROMPT: &str = concat!(
    "The harbor at dawn is quiet except for the soft clink of halyards against aluminum masts ",
    "and the low murmur of a diesel heater in a nearby cabin. A sailor checking the weather will ",
    "look first at the barometer, then at the sky, because instruments and observation correct ",
    "each other. The wind over open water behaves differently from the wind between hills, and ",
    "a forecast written for a town ten miles inland can mislead a crew leaving the breakwater. ",
    "Good preparation is mostly unglamorous: charged batteries, inspected rigging, a filed float ",
    "plan, and enough local knowledge to know which channel silts first after a storm. None of ",
    "this guarantees a calm passage, but it changes luck from a plan into a margin. ",
    "Explain in a detailed, organized answer why preparation matters more than luck for a ",
    "short coastal passage, and give several concrete examples?"
);

/// The long prompt (~4,000 tokens): fixed filler — the same
/// station-ledger paragraph repeated — with the question at the
/// end, so a run exercises prompt processing and the KV cache
/// at depth before generating.
pub const LONG_PROMPT: &str = concat!(
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
        "The valley station recorded the morning in its ordinary way: wind from the northwest at twelve knots, a thin layer of cloud over the ridge, and the river running clear and cold below the gauge. The keeper entered each reading in the ledger, checked the instruments against the reference set, noted two minor discrepancies for the maintenance crew, and filed the sheet with the previous season's records before beginning the next round of observations. ",
    "After reading all of the station records above, state the recurring pattern in the ",
    "observations, explain in detail why a careful keeper records minor discrepancies ",
    "instead of ignoring them, and say what practical conclusions a maintenance crew ",
    "should draw from the season's records?"
);

/// One benchmark prompt and its generation budget.
#[derive(Debug, Clone, Copy)]
pub struct BenchmarkPrompt {
    /// "short" / "long" — the store's field names.
    pub name: &'static str,
    pub text: &'static str,
    pub max_tokens: u32,
}

/// The two standardized prompts, in run order.
pub fn benchmark_prompts() -> [BenchmarkPrompt; 2] {
    [
        BenchmarkPrompt {
            name: "short",
            text: SHORT_PROMPT,
            max_tokens: SHORT_MAX_TOKENS,
        },
        BenchmarkPrompt {
            name: "long",
            text: LONG_PROMPT,
            max_tokens: LONG_MAX_TOKENS,
        },
    ]
}

// ---------------------------------------------------------------------------
// The estimate profile (tracker #56): the abbreviated benchmark
// ---------------------------------------------------------------------------

/// Generation budget for the short prompt in an estimate run:
/// the short prompt's job is the decode rate, which is at
/// steady state well inside 128 generated tokens — half the
/// standard budget still leaves a stable timings sample.
pub const ESTIMATE_SHORT_MAX_TOKENS: u32 = 128;
/// Generation budget for the long prompt in an estimate run:
/// the long prompt's job is prompt processing at depth (its
/// ~4,000-token prefill IS the sample), so generation is cut
/// to the minimum that still yields llama.cpp timings and a
/// TTFT figure.
pub const ESTIMATE_LONG_MAX_TOKENS: u32 = 16;
/// Runs per prompt in an estimate: one, with no separate
/// warm-up. An estimate is a directional figure, not a median;
/// finalists are confirmed with the full benchmark.
pub const ESTIMATE_RUNS: usize = 1;

/// The two standardized prompts at the estimate profile's
/// generation budgets: the same texts as [`benchmark_prompts`]
/// (prompt processing at depth must come from the same long
/// prompt), one run each, no warm-up.
///
/// Wall time, against a full measurement's (warm-up + 3 runs)
/// × both prompts: the short run costs ≈ half (half the
/// generation budget, prefill unchanged) and the long run
/// ≈ half (the same 4,000-token prefill, near-zero
/// generation), and each runs once instead of four times —
/// ≈13% of a measurement's wall time in total.
pub fn estimate_prompts() -> [BenchmarkPrompt; 2] {
    [
        BenchmarkPrompt {
            name: "short",
            text: SHORT_PROMPT,
            max_tokens: ESTIMATE_SHORT_MAX_TOKENS,
        },
        BenchmarkPrompt {
            name: "long",
            text: LONG_PROMPT,
            max_tokens: ESTIMATE_LONG_MAX_TOKENS,
        },
    ]
}

// ---------------------------------------------------------------------------
// Median
// ---------------------------------------------------------------------------

/// The median of a set of figures: the middle value for an odd
/// count, the mean of the two middle values for an even count.
/// `None` for an empty (or all-non-finite) input.
pub fn median(values: &[f64]) -> Option<f64> {
    let mut v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = v.len() / 2;
    if v.len() % 2 == 1 {
        Some(v[mid])
    } else {
        Some((v[mid - 1] + v[mid]) / 2.0)
    }
}

fn median_of(runs: &[BenchRun], pick: impl Fn(&BenchRun) -> Option<f64>) -> Option<f64> {
    let values: Vec<f64> = runs.iter().filter_map(pick).collect();
    median(&values)
}

// ---------------------------------------------------------------------------
// Measurement store (§A.1 persistence)
// ---------------------------------------------------------------------------

/// One measured run of one benchmark prompt. Every run is
/// stored, not just the median (§A.1), so the spread behind a
/// figure stays inspectable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchRun {
    pub prompt_per_second: Option<f64>,
    pub predicted_per_second: Option<f64>,
    pub ttft_ms: Option<f64>,
    pub latency_ms: u64,
}

/// One prompt's stored result: the median rates and TTFT (the
/// figures consultations and the page quote) plus every
/// per-run sample they were taken from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchResult {
    pub prompt_per_second: Option<f64>,
    pub predicted_per_second: Option<f64>,
    pub ttft_ms: Option<f64>,
    pub runs: Vec<BenchRun>,
}

impl BenchResult {
    /// The stored figure is the median of the measured runs
    /// (§A.1), computed per metric over the runs that reported
    /// it.
    pub fn from_runs(runs: Vec<BenchRun>) -> Self {
        Self {
            prompt_per_second: median_of(&runs, |r| r.prompt_per_second),
            predicted_per_second: median_of(&runs, |r| r.predicted_per_second),
            ttft_ms: median_of(&runs, |r| r.ttft_ms),
            runs,
        }
    }
}

/// How a measurement was produced (predicted-throughput
/// design §4.2). `Live` is reserved: no writer produces live
/// entries in M1/M2 (decision D7) — the value exists so the
/// on-disk format never needs a v3 for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeasurementSource {
    /// The operator's Measure action (the standardized
    /// benchmark).
    Benchmark,
    /// An Optimize experiment run.
    Experiment,
    /// A live gateway observation (reserved, unwritten).
    Live,
}

/// The readable fingerprint inputs an entry was measured
/// under (design §4.3): the exact values fed to
/// [`measurement_fingerprint`], stored readably — the offload
/// as its resolved label, the raw flags as the normalized
/// string that was hashed, the GPU set as the sorted
/// `index:name` list. Together with the entry's `model_id` and
/// its hash-or-size identity this is everything the near-match
/// diff (§5.3) compares.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeasurementInputs {
    pub n_ctx: u32,
    pub n_batch: u32,
    /// Resolved offload label: `none` / `all_experts` /
    /// `cpu_moe_layers:N`.
    pub offload: String,
    pub tensor_split: Vec<f32>,
    /// Split mode label: `none` / `layer` / `row`.
    pub split_mode: String,
    /// The normalized raw-flags string that was hashed.
    pub raw_flags: String,
    /// Sorted `index:name` GPU list.
    pub gpus: Vec<String>,
}

impl MeasurementInputs {
    /// The snapshot of a resolved fingerprint input set — the
    /// same values [`measurement_fingerprint`] hashes, in
    /// readable form.
    pub fn from_fingerprint_inputs(inputs: &MeasurementFingerprintInputs) -> Self {
        let mut gpus = inputs.gpus.clone();
        gpus.sort_by_key(|(index, _)| *index);
        Self {
            n_ctx: inputs.n_ctx,
            n_batch: inputs.n_batch,
            offload: offload_label(inputs.offload),
            tensor_split: inputs.tensor_split.clone(),
            split_mode: split_mode_label(inputs.split_mode).to_string(),
            raw_flags: normalize_raw_flags(inputs.raw_flags.as_deref()),
            gpus: gpus
                .iter()
                .map(|(index, name)| format!("{index}:{name}"))
                .collect(),
        }
    }

    /// The snapshot fields in which `self` (the configuration
    /// in question) differs from `other` (a stored entry), in
    /// the design §5.3 order, named as the surfaces name them.
    pub fn diff_fields(&self, other: &MeasurementInputs) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::new();
        if self.n_ctx != other.n_ctx {
            out.push("context");
        }
        if self.n_batch != other.n_batch {
            out.push("batch");
        }
        if self.offload != other.offload {
            out.push("offload");
        }
        if self.tensor_split != other.tensor_split {
            out.push("tensor split");
        }
        if self.split_mode != other.split_mode {
            out.push("split mode");
        }
        if self.raw_flags != other.raw_flags {
            out.push("raw flags");
        }
        if self.gpus != other.gpus {
            out.push("GPU set");
        }
        out
    }
}

/// One backend configuration's measurement, keyed in the store
/// by its config fingerprint (§A.1). The v2 fields (design
/// §4.2) are all optional on read, so a v1 entry loads without
/// them; the writers populate them from M1 on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeasurementEntry {
    pub fingerprint: String,
    pub backend_id: String,
    pub model_id: String,
    /// RFC 3339 timestamp of the measurement.
    pub measured_at: String,
    pub app_version: String,
    pub short: BenchResult,
    pub long: BenchResult,
    /// How the measurement was produced; absent on v1 entries
    /// (their provenance was not recorded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<MeasurementSource>,
    /// For experiment entries: the real backend the experiment
    /// configuration derived from, when known. `backend_id`
    /// keeps the temp id as literal provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_backend_id: Option<String>,
    /// The model file name, for history display without
    /// resolving the catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_file: Option<String>,
    /// The verified SHA-256 when the fingerprint used it;
    /// absent when the fingerprint fell back to file size, in
    /// which case `model_size_bytes` is recorded instead.
    /// Exactly one of the two is present on a v2 entry — the
    /// identity the fingerprint actually used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_size_bytes: Option<u64>,
    /// The readable fingerprint inputs (§4.3); absent on v1
    /// entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<MeasurementInputs>,
    /// GPU driver version(s) of the fingerprint's GPU set at
    /// measure time, from the telemetry snapshot,
    /// deduplicated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub driver_version: Vec<String>,
    /// The llama-server build string reported by the server at
    /// benchmark time, when obtainable; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llama_build: Option<String>,
    /// Residency verdict under which the figure was measured
    /// (Addendum B extension E4): `verified_exclusive` or
    /// `unverified`, recorded by tuning-sweep arms so a figure
    /// surfaced later carries the conditions it was measured
    /// under, not just the configuration. Absent on entries no
    /// sweep produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub residency: Option<String>,
    /// Peak GPU temperature (°C) observed during the
    /// measurement (Addendum B extension E4); absent when no
    /// temperature was sampled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_temp_c: Option<f32>,
}

/// One backend configuration's ESTIMATE (tracker #56): the
/// abbreviated benchmark's figures, stored alongside — never
/// inside — the measurements. An estimate carries its own,
/// weaker basis: it never replaces a measurement under the
/// same fingerprint, and [`facts`] never surfaces one (the
/// facts are the consultation's ground truth and are built
/// from `entries` only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EstimateEntry {
    pub fingerprint: String,
    pub backend_id: String,
    pub model_id: String,
    /// RFC 3339 timestamp of the estimate.
    pub estimated_at: String,
    pub app_version: String,
    pub short: BenchResult,
    pub long: BenchResult,
}

/// The store file's format version (design §4.1). Version 1
/// files carry no `version` field; the loader accepts that
/// shape and migrates in memory, and the next write persists
/// v2.
pub const STORE_FORMAT_VERSION: u32 = 2;

/// The on-disk store (`<data_dir>/optimize-measurements.json`):
/// `{ "version": 2, "entries": [...], "estimates": [...] }`.
/// Entries are append-only (design §4.5): a re-measurement of
/// the same fingerprint adds an entry and [`Self::lookup`]
/// takes the newest, so the full history stays available to
/// the history view and to per-entry delete. Old entries are
/// the delta history a follow-up consultation reasons over,
/// and surface as superseded once their fingerprint no longer
/// matches the backend's configuration. Estimates live in
/// their own list under the same fingerprint key, with a
/// separate basis (see [`EstimateEntry`]); the v1 -> v2
/// migration preserves them intact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeasurementStore {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<MeasurementEntry>,
    #[serde(default)]
    pub estimates: Vec<EstimateEntry>,
}

impl Default for MeasurementStore {
    fn default() -> Self {
        Self {
            version: STORE_FORMAT_VERSION,
            entries: Vec::new(),
            estimates: Vec::new(),
        }
    }
}

/// Precedence among entries competing for one lookup key
/// (design §5.2): on a `measured_at` tie, benchmark outranks
/// experiment; a v1 entry's unrecorded source ranks last.
fn source_rank(source: Option<MeasurementSource>) -> u8 {
    match source {
        Some(MeasurementSource::Benchmark) => 3,
        Some(MeasurementSource::Experiment) => 2,
        Some(MeasurementSource::Live) => 1,
        None => 0,
    }
}

/// The ordering key [`MeasurementStore::lookup`] maximizes: a
/// parseable `measured_at` (unparseable sorts oldest), then
/// the source precedence.
fn winner_key(entry: &MeasurementEntry) -> (DateTime<Utc>, u8) {
    let at = DateTime::parse_from_rfc3339(&entry.measured_at)
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
    (at, source_rank(entry.source))
}

impl MeasurementStore {
    /// Load the store at `path`. A missing or unparseable file
    /// is an empty store, matching the plan file's precedent:
    /// measurements are re-takeable, never load-bearing. An
    /// unversioned (v1) file migrates in memory: its entries
    /// load with the v2 fields absent and its estimates list
    /// intact; the next save persists the v2 shape.
    pub fn load(path: &Path) -> Self {
        let mut store: Self = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        store.version = STORE_FORMAT_VERSION;
        store
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut out = self.clone();
        out.version = STORE_FORMAT_VERSION;
        let text = serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| e.to_string())
    }

    /// Append an entry (design §4.5). Nothing is replaced: a
    /// re-measurement of the same fingerprint stands alongside
    /// the earlier one, and lookup takes the newest.
    pub fn upsert(&mut self, entry: MeasurementEntry) {
        self.entries.push(entry);
    }

    /// The winning entry for a fingerprint, across ALL entries
    /// regardless of `backend_id` (design §5.2): provenance no
    /// longer gates surfacing — an experiment that benchmarked
    /// configuration X is a measurement of X, whoever later
    /// runs X. Among entries with the same fingerprint the
    /// newest `measured_at` wins; on a timestamp tie benchmark
    /// outranks experiment; on a full tie the later entry in
    /// the file wins.
    pub fn lookup(&self, fingerprint: &str) -> Option<&MeasurementEntry> {
        self.entries
            .iter()
            .filter(|e| e.fingerprint == fingerprint)
            .max_by(|a, b| winner_key(a).cmp(&winner_key(b)))
    }

    /// The current (non-superseded) entry for one backend, given
    /// its current fingerprint: the newest of the backend's own
    /// entries under that fingerprint, under the same winner
    /// ordering as [`Self::lookup`].
    pub fn current(&self, backend_id: &str, fingerprint: &str) -> Option<&MeasurementEntry> {
        self.entries
            .iter()
            .filter(|e| e.backend_id == backend_id && e.fingerprint == fingerprint)
            .max_by(|a, b| winner_key(a).cmp(&winner_key(b)))
    }

    /// Delete one entry, identified by (fingerprint,
    /// measured_at) — the per-entry delete the history view
    /// offers (decision D9). When several entries share the
    /// key, the one lookup would surface (the last in winner
    /// order) is removed. Returns whether an entry was removed.
    pub fn delete(&mut self, fingerprint: &str, measured_at: &str) -> bool {
        let idx = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.fingerprint == fingerprint && e.measured_at == measured_at)
            .max_by(|(_, a), (_, b)| winner_key(a).cmp(&winner_key(b)))
            .map(|(i, _)| i);
        match idx {
            Some(i) => {
                self.entries.remove(i);
                true
            }
            None => false,
        }
    }

    /// The nearest near match for a configuration (design
    /// §5.3): among entries that carry an `inputs` snapshot and
    /// share the configuration's model identity (same
    /// `model_id` and the same hash-or-size identity), the one
    /// whose snapshot differs from `target` in the fewest
    /// fields (tie: newest, under the lookup winner ordering),
    /// returned with the differing fields. It counts as near
    /// only when at most two fields differ; an exact match
    /// (zero differing fields) is not a near match — lookup
    /// owns it.
    pub fn near_match(
        &self,
        model_id: &str,
        model_sha256: Option<&str>,
        model_size_bytes: Option<u64>,
        target: &MeasurementInputs,
    ) -> Option<(&MeasurementEntry, Vec<&'static str>)> {
        let same_identity = |e: &MeasurementEntry| {
            if e.model_id != model_id {
                return false;
            }
            match (e.model_sha256.as_deref(), model_sha256) {
                (Some(a), Some(b)) => a == b,
                (None, None) => match (e.model_size_bytes, model_size_bytes) {
                    (Some(a), Some(b)) => a == b,
                    _ => false,
                },
                _ => false,
            }
        };
        let mut best: Option<(&MeasurementEntry, Vec<&'static str>)> = None;
        for entry in &self.entries {
            let Some(inputs) = &entry.inputs else {
                continue;
            };
            if !same_identity(entry) {
                continue;
            }
            let diffs = target.diff_fields(inputs);
            if diffs.is_empty() || diffs.len() > 2 {
                continue;
            }
            let replace = match &best {
                None => true,
                Some((b, bd)) => {
                    diffs.len() < bd.len()
                        || (diffs.len() == bd.len()
                            && winner_key(entry) >= winner_key(b))
                }
            };
            if replace {
                best = Some((entry, diffs));
            }
        }
        best
    }

    /// Insert an estimate, replacing any existing ESTIMATE
    /// under the same fingerprint. Measurements are never
    /// touched: a full measurement of the same configuration
    /// stands alongside the estimate, not over it, and an
    /// estimate never displaces one.
    pub fn upsert_estimate(&mut self, entry: EstimateEntry) {
        if let Some(existing) = self
            .estimates
            .iter_mut()
            .find(|e| e.fingerprint == entry.fingerprint)
        {
            *existing = entry;
        } else {
            self.estimates.push(entry);
        }
    }

    /// The estimate for one backend under its current
    /// fingerprint, when one exists.
    pub fn current_estimate(
        &self,
        backend_id: &str,
        fingerprint: &str,
    ) -> Option<&EstimateEntry> {
        self.estimates
            .iter()
            .find(|e| e.backend_id == backend_id && e.fingerprint == fingerprint)
    }
}

// ---------------------------------------------------------------------------
// The measurement fingerprint (§A.1)
// ---------------------------------------------------------------------------

/// Everything the fingerprint hashes over, resolved by the
/// caller (the src-tauri glue) from the live configuration.
/// A plain struct on purpose: this crate stays free of the
/// app's I/O and telemetry types.
#[derive(Debug, Clone)]
pub struct MeasurementFingerprintInputs {
    /// The model's verified SHA-256 stamp
    /// (`ModelConfig::verified_sha256`) when one exists; the
    /// file identity then needs nothing else.
    pub verified_sha256: Option<String>,
    /// The model file's size on disk — the file identity when
    /// no verified stamp exists.
    pub file_size_bytes: Option<u64>,
    pub quant: String,
    pub n_ctx: u32,
    pub n_batch: u32,
    /// The resolved expert offload (from the backend's raw
    /// flags via `manager_models::expert_offload`, with the
    /// catalog default applied by the glue where the launch
    /// path applies one). Kept as its own field alongside the
    /// normalized raw flags, per §A.1's field list, even though
    /// offload is also spelled inside those flags.
    pub offload: ExpertOffload,
    pub tensor_split: Vec<f32>,
    pub split_mode: SplitMode,
    /// The backend's raw flags, verbatim; whitespace is
    /// collapsed before hashing.
    pub raw_flags: Option<String>,
    /// The GPU set the backend is placed on: (index, name)
    /// pairs. Names ride along so a GPU swap under the same
    /// indices still changes the fingerprint.
    pub gpus: Vec<(u32, String)>,
}

/// Raw flags with whitespace collapsed to single spaces (and
/// trimmed), so a purely cosmetic reformat of the same flags
/// hashes identically.
pub fn normalize_raw_flags(raw_flags: Option<&str>) -> String {
    raw_flags
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn offload_label(offload: ExpertOffload) -> String {
    match offload {
        ExpertOffload::None => "none".to_string(),
        ExpertOffload::AllExperts => "all_experts".to_string(),
        ExpertOffload::CpuMoeLayers(n) => format!("cpu_moe_layers:{n}"),
    }
}

fn split_mode_label(mode: SplitMode) -> &'static str {
    match mode {
        SplitMode::None => "none",
        SplitMode::Layer => "layer",
        SplitMode::Row => "row",
    }
}

/// The measurement fingerprint: a hash over the model's file
/// identity (verified sha256 when stamped, else file size),
/// quant, `n_ctx`, `n_batch`, resolved offload layers, tensor
/// split, split mode, normalized `raw_flags`, and the GPU set
/// (indices + names) — §A.1's field list, in that order. This
/// is a different function from `plan::config_fingerprint`
/// (the whole-config hash a plan's staleness check uses): a
/// configuration change that cannot affect speed (a purpose
/// edit, say) changes that hash but must not invalidate a
/// measurement, and a change that does affect speed must.
pub fn measurement_fingerprint(inputs: &MeasurementFingerprintInputs) -> String {
    let mut h = Sha256::new();
    let mut field = |label: &str, value: &str| {
        h.update(label.as_bytes());
        h.update(b"=");
        h.update(value.as_bytes());
        h.update(b"\n");
    };
    match inputs
        .verified_sha256
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(sha) => field("model_sha256", &sha.to_lowercase()),
        None => field(
            "model_file_size",
            &inputs.file_size_bytes.unwrap_or(0).to_string(),
        ),
    }
    field("quant", &inputs.quant);
    field("n_ctx", &inputs.n_ctx.to_string());
    field("n_batch", &inputs.n_batch.to_string());
    field("offload", &offload_label(inputs.offload));
    // Tensor split by bit pattern: exact, formatting-free.
    let split = inputs
        .tensor_split
        .iter()
        .map(|f| f.to_bits().to_string())
        .collect::<Vec<_>>()
        .join(",");
    field("tensor_split", &split);
    field("split_mode", split_mode_label(inputs.split_mode));
    field(
        "raw_flags",
        &normalize_raw_flags(inputs.raw_flags.as_deref()),
    );
    let mut gpus = inputs.gpus.clone();
    gpus.sort_by_key(|(index, _)| *index);
    let gpu_set = gpus
        .iter()
        .map(|(index, name)| format!("{index}:{name}"))
        .collect::<Vec<_>>()
        .join(",");
    field("gpus", &gpu_set);
    format!("{:x}", h.finalize())
}

// ---------------------------------------------------------------------------
// Into the context (§A.1)
// ---------------------------------------------------------------------------

/// An entry's headline figures, composed the one way every
/// surface composes them (design §5.2): prompt processing
/// from the long prompt (4,000 tokens is a real sample; 200 is
/// noise-dominated) and generation rate + TTFT from the short
/// prompt (the interactive-depth figure), each falling back to
/// the other prompt's figure when absent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeadlineFigures {
    pub prompt_per_second: Option<f64>,
    pub predicted_per_second: Option<f64>,
    pub ttft_ms: Option<f64>,
}

/// The headline figures of one entry (see [`HeadlineFigures`]).
pub fn headline_figures(entry: &MeasurementEntry) -> HeadlineFigures {
    HeadlineFigures {
        prompt_per_second: entry
            .long
            .prompt_per_second
            .or(entry.short.prompt_per_second),
        predicted_per_second: entry
            .short
            .predicted_per_second
            .or(entry.long.predicted_per_second),
        ttft_ms: entry.short.ttft_ms.or(entry.long.ttft_ms),
    }
}

/// Throughput facts for the context document: for each backend
/// in `current_fingerprints` (backend id -> its current
/// measurement fingerprint), the winning entry under the
/// current fingerprint (`superseded: false`) plus that
/// backend's older entries, labeled superseded — the delta
/// history a follow-up consultation reasons over ("the change
/// cost 2 tok/s"). Entries for backends absent from the map
/// are not surfaced. Estimates (`store.estimates`, tracker
/// #56) are NEVER surfaced here: this function reads `entries`
/// only, so an estimate can never reach the consultation
/// labeled as a measured fact.
///
/// The current fact comes from the fingerprint-only lookup
/// (design §5.2): the winner may be any backend's entry — an
/// experiment measurement of the same configuration feeds the
/// consultation exactly as the backend's own benchmark would.
/// The fact is labeled with the CONFIGURED backend's id, never
/// the winning entry's provenance id. Superseded facts remain
/// the backend's own entries under other fingerprints; its own
/// older entries under the current fingerprint are history
/// (the history view's material), not consultation facts.
pub fn facts(
    store: &MeasurementStore,
    current_fingerprints: &HashMap<String, String>,
    now: DateTime<Utc>,
) -> Vec<ThroughputFact> {
    let mut backend_ids: Vec<&String> = current_fingerprints.keys().collect();
    backend_ids.sort();
    let mut out = Vec::new();
    let push_fact = |out: &mut Vec<ThroughputFact>,
                         backend_id: &str,
                         entry: &MeasurementEntry,
                         superseded: bool| {
        let measured_at = DateTime::parse_from_rfc3339(&entry.measured_at)
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or(now);
        let age_secs = now.signed_duration_since(measured_at).num_seconds().max(0) as u64;
        let figures = headline_figures(entry);
        out.push(ThroughputFact {
            backend_id: backend_id.to_string(),
            config_fingerprint: entry.fingerprint.clone(),
            prompt_per_second: figures.prompt_per_second,
            predicted_per_second: figures.predicted_per_second,
            ttft_ms: figures.ttft_ms,
            age_secs,
            superseded,
        });
    };
    for backend_id in backend_ids {
        let current_fp = &current_fingerprints[backend_id];
        // Current first: the lookup winner for the backend's
        // current fingerprint, whatever its provenance.
        if let Some(winner) = store.lookup(current_fp) {
            push_fact(&mut out, backend_id, winner, false);
        }
        // Then the backend's own superseded entries, in store
        // order.
        for entry in store
            .entries
            .iter()
            .filter(|e| &e.backend_id == backend_id && &e.fingerprint != current_fp)
        {
            push_fact(&mut out, backend_id, entry, true);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_lengths_are_in_their_token_bands() {
        // ~4 chars per token: short ~200 tokens, long ~4,000.
        assert!(
            (600..=1_100).contains(&SHORT_PROMPT.len()),
            "short prompt is {} chars",
            SHORT_PROMPT.len()
        );
        assert!(
            (14_000..=18_000).contains(&LONG_PROMPT.len()),
            "long prompt is {} chars",
            LONG_PROMPT.len()
        );
        // Both prompts end with their question (§A.1's shape:
        // the long prompt's question is at the end).
        assert!(SHORT_PROMPT.ends_with('?'));
        assert!(LONG_PROMPT.ends_with('?'));
        assert_eq!(SHORT_MAX_TOKENS, 256);
        assert_eq!(LONG_MAX_TOKENS, 128);
        assert_eq!(benchmark_prompts()[0].text, SHORT_PROMPT);
        assert_eq!(benchmark_prompts()[1].text, LONG_PROMPT);
    }

    #[test]
    fn median_odd_even_and_empty() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[7.0]), Some(7.0));
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(median(&[f64::NAN, 5.0]), Some(5.0));
    }

    fn run(prompt: f64, predicted: f64, ttft: f64) -> BenchRun {
        BenchRun {
            prompt_per_second: Some(prompt),
            predicted_per_second: Some(predicted),
            ttft_ms: Some(ttft),
            latency_ms: 1000,
        }
    }

    #[test]
    fn bench_result_takes_the_median_of_its_runs() {
        let result = BenchResult::from_runs(vec![
            run(100.0, 10.0, 300.0),
            run(300.0, 30.0, 100.0),
            run(200.0, 20.0, 200.0),
        ]);
        assert_eq!(result.prompt_per_second, Some(200.0));
        assert_eq!(result.predicted_per_second, Some(20.0));
        assert_eq!(result.ttft_ms, Some(200.0));
        assert_eq!(result.runs.len(), 3, "per-run numbers are stored");
    }

    fn inputs() -> MeasurementFingerprintInputs {
        MeasurementFingerprintInputs {
            verified_sha256: Some("AB".repeat(32)),
            file_size_bytes: Some(1234),
            quant: "Q4_K_M".to_string(),
            n_ctx: 32768,
            n_batch: 512,
            offload: ExpertOffload::CpuMoeLayers(46),
            tensor_split: vec![0.5, 0.5],
            split_mode: SplitMode::Layer,
            raw_flags: Some("--n-cpu-moe   46".to_string()),
            gpus: vec![(1, "Tesla P100".to_string()), (0, "RTX A4000".to_string())],
        }
    }

    #[test]
    fn fingerprint_is_stable_and_whitespace_insensitive() {
        let a = inputs();
        let mut b = inputs();
        // GPU order and raw-flag whitespace are presentation,
        // not configuration.
        b.gpus.reverse();
        b.raw_flags = Some("--n-cpu-moe 46".to_string());
        assert_eq!(measurement_fingerprint(&a), measurement_fingerprint(&b));
        assert_eq!(
            measurement_fingerprint(&a),
            measurement_fingerprint(&inputs())
        );
    }

    #[test]
    fn fingerprint_changes_with_each_speed_input() {
        let base = measurement_fingerprint(&inputs());
        let changed = |mutate: &dyn Fn(&mut MeasurementFingerprintInputs)| {
            let mut i = inputs();
            mutate(&mut i);
            assert_ne!(measurement_fingerprint(&i), base);
        };
        changed(&|i| i.n_ctx = 65536);
        changed(&|i| i.n_batch = 1024);
        changed(&|i| i.quant = "IQ3_S".to_string());
        changed(&|i| i.offload = ExpertOffload::AllExperts);
        changed(&|i| i.tensor_split = vec![1.0, 0.0]);
        changed(&|i| i.split_mode = SplitMode::Row);
        changed(&|i| i.raw_flags = Some("--threads 8".to_string()));
        changed(&|i| i.gpus = vec![(0, "RTX A4000".to_string())]);
        changed(&|i| i.gpus[1].1 = "Tesla P40".to_string());
        changed(&|i| i.verified_sha256 = Some("cd".repeat(32)));
    }

    #[test]
    fn fingerprint_file_identity_prefers_the_verified_stamp() {
        // With a stamp, a size change alone does not move the
        // fingerprint — the verified hash IS the identity.
        let mut i = inputs();
        i.file_size_bytes = Some(9999);
        assert_eq!(
            measurement_fingerprint(&i),
            measurement_fingerprint(&inputs())
        );
        // Without a stamp, the size is the identity.
        let mut a = inputs();
        a.verified_sha256 = None;
        let mut b = a.clone();
        b.file_size_bytes = Some(9999);
        assert_ne!(measurement_fingerprint(&a), measurement_fingerprint(&b));
    }

    fn entry(fingerprint: &str, backend: &str, at: &str) -> MeasurementEntry {
        MeasurementEntry {
            fingerprint: fingerprint.to_string(),
            backend_id: backend.to_string(),
            model_id: "m1".to_string(),
            measured_at: at.to_string(),
            app_version: "0.1.33".to_string(),
            short: BenchResult::from_runs(vec![
                run(200.0, 20.0, 200.0),
                run(220.0, 22.0, 220.0),
                run(210.0, 21.0, 210.0),
            ]),
            long: BenchResult::from_runs(vec![
                run(800.0, 18.0, 900.0),
                run(820.0, 19.0, 920.0),
                run(810.0, 17.0, 910.0),
            ]),
            source: None,
            origin_backend_id: None,
            model_file: None,
            model_sha256: None,
            model_size_bytes: None,
            inputs: None,
            driver_version: Vec::new(),
            llama_build: None,
            residency: None,
            peak_temp_c: None,
        }
    }

    #[test]
    fn store_round_trips_and_upsert_appends_by_fingerprint() {
        let dir = std::env::temp_dir().join(format!(
            "manager-optimize-measure-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("optimize-measurements.json");
        // Absent file = empty store.
        assert!(MeasurementStore::load(&path).entries.is_empty());
        let mut store = MeasurementStore::default();
        store.upsert(entry("fp1", "planner", "2026-10-17T12:00:00Z"));
        store.upsert(entry("fp2", "planner", "2026-10-17T12:00:00Z"));
        assert_eq!(store.entries.len(), 2);
        // Same fingerprint: appended, not replaced (design
        // §4.5) — lookup and current take the newest.
        let mut newer = entry("fp1", "planner", "2026-10-17T13:00:00Z");
        newer.app_version = "0.1.34".to_string();
        store.upsert(newer);
        assert_eq!(store.entries.len(), 3);
        store.save(&path).unwrap();
        let loaded = MeasurementStore::load(&path);
        assert_eq!(loaded, store);
        assert_eq!(
            loaded.current("planner", "fp1").unwrap().app_version,
            "0.1.34"
        );
        assert_eq!(
            loaded.lookup("fp1").unwrap().app_version,
            "0.1.34"
        );
        assert!(loaded.current("planner", "nope").is_none());
        assert!(loaded.lookup("nope").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v1_store_file_migrates_with_measurements_and_estimates_intact() {
        // A v1 file: no `version` field, entries without the
        // v2 fields, and a #56 estimates list. The migration
        // evidence for the v2 rollout: both lists survive the
        // load, the v2 fields read as absent, and the next
        // save persists the v2 shape with both lists.
        let v1 = r#"{
  "entries": [
    {
      "fingerprint": "fp-a",
      "backend_id": "planner",
      "model_id": "m1",
      "measured_at": "2026-10-17T12:00:00Z",
      "app_version": "0.1.40",
      "short": { "prompt_per_second": 210.0, "predicted_per_second": 21.0, "ttft_ms": 210.0, "runs": [] },
      "long": { "prompt_per_second": 810.0, "predicted_per_second": 18.0, "ttft_ms": 910.0, "runs": [] }
    },
    {
      "fingerprint": "fp-b",
      "backend_id": "opt-exp-3",
      "model_id": "m1",
      "measured_at": "2026-10-17T13:00:00Z",
      "app_version": "0.1.40",
      "short": { "prompt_per_second": 190.0, "predicted_per_second": 19.0, "ttft_ms": 230.0, "runs": [] },
      "long": { "prompt_per_second": 790.0, "predicted_per_second": 17.0, "ttft_ms": 940.0, "runs": [] }
    }
  ],
  "estimates": [
    {
      "fingerprint": "fp-a",
      "backend_id": "planner",
      "model_id": "m1",
      "estimated_at": "2026-10-17T12:30:00Z",
      "app_version": "0.1.40",
      "short": { "prompt_per_second": 111.0, "predicted_per_second": 99.0, "ttft_ms": 111.0, "runs": [] },
      "long": { "prompt_per_second": 111.0, "predicted_per_second": 88.0, "ttft_ms": 111.0, "runs": [] }
    }
  ]
}"#;
        let dir = std::env::temp_dir().join(format!(
            "manager-optimize-migrate-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("optimize-measurements.json");
        std::fs::write(&path, v1).unwrap();
        let store = MeasurementStore::load(&path);
        // Migrated in memory to the current format version.
        assert_eq!(store.version, STORE_FORMAT_VERSION);
        // Measurements intact, v2 fields absent (not invented).
        assert_eq!(store.entries.len(), 2);
        let a = &store.entries[0];
        assert_eq!(a.fingerprint, "fp-a");
        assert_eq!(a.short.predicted_per_second, Some(21.0));
        assert_eq!(a.long.prompt_per_second, Some(810.0));
        assert_eq!(a.source, None);
        assert_eq!(a.origin_backend_id, None);
        assert_eq!(a.model_file, None);
        assert_eq!(a.model_sha256, None);
        assert_eq!(a.model_size_bytes, None);
        assert_eq!(a.inputs, None);
        assert!(a.driver_version.is_empty());
        assert_eq!(a.llama_build, None);
        // The experiment-provenance v1 entry is intact too.
        assert_eq!(store.entries[1].backend_id, "opt-exp-3");
        // Estimates intact (#56): the migration never touches
        // the estimates list.
        assert_eq!(store.estimates.len(), 1);
        assert_eq!(store.estimates[0].fingerprint, "fp-a");
        assert_eq!(
            store.estimates[0].short.predicted_per_second,
            Some(99.0)
        );
        // The next save persists v2 with both lists, and the
        // reloaded store equals the migrated one.
        store.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"version\": 2"), "saved file: {text}");
        assert!(text.contains("\"estimates\""), "saved file: {text}");
        assert_eq!(MeasurementStore::load(&path), store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lookup_is_fingerprint_only_and_newest_wins_with_source_tiebreak() {
        let mut store = MeasurementStore::default();
        // An experiment entry under a temp id and the backend's
        // own older benchmark under the same fingerprint:
        // lookup is by fingerprint alone and takes the newest,
        // whoever measured it.
        let mut bench = entry("fp1", "planner", "2026-10-17T12:00:00Z");
        bench.source = Some(MeasurementSource::Benchmark);
        store.upsert(bench);
        let mut exp = entry("fp1", "opt-exp-3", "2026-10-17T13:00:00Z");
        exp.source = Some(MeasurementSource::Experiment);
        store.upsert(exp);
        let winner = store.lookup("fp1").unwrap();
        assert_eq!(winner.backend_id, "opt-exp-3");
        assert_eq!(winner.source, Some(MeasurementSource::Experiment));
        // On a timestamp tie, benchmark outranks experiment.
        let mut bench2 = entry("fp1", "planner", "2026-10-17T13:00:00Z");
        bench2.source = Some(MeasurementSource::Benchmark);
        store.upsert(bench2);
        let winner = store.lookup("fp1").unwrap();
        assert_eq!(winner.backend_id, "planner");
        assert_eq!(winner.source, Some(MeasurementSource::Benchmark));
        // current() stays scoped to the backend's own entries.
        assert_eq!(
            store.current("opt-exp-3", "fp1").unwrap().measured_at,
            "2026-10-17T13:00:00Z"
        );
    }

    #[test]
    fn delete_removes_one_entry_by_fingerprint_and_timestamp() {
        let mut store = MeasurementStore::default();
        store.upsert(entry("fp1", "planner", "2026-10-17T12:00:00Z"));
        store.upsert(entry("fp1", "planner", "2026-10-17T13:00:00Z"));
        store.upsert(entry("fp2", "planner", "2026-10-17T13:00:00Z"));
        assert!(store.delete("fp1", "2026-10-17T13:00:00Z"));
        assert_eq!(store.entries.len(), 2);
        // The remaining fp1 entry is the older one.
        assert_eq!(
            store.lookup("fp1").unwrap().measured_at,
            "2026-10-17T12:00:00Z"
        );
        // A missing key deletes nothing and says so.
        assert!(!store.delete("fp1", "2026-10-17T13:00:00Z"));
        assert!(!store.delete("nope", "2026-10-17T13:00:00Z"));
        assert_eq!(store.entries.len(), 2);
    }

    fn snapshot(n_ctx: u32, n_batch: u32, gpus: &[&str]) -> MeasurementInputs {
        MeasurementInputs {
            n_ctx,
            n_batch,
            offload: "all_experts".to_string(),
            tensor_split: vec![1.0, 0.0],
            split_mode: "layer".to_string(),
            raw_flags: "--n-cpu-moe 46".to_string(),
            gpus: gpus.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn near_match_diffs_snapshot_fields_and_requires_same_model_identity() {
        let mut store = MeasurementStore::default();
        // Entry A: differs from the target in context only.
        let mut a = entry("fp-a", "planner", "2026-10-17T12:00:00Z");
        a.model_sha256 = Some("abc123".to_string());
        a.inputs = Some(snapshot(16384, 512, &["0:GPU A"]));
        store.upsert(a);
        // Entry B: differs in context, batch and GPU set —
        // three fields, past the near threshold.
        let mut b = entry("fp-b", "planner", "2026-10-17T13:00:00Z");
        b.model_sha256 = Some("abc123".to_string());
        b.inputs = Some(snapshot(16384, 256, &["1:GPU B"]));
        store.upsert(b);
        // Entry C: one differing field, but a different model
        // identity — never a near match.
        let mut c = entry("fp-c", "coder", "2026-10-17T14:00:00Z");
        c.model_sha256 = Some("def456".to_string());
        c.inputs = Some(snapshot(16384, 512, &["0:GPU A"]));
        store.upsert(c);
        // Entry D: same identity, one differing field, but no
        // snapshot (a v1-shaped entry) — not eligible.
        let mut d = entry("fp-d", "coder", "2026-10-17T15:00:00Z");
        d.model_sha256 = Some("abc123".to_string());
        store.upsert(d);
        let target = snapshot(32768, 512, &["0:GPU A"]);
        let (winner, diffs) = store
            .near_match("m1", Some("abc123"), None, &target)
            .expect("entry A is the near match");
        assert_eq!(winner.fingerprint, "fp-a");
        assert_eq!(diffs, vec!["context"]);
        // An exact snapshot is lookup's business, not a near
        // match: entry A (zero differing fields) is excluded,
        // and the nearest remaining candidate is B at two
        // differing fields.
        let (winner, diffs) = store
            .near_match("m1", Some("abc123"), None, &snapshot(16384, 512, &["0:GPU A"]))
            .expect("entry B is the near match for the exact-A target");
        assert_eq!(winner.fingerprint, "fp-b");
        assert_eq!(diffs, vec!["batch", "GPU set"]);
        // Size-identity entries match on size when neither side
        // used a hash.
        let mut e = entry("fp-e", "coder", "2026-10-17T16:00:00Z");
        e.model_sha256 = None;
        e.model_size_bytes = Some(12345);
        e.inputs = Some(snapshot(16384, 512, &["0:GPU A"]));
        store.upsert(e);
        let (winner, diffs) = store
            .near_match("m1", None, Some(12345), &target)
            .expect("entry E is the near match on size identity");
        assert_eq!(winner.fingerprint, "fp-e");
        assert_eq!(diffs, vec!["context"]);
    }

    #[test]
    fn estimate_profile_shape() {
        // One run per prompt, no warm-up (the run sequence is
        // the runner's; the profile pins the budgets and the
        // shared prompt texts).
        assert_eq!(ESTIMATE_RUNS, 1);
        assert_eq!(ESTIMATE_SHORT_MAX_TOKENS, 128);
        assert_eq!(ESTIMATE_LONG_MAX_TOKENS, 16);
        let est = estimate_prompts();
        let std = benchmark_prompts();
        // Same texts: prompt processing at depth must come from
        // the same long prompt, the decode sample from the same
        // short one — only the generation budgets shrink.
        assert_eq!(est[0].text, std[0].text);
        assert_eq!(est[1].text, std[1].text);
        assert_eq!(est[0].name, "short");
        assert_eq!(est[1].name, "long");
        assert_eq!(est[0].max_tokens, ESTIMATE_SHORT_MAX_TOKENS);
        assert_eq!(est[1].max_tokens, ESTIMATE_LONG_MAX_TOKENS);
        // The fraction the profile promises, in wall-clock
        // terms: time ≈ prefill tokens / prompt rate +
        // generation tokens / decode rate, at representative
        // rates (the planner figures used below: 810 tok/s
        // prompt, 21 tok/s generation), prefill tokens ≈
        // chars / 4. The estimate's total must land well under
        // a quarter of the measurement's (warm-up + 3 runs).
        let work = |prompts: &[BenchmarkPrompt; 2], runs: usize| -> f64 {
            prompts
                .iter()
                .map(|p| {
                    (p.text.len() as f64 / 4.0 / 810.0 + p.max_tokens as f64 / 21.0)
                        * runs as f64
                })
                .sum()
        };
        let ratio = work(&est, ESTIMATE_RUNS) / work(&std, MEASURED_RUNS + 1);
        assert!(
            ratio < 0.25,
            "estimate/measure work ratio {ratio:.3} must stay under 0.25"
        );
    }

    fn estimate_entry(fingerprint: &str, backend: &str, at: &str) -> EstimateEntry {
        EstimateEntry {
            fingerprint: fingerprint.to_string(),
            backend_id: backend.to_string(),
            model_id: "m1".to_string(),
            estimated_at: at.to_string(),
            app_version: "0.1.40".to_string(),
            // Deliberately far from the measured figures in
            // `entry()` so a mix-up is visible in assertions.
            short: BenchResult::from_runs(vec![run(111.0, 99.0, 111.0)]),
            long: BenchResult::from_runs(vec![run(111.0, 88.0, 111.0)]),
        }
    }

    #[test]
    fn estimates_never_displace_measurements() {
        let mut store = MeasurementStore::default();
        store.upsert(entry("fp1", "planner", "2026-10-17T12:00:00Z"));
        store.upsert_estimate(estimate_entry("fp1", "planner", "2026-10-17T12:30:00Z"));
        // Both live under the same fingerprint, each intact.
        assert_eq!(store.entries.len(), 1);
        assert_eq!(store.estimates.len(), 1);
        let measured = store.current("planner", "fp1").unwrap();
        assert_eq!(measured.short.predicted_per_second, Some(21.0));
        let estimate = store.current_estimate("planner", "fp1").unwrap();
        assert_eq!(estimate.short.predicted_per_second, Some(99.0));
        // A re-measurement appends a measurement only (design
        // §4.5); a re-estimate replaces the estimate only.
        let mut newer = entry("fp1", "planner", "2026-10-17T13:00:00Z");
        newer.app_version = "0.1.41".to_string();
        store.upsert(newer);
        assert_eq!(store.entries.len(), 2);
        assert_eq!(store.estimates.len(), 1);
        assert_eq!(
            store.current_estimate("planner", "fp1").unwrap().estimated_at,
            "2026-10-17T12:30:00Z"
        );
        store.upsert_estimate(estimate_entry("fp1", "planner", "2026-10-17T14:00:00Z"));
        assert_eq!(store.estimates.len(), 1);
        assert_eq!(
            store.current("planner", "fp1").unwrap().app_version,
            "0.1.41"
        );
        // Round trip: both lists persist.
        let dir = std::env::temp_dir().join(format!(
            "manager-optimize-estimate-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("optimize-measurements.json");
        store.save(&path).unwrap();
        assert_eq!(MeasurementStore::load(&path), store);
        let _ = std::fs::remove_dir_all(&dir);
        // A pre-#56 file (no "estimates" key) loads with an
        // empty estimate list and its entries intact.
        let legacy: MeasurementStore =
            serde_json::from_str(r#"{ "entries": [] }"#).unwrap();
        assert!(legacy.estimates.is_empty());
    }

    #[test]
    fn facts_never_surface_estimates() {
        let now = DateTime::parse_from_rfc3339("2026-10-17T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut current = HashMap::new();
        current.insert("planner".to_string(), "fp1".to_string());
        // An estimate alone produces NO fact: the consultation
        // must never see it labeled as measured.
        let mut store = MeasurementStore::default();
        store.upsert_estimate(estimate_entry("fp1", "planner", "2026-10-17T11:30:00Z"));
        assert!(facts(&store, &current, now).is_empty());
        // With a measurement under the same fingerprint, the
        // fact carries the MEASUREMENT's figures, not the
        // estimate's.
        store.upsert(entry("fp1", "planner", "2026-10-17T11:00:00Z"));
        let facts = facts(&store, &current, now);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].predicted_per_second, Some(21.0));
        assert_eq!(facts[0].prompt_per_second, Some(810.0));
        assert!(!facts[0].superseded);
    }

    #[test]
    fn facts_label_current_and_superseded_with_ages() {
        let mut store = MeasurementStore::default();
        store.upsert(entry("old-fp", "planner", "2026-10-17T10:00:00Z"));
        store.upsert(entry("cur-fp", "planner", "2026-10-17T11:30:00Z"));
        store.upsert(entry("other-fp", "ghost", "2026-10-17T11:00:00Z"));
        let mut current = HashMap::new();
        current.insert("planner".to_string(), "cur-fp".to_string());
        let now = DateTime::parse_from_rfc3339("2026-10-17T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let facts = facts(&store, &current, now);
        // The ghost backend is not in the current map: not surfaced.
        assert_eq!(facts.len(), 2);
        // Current first, headline figures composed per prompt:
        // prompt rate from the long runs' median, generation
        // and TTFT from the short runs' medians.
        assert_eq!(facts[0].config_fingerprint, "cur-fp");
        assert!(!facts[0].superseded);
        assert_eq!(facts[0].prompt_per_second, Some(810.0));
        assert_eq!(facts[0].predicted_per_second, Some(21.0));
        assert_eq!(facts[0].ttft_ms, Some(210.0));
        assert_eq!(facts[0].age_secs, 1800);
        // The older entry survives, labeled superseded.
        assert_eq!(facts[1].config_fingerprint, "old-fp");
        assert!(facts[1].superseded);
        assert_eq!(facts[1].age_secs, 7200);
    }

    #[test]
    fn facts_current_fact_comes_from_any_provenance() {
        // An experiment measurement of the planner's current
        // configuration (recorded under a temp backend id) is
        // the planner's current fact (design §5.2): labeled
        // with the configured backend's id, not the temp id.
        // The planner's own older entry under an old
        // fingerprint stays superseded, and its own older entry
        // under the CURRENT fingerprint does not duplicate the
        // current fact — the lookup winner stands alone.
        let mut store = MeasurementStore::default();
        let mut own_current = entry("cur-fp", "planner", "2026-10-17T10:00:00Z");
        own_current.source = Some(MeasurementSource::Benchmark);
        store.upsert(own_current);
        let mut exp = entry("cur-fp", "opt-exp-3", "2026-10-17T11:00:00Z");
        exp.source = Some(MeasurementSource::Experiment);
        // Distinct figures so the winner is identifiable:
        // median predicted 31, prompt 610.
        exp.short = BenchResult::from_runs(vec![run(300.0, 31.0, 150.0)]);
        exp.long = BenchResult::from_runs(vec![run(610.0, 29.0, 700.0)]);
        store.upsert(exp);
        store.upsert(entry("old-fp", "planner", "2026-10-17T09:00:00Z"));
        let mut current = HashMap::new();
        current.insert("planner".to_string(), "cur-fp".to_string());
        let now = DateTime::parse_from_rfc3339("2026-10-17T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let facts = facts(&store, &current, now);
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].backend_id, "planner");
        assert_eq!(facts[0].config_fingerprint, "cur-fp");
        assert!(!facts[0].superseded);
        assert_eq!(facts[0].predicted_per_second, Some(31.0));
        assert_eq!(facts[0].prompt_per_second, Some(610.0));
        assert_eq!(facts[1].config_fingerprint, "old-fp");
        assert!(facts[1].superseded);
    }
}
