//! Measurement summaries and the results store (design §5.1):
//! per-run rows, per-arm summaries over the measured runs, one
//! record per sweep at `<data_dir>/bench-results.json`, and the
//! per-condition winner computation (§5.2).

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::arm::ArmSpec;
use crate::comparability::ModelIdentity;

/// Median of `values`, or `None` for an empty slice. For an even
/// count, the mean of the two middle values.
pub fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted: Vec<f64> = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        Some(sorted[mid])
    } else {
        Some((sorted[mid - 1] + sorted[mid]) / 2.0)
    }
}

fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        None
    } else {
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }
}

/// Per-drafter accepted/generated tokens for one run, from the
/// per-implementation statistics lines (kit §1.5).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DrafterAgg {
    pub kind: String,
    pub gen_tokens: u64,
    pub acc_tokens: u64,
}

/// One run of an arm: the warm-up run (flagged, never averaged in)
/// or one measured run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RunRow {
    #[serde(default)]
    pub warmup: bool,
    #[serde(default)]
    pub prompt_tps: Option<f64>,
    #[serde(default)]
    pub gen_tps: Option<f64>,
    #[serde(default)]
    pub ttft_ms: Option<f64>,
    #[serde(default)]
    pub prompt_tokens: Option<u32>,
    #[serde(default)]
    pub generated_tokens: Option<u32>,
    #[serde(default)]
    pub acceptance_rate: Option<f64>,
    #[serde(default)]
    pub accepted: Option<u64>,
    #[serde(default)]
    pub generated: Option<u64>,
    #[serde(default)]
    pub mean_accepted_len: Option<f64>,
    #[serde(default)]
    pub drafters: Vec<DrafterAgg>,
}

/// An arm's headline figures: medians over its measured runs
/// (D12 — one statistic for all arms, median-of-3), means for the
/// acceptance figures over the runs that carry them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ArmSummary {
    pub median_prompt_tps: Option<f64>,
    pub median_gen_tps: Option<f64>,
    pub median_ttft_ms: Option<f64>,
    pub mean_acceptance_rate: Option<f64>,
    pub mean_accepted_len: Option<f64>,
}

/// Summarize an arm's runs. Warm-up rows are excluded from every
/// figure; each median is taken over the runs that carry the value,
/// and each acceptance mean over the runs that carry it.
pub fn summarize(runs: &[RunRow]) -> ArmSummary {
    let measured: Vec<&RunRow> = runs.iter().filter(|r| !r.warmup).collect();
    let collect = |f: &dyn Fn(&RunRow) -> Option<f64>| -> Vec<f64> {
        measured.iter().filter_map(|r| f(r)).collect()
    };
    ArmSummary {
        median_prompt_tps: median(&collect(&|r| r.prompt_tps)),
        median_gen_tps: median(&collect(&|r| r.gen_tps)),
        median_ttft_ms: median(&collect(&|r| r.ttft_ms)),
        mean_acceptance_rate: mean(&collect(&|r| r.acceptance_rate)),
        mean_accepted_len: mean(&collect(&|r| r.mean_accepted_len)),
    }
}

/// An arm's lifecycle status in a sweep record (§5.1). Failed,
/// pruned, and preflight-rejected arms are statuses with reasons,
/// never zeroes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArmStatus {
    #[serde(rename = "pending")]
    #[default]
    Pending,
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "measured")]
    Measured,
    #[serde(rename = "pruned")]
    Pruned,
    #[serde(rename = "preflight_failed")]
    PreflightFailed,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "cancelled")]
    Cancelled,
}

/// One arm's stored record: its full configuration snapshot and
/// comparability key, the exact flags it launched with, its runs,
/// and its outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ArmRecord {
    pub index: usize,
    pub label: String,
    pub spec: ArmSpec,
    pub raw_flags: String,
    pub comparability_key: String,
    pub status: ArmStatus,
    /// Prune reason, preflight verdict, or error text, by status.
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub runs: Vec<RunRow>,
    #[serde(default)]
    pub summary: Option<ArmSummary>,
    #[serde(default)]
    pub peak_vram_mib: Option<u64>,
    #[serde(default)]
    pub started_ms: Option<i64>,
    #[serde(default)]
    pub ended_ms: Option<i64>,
}

/// The interlock record of a sweep (§6.2): which backends were
/// running when the sweep was confirmed, and which the operator
/// confirmed stopping. Bench never restarts them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct InterlockRecord {
    #[serde(default)]
    pub running_before: Vec<String>,
    #[serde(default)]
    pub stopped_confirmed: Vec<String>,
}

/// One sweep's stored record (§5.1), arms nested. `status` is a
/// plain string — `"running"` / `"complete"` / `"partial"` /
/// `"failed"` — matching the record's lifecycle vocabulary.
/// `projected_minutes` is stored beside the actual duration the
/// timestamps give, so a target overrun is a reported fact of the
/// record, not a hidden one (D14).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SweepRecord {
    pub id: String,
    pub status: String,
    pub started_ms: i64,
    #[serde(default)]
    pub ended_ms: Option<i64>,
    pub app_version: String,
    pub llama_build: String,
    #[serde(default)]
    pub machine_gpus: Vec<String>,
    pub backend_id: String,
    pub backend_name: String,
    pub model: ModelIdentity,
    pub prompt_id: String,
    pub prompt_name: String,
    pub prompt_sha256: String,
    #[serde(default)]
    pub prompt_tokens: Option<u32>,
    pub temperature: f32,
    pub correctness_control: bool,
    #[serde(default)]
    pub interlock: InterlockRecord,
    pub baseline_arm_index: usize,
    pub projected_minutes: f64,
    #[serde(default)]
    pub partial: bool,
    #[serde(default)]
    pub arms: Vec<ArmRecord>,
}

/// The winner of one measurement condition (one depth) inside a
/// sweep: the measured arm with the highest median generation rate,
/// with its delta against the baseline arm in the same condition
/// when the baseline was measured there (§5.2: one winner per
/// condition, never a single winner across conditions).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ConditionWinner {
    pub depth: u32,
    pub arm_index: usize,
    pub median_gen_tps: f64,
    pub baseline_gen_tps: Option<f64>,
    pub delta_pct: Option<f64>,
}

/// Compute the per-condition winners of a sweep. Only arms with
/// status `measured` and a present `median_gen_tps` compete. Winners
/// are returned in ascending depth order.
pub fn condition_winners(sweep: &SweepRecord) -> Vec<ConditionWinner> {
    let mut by_depth: BTreeMap<u32, Vec<&ArmRecord>> = BTreeMap::new();
    for arm in &sweep.arms {
        if arm.status != ArmStatus::Measured {
            continue;
        }
        if arm
            .summary
            .as_ref()
            .and_then(|s| s.median_gen_tps)
            .is_none()
        {
            continue;
        }
        by_depth.entry(arm.spec.depth).or_default().push(arm);
    }
    let mut winners = Vec::new();
    for (depth, arms) in by_depth {
        let winner = arms
            .iter()
            .max_by(|a, b| {
                let ga = a.summary.as_ref().and_then(|s| s.median_gen_tps);
                let gb = b.summary.as_ref().and_then(|s| s.median_gen_tps);
                ga.partial_cmp(&gb).unwrap_or(std::cmp::Ordering::Equal)
            })
            .expect("condition has at least one measured arm");
        let winner_tps = winner
            .summary
            .as_ref()
            .and_then(|s| s.median_gen_tps)
            .expect("winner was filtered to measured arms");
        let baseline = arms
            .iter()
            .find(|a| a.index == sweep.baseline_arm_index);
        let baseline_gen_tps = baseline.and_then(|a| {
            a.summary.as_ref().and_then(|s| s.median_gen_tps)
        });
        let delta_pct = baseline_gen_tps.and_then(|base| {
            if base != 0.0 {
                Some((winner_tps - base) / base * 100.0)
            } else {
                None
            }
        });
        winners.push(ConditionWinner {
            depth,
            arm_index: winner.index,
            median_gen_tps: winner_tps,
            baseline_gen_tps,
            delta_pct,
        });
    }
    winners
}

/// Current on-disk format version of the results file.
const BENCH_RESULTS_FORMAT_VERSION: u32 = 1;

/// The results store: one record per sweep, retained indefinitely
/// (§5.3); deletion is an explicit operator action per sweep.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct BenchResults {
    pub version: u32,
    #[serde(default)]
    pub sweeps: Vec<SweepRecord>,
}

impl Default for BenchResults {
    fn default() -> Self {
        BenchResults {
            version: BENCH_RESULTS_FORMAT_VERSION,
            sweeps: Vec::new(),
        }
    }
}

impl BenchResults {
    /// Load the results at `path`. A missing file is an empty store;
    /// a present but unparseable file is an error, matching
    /// [`crate::PromptLibrary::load`].
    pub fn load(path: &Path) -> io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let mut results: BenchResults = serde_json::from_str(&text)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                results.version = BENCH_RESULTS_FORMAT_VERSION;
                Ok(results)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(BenchResults::default()),
            Err(e) => Err(e),
        }
    }

    /// Save the results at `path` as pretty JSON, creating parent
    /// directories when needed.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, text)
    }

    /// Append a sweep record, replacing any record with the same id
    /// (a sweep's record is rewritten as it progresses).
    pub fn upsert(&mut self, sweep: SweepRecord) {
        if let Some(existing) = self.sweeps.iter_mut().find(|s| s.id == sweep.id) {
            *existing = sweep;
        } else {
            self.sweeps.push(sweep);
        }
    }

    /// Remove the sweep record with `id`; returns whether one was
    /// removed.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.sweeps.len();
        self.sweeps.retain(|s| s.id != id);
        self.sweeps.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arm::ArmSpec;

    fn run(warmup: bool, gen_tps: f64) -> RunRow {
        RunRow {
            warmup,
            gen_tps: Some(gen_tps),
            prompt_tps: Some(100.0),
            ttft_ms: Some(50.0),
            ..RunRow::default()
        }
    }

    #[test]
    fn median_handles_empty_odd_and_even() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(median(&[7.0]), Some(7.0));
    }

    #[test]
    fn summarize_excludes_warmup_rows() {
        let runs = vec![
            run(true, 999.0),
            run(false, 10.0),
            run(false, 30.0),
            run(false, 20.0),
        ];
        let s = summarize(&runs);
        assert_eq!(s.median_gen_tps, Some(20.0));
        assert_eq!(s.median_prompt_tps, Some(100.0));
        // No acceptance data anywhere: acceptance figures are None,
        // not zero.
        assert_eq!(s.mean_acceptance_rate, None);
        assert_eq!(s.mean_accepted_len, None);
    }

    #[test]
    fn summarize_means_acceptance_over_rows_that_have_it() {
        let mut r1 = run(false, 10.0);
        r1.acceptance_rate = Some(0.5);
        r1.mean_accepted_len = Some(2.0);
        let mut r2 = run(false, 12.0);
        r2.acceptance_rate = Some(0.7);
        r2.mean_accepted_len = Some(3.0);
        let r3 = run(false, 11.0);
        let s = summarize(&[r1, r2, r3]);
        assert!((s.mean_acceptance_rate.expect("mean") - 0.6).abs() < 1e-9);
        assert!((s.mean_accepted_len.expect("mean") - 2.5).abs() < 1e-9);
        assert_eq!(summarize(&[]).median_gen_tps, None);
    }

    fn measured_arm(index: usize, depth: u32, gen_tps: f64) -> ArmRecord {
        ArmRecord {
            index,
            label: format!("arm {index}"),
            spec: ArmSpec {
                depth,
                ..ArmSpec::default()
            },
            raw_flags: String::new(),
            comparability_key: "k".to_string(),
            status: ArmStatus::Measured,
            detail: None,
            runs: Vec::new(),
            summary: Some(ArmSummary {
                median_gen_tps: Some(gen_tps),
                ..ArmSummary::default()
            }),
            peak_vram_mib: None,
            started_ms: None,
            ended_ms: None,
        }
    }

    fn sweep(arms: Vec<ArmRecord>, baseline_arm_index: usize) -> SweepRecord {
        SweepRecord {
            id: "sweep-1".to_string(),
            status: "complete".to_string(),
            started_ms: 0,
            ended_ms: Some(1),
            app_version: "0.1.50".to_string(),
            llama_build: "b11658".to_string(),
            machine_gpus: vec!["Tesla P100".to_string()],
            backend_id: "coder".to_string(),
            backend_name: "Coder".to_string(),
            model: ModelIdentity {
                path: "coder.gguf".to_string(),
                size_bytes: 1,
                sha256: None,
            },
            prompt_id: "coder-standard".to_string(),
            prompt_name: "Coder".to_string(),
            prompt_sha256: "ab".to_string(),
            prompt_tokens: Some(4000),
            temperature: 0.0,
            correctness_control: true,
            interlock: InterlockRecord::default(),
            baseline_arm_index,
            projected_minutes: 9.0,
            partial: false,
            arms,
        }
    }

    #[test]
    fn condition_winners_pick_per_depth_winner_with_baseline_delta() {
        let s = sweep(
            vec![
                measured_arm(0, 8192, 50.0),
                measured_arm(1, 8192, 75.0),
                measured_arm(2, 30720, 40.0),
                measured_arm(3, 30720, 44.0),
            ],
            0,
        );
        let winners = condition_winners(&s);
        assert_eq!(winners.len(), 2);
        // Depth 8192: arm 1 wins, +50% over the baseline (arm 0,
        // measured in this same condition).
        assert_eq!(winners[0].depth, 8192);
        assert_eq!(winners[0].arm_index, 1);
        assert_eq!(winners[0].baseline_gen_tps, Some(50.0));
        assert!((winners[0].delta_pct.expect("delta") - 50.0).abs() < 1e-9);
        // Depth 30720: arm 3 wins; the baseline arm is not in this
        // condition, so there is no baseline and no delta.
        assert_eq!(winners[1].depth, 30720);
        assert_eq!(winners[1].arm_index, 3);
        assert_eq!(winners[1].baseline_gen_tps, None);
        assert_eq!(winners[1].delta_pct, None);
    }

    #[test]
    fn condition_winners_skip_unmeasured_arms() {
        let mut failed = measured_arm(1, 8192, 999.0);
        failed.status = ArmStatus::Failed;
        let s = sweep(vec![measured_arm(0, 8192, 50.0), failed], 0);
        let winners = condition_winners(&s);
        assert_eq!(winners.len(), 1);
        assert_eq!(winners[0].arm_index, 0);
        assert_eq!(winners[0].delta_pct, Some(0.0));
        assert!(condition_winners(&sweep(vec![], 0)).is_empty());
    }

    #[test]
    fn arm_status_serialization_matches_contract() {
        assert_eq!(
            serde_json::to_string(&ArmStatus::PreflightFailed).expect("ser"),
            "\"preflight_failed\""
        );
        assert_eq!(
            serde_json::to_string(&ArmStatus::Measured).expect("ser"),
            "\"measured\""
        );
    }

    #[test]
    fn results_roundtrip_and_missing_file() {
        let dir = std::env::temp_dir().join(format!(
            "manager-bench-results-test-{}",
            std::process::id()
        ));
        let path = dir.join("bench-results.json");
        let store = BenchResults::load(&path).expect("missing loads empty");
        assert!(store.sweeps.is_empty());
        let mut store = BenchResults::default();
        store.upsert(sweep(vec![measured_arm(0, 8192, 50.0)], 0));
        store.save(&path).expect("save");
        let back = BenchResults::load(&path).expect("load saved");
        assert_eq!(back, store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
