//! Pure logic for the Bench capability (design:
//! `docs`/bench design §1.1, §3–§5): sweep arm types with
//! expansion and validity pruning, comparability keys, raw-flag
//! rewriting for arm launches, speculative-decoding log parsing,
//! the prompt library with its shipped defaults, measurement
//! summaries, the results store, and per-condition winner
//! computation.
//!
//! Everything here is Tauri-free and pure over its inputs, so it is
//! unit-tested on Linux CI, following the `manager-optimize` split:
//! process, HTTP, and supervisor glue live in `manager-app`.
//!
//! One framing rules the whole crate (D14, Pete's): the 20-minute
//! sweep figure is a **target, not a gate**. The projection
//! ([`project_minutes`]) exists so an over-target sweep *warns* with
//! its arithmetic shown; nothing in this crate refuses a sweep or
//! stops a run on time.

pub mod arm;
pub mod comparability;
pub mod flags;
pub mod logs;
pub mod prompts;
pub mod results;

pub use arm::{
    expand, preset_combined, preset_kv, preset_spec, project_minutes, prune_arms, ArmOutcome,
    ArmSpec, ArmVariant, CacheType, DraftDevice, FlashAttn, PresetSweep, PruneContext, SpecKind,
};
pub use comparability::{
    comparability_key, differing_fields, prompt_sha256, KeyInputs, ModelIdentity, RUNNER_KIND,
};
pub use flags::arm_raw_flags;
pub use logs::{parse_acceptance_line, parse_statistics_line, AcceptanceSample, DrafterStats};
pub use prompts::{
    padding_blocks, shipped_prompts, PromptEntry, PromptLibrary, PromptOrigin,
};
pub use results::{
    condition_winners, median, summarize, ArmRecord, ArmStatus, ArmSummary, BenchResults,
    ConditionWinner, DrafterAgg, InterlockRecord, RunRow, SweepRecord,
};

/// Measured runs per arm after the discarded warm-up (D12:
/// median-of-3, one statistic for all arms).
pub const MEASURED_RUNS: usize = 3;

/// Generated-token cap for an arm's warm-up run (Amendment 2: the
/// warm-up's real work is the prefill and the cache warm).
pub const WARMUP_MAX_TOKENS: u32 = 128;

/// Generated-token cap for each measured run (Amendment 2).
pub const MEASURED_MAX_TOKENS: u32 = 512;

/// Screening-pass temperature.
pub const DEFAULT_TEMPERATURE: f32 = 0.0;

/// Confirmation-pass temperature.
pub const CONFIRM_TEMPERATURE: f32 = 0.6;

/// Default arm-count planning bound per sweep (D14, derived from
/// the halved per-arm budget). A planning bound, not a cap: custom
/// sweeps may exceed it with the projection warning shown.
pub const ARM_COUNT_DEFAULT: usize = 6;

/// Sweep wall-clock target in minutes (D14). A target to try for,
/// never a gate: projections warn, runs are never stopped on it.
pub const SWEEP_TARGET_MINUTES: f64 = 20.0;

/// Current wall-clock time in milliseconds since the Unix epoch,
/// for record timestamps (`started_ms`, `updated_ms`, …).
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_constants_match_the_design() {
        assert_eq!(MEASURED_RUNS, 3);
        assert_eq!(WARMUP_MAX_TOKENS, 128);
        assert_eq!(MEASURED_MAX_TOKENS, 512);
        assert_eq!(DEFAULT_TEMPERATURE, 0.0);
        assert_eq!(CONFIRM_TEMPERATURE, 0.6);
        assert_eq!(ARM_COUNT_DEFAULT, 6);
        assert_eq!(SWEEP_TARGET_MINUTES, 20.0);
        assert_eq!(RUNNER_KIND, "server");
    }
}
