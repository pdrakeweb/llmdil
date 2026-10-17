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

/// One backend configuration's measurement, keyed in the store
/// by its config fingerprint (§A.1).
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
}

/// The on-disk store (`<data_dir>/optimize-measurements.json`):
/// `{ "entries": [...] }`. Old entries are retained — they are
/// the delta history a follow-up consultation reasons over —
/// and surface as superseded once their fingerprint no longer
/// matches the backend's configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MeasurementStore {
    #[serde(default)]
    pub entries: Vec<MeasurementEntry>,
}

impl MeasurementStore {
    /// Load the store at `path`. A missing or unparseable file
    /// is an empty store, matching the plan file's precedent:
    /// measurements are re-takeable, never load-bearing.
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

    /// Insert an entry, replacing any existing entry under the
    /// same fingerprint (a re-measurement of the same
    /// configuration is the newer truth for that key).
    pub fn upsert(&mut self, entry: MeasurementEntry) {
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|e| e.fingerprint == entry.fingerprint)
        {
            *existing = entry;
        } else {
            self.entries.push(entry);
        }
    }

    /// The current (non-superseded) entry for one backend, given
    /// its current fingerprint.
    pub fn current(&self, backend_id: &str, fingerprint: &str) -> Option<&MeasurementEntry> {
        self.entries
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

/// Throughput facts for the context document: for each backend
/// in `current_fingerprints` (backend id -> its current
/// measurement fingerprint), the entry under the current
/// fingerprint (`superseded: false`) plus that backend's older
/// entries, labeled superseded — the delta history a follow-up
/// consultation reasons over ("the change cost 2 tok/s").
/// Entries for backends absent from the map are not surfaced.
///
/// The headline figures combine the two prompts by what each
/// measures best: prompt processing from the long prompt
/// (4,000 tokens is a real sample; 200 is noise-dominated) and
/// generation rate + TTFT from the short prompt (the
/// interactive-depth figure), each falling back to the other
/// prompt's figure when absent.
pub fn facts(
    store: &MeasurementStore,
    current_fingerprints: &HashMap<String, String>,
    now: DateTime<Utc>,
) -> Vec<ThroughputFact> {
    let mut backend_ids: Vec<&String> = current_fingerprints.keys().collect();
    backend_ids.sort();
    let mut out = Vec::new();
    for backend_id in backend_ids {
        let current_fp = &current_fingerprints[backend_id];
        let mut entries: Vec<&MeasurementEntry> = store
            .entries
            .iter()
            .filter(|e| &e.backend_id == backend_id)
            .collect();
        // Current first, then superseded in store order.
        entries.sort_by_key(|e| e.fingerprint != *current_fp);
        for entry in entries {
            let measured_at = DateTime::parse_from_rfc3339(&entry.measured_at)
                .map(|t| t.with_timezone(&Utc))
                .unwrap_or(now);
            let age_secs = now.signed_duration_since(measured_at).num_seconds().max(0) as u64;
            out.push(ThroughputFact {
                backend_id: entry.backend_id.clone(),
                config_fingerprint: entry.fingerprint.clone(),
                prompt_per_second: entry
                    .long
                    .prompt_per_second
                    .or(entry.short.prompt_per_second),
                predicted_per_second: entry
                    .short
                    .predicted_per_second
                    .or(entry.long.predicted_per_second),
                ttft_ms: entry.short.ttft_ms.or(entry.long.ttft_ms),
                age_secs,
                superseded: entry.fingerprint != *current_fp,
            });
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
        }
    }

    #[test]
    fn store_round_trips_and_upsert_replaces_by_fingerprint() {
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
        // Same fingerprint: replaced, not appended.
        let mut newer = entry("fp1", "planner", "2026-10-17T13:00:00Z");
        newer.app_version = "0.1.34".to_string();
        store.upsert(newer);
        assert_eq!(store.entries.len(), 2);
        store.save(&path).unwrap();
        let loaded = MeasurementStore::load(&path);
        assert_eq!(loaded, store);
        assert_eq!(
            loaded.current("planner", "fp1").unwrap().app_version,
            "0.1.34"
        );
        assert!(loaded.current("planner", "nope").is_none());
        let _ = std::fs::remove_dir_all(&dir);
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
}
