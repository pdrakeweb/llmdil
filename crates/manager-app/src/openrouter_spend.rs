//! OpenRouter daily spend ledger (technical spec §7 cost guardrail).
//!
//! Enforcement lives in the gateway: the generated LiteLLM config
//! carries `router_settings.provider_budget_config.openrouter`
//! (`budget_limit` = the configured daily cap, `time_period: 1d`), so
//! LiteLLM itself filters every `openrouter/...` deployment out of
//! routing once the window's spend reaches the cap (fail closed to
//! local), and a zero cap generates no OpenRouter deployments at all
//! (see `manager_gateway::openrouter_cap_allows_spend`).
//!
//! This module is the app's MEASUREMENT of that spend, for the status
//! surfaces and the Dashboard / Gateway banner. The gateway metrics
//! poller hands each `/metrics` scrape to [`record_scrape`], which
//! folds the deltas of LiteLLM's cumulative `litellm_spend_metric`
//! counters into a per-local-day ledger persisted in the data
//! directory (so an app restart keeps today's figure). Attribution:
//! the gateway's only metered provider is OpenRouter — local
//! llama-server deployments are `openai/<id>` models with no price,
//! which LiteLLM costs at zero — so the summed spend counter IS the
//! OpenRouter spend; anything else that ever reported a cost would
//! over-count, which errs toward the cap (never toward spending).
//!
//! Counter semantics: the counters are cumulative since the gateway
//! process started and reset when it restarts. Each series is
//! tracked by its full label set with its `_created` timestamp; a new
//! series, a changed creation time, or a decreasing value is a reset
//! and counts from zero. The ledger resets at local midnight; the
//! LiteLLM window instead opens at the first metered spend and lasts
//! 24 h, and is held in the gateway's memory — a gateway restart
//! starts a fresh LiteLLM window. The two can therefore disagree near
//! a day boundary or after a gateway restart; the status payload
//! reports the app ledger, which is the conservative view for a
//! restart (it keeps counting across gateway restarts).

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

/// Ledger file in the data directory.
pub const LEDGER_FILE: &str = "openrouter-spend.json";

/// Fraction of the cap at which the status surfaces warn.
pub const WARN_FRACTION: f64 = 0.8;

/// One counter series' last observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeriesMark {
    /// The series' `_created` timestamp, when exposed.
    pub created: Option<f64>,
    /// Last cumulative value (USD).
    pub value: f64,
}

/// The persisted per-day ledger.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SpendLedger {
    /// Local calendar day `spent_usd` covers.
    pub day: Option<NaiveDate>,
    /// Spend observed on `day`, in USD.
    pub spent_usd: f64,
    /// Label set -> last observation, the baseline for the next delta.
    #[serde(default)]
    pub series: BTreeMap<String, SeriesMark>,
}

/// Parse the spend counter series from a Prometheus exposition:
/// `litellm_spend_metric_total` (or the bare name, for exporters that
/// omit the suffix) values keyed by label block, joined with the
/// matching `litellm_spend_metric_created` timestamps.
pub fn parse_spend(metrics: &str) -> BTreeMap<String, SeriesMark> {
    let mut values: BTreeMap<String, f64> = BTreeMap::new();
    let mut created: BTreeMap<String, f64> = BTreeMap::new();
    for line in metrics.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, labels, value)) = split_series(line) else {
            continue;
        };
        match name {
            "litellm_spend_metric_total" | "litellm_spend_metric"
                if value.is_finite() && value >= 0.0 =>
            {
                *values.entry(labels.to_string()).or_insert(0.0) += value;
            }
            "litellm_spend_metric_created" if value.is_finite() => {
                created.insert(labels.to_string(), value);
            }
            _ => {}
        }
    }
    values
        .into_iter()
        .map(|(labels, value)| {
            let c = created.get(&labels).copied();
            (labels, SeriesMark { created: c, value })
        })
        .collect()
}

/// Split one exposition line into (name, raw label block, value). The
/// value is the last whitespace-separated token; the label block runs
/// from the first `{` to the last `}` before it, so quoted label
/// values containing braces stay inside the block.
fn split_series(line: &str) -> Option<(&str, &str, f64)> {
    let (head, value) = line.rsplit_once(' ')?;
    let value: f64 = value.trim().parse().ok()?;
    let head = head.trim_end();
    match head.find('{') {
        Some(open) => {
            let close = head.rfind('}')?;
            if close < open {
                return None;
            }
            Some((&head[..open], &head[open + 1..close], value))
        }
        None => Some((head, "", value)),
    }
}

/// Fold one scrape into the ledger for local day `today`: a day
/// change resets the day's total (the counter baselines carry over,
/// so only spend after the boundary counts toward the new day).
pub fn apply_scrape(
    ledger: &mut SpendLedger,
    sample: BTreeMap<String, SeriesMark>,
    today: NaiveDate,
) {
    if ledger.day != Some(today) {
        ledger.day = Some(today);
        ledger.spent_usd = 0.0;
    }
    for (labels, mark) in &sample {
        let delta = match ledger.series.get(labels) {
            Some(prev) if prev.created == mark.created && mark.value >= prev.value => {
                mark.value - prev.value
            }
            // New series, a restarted gateway (new creation time), or
            // a counter that went backwards: count from zero.
            _ => mark.value,
        };
        if delta.is_finite() && delta > 0.0 {
            ledger.spent_usd += delta;
        }
    }
    // Series absent from this scrape belonged to a previous gateway
    // process; the scrape is the complete current baseline.
    ledger.series = sample;
}

/// Load the ledger; a missing or unreadable file is an empty ledger.
pub fn load(data_dir: &Path) -> SpendLedger {
    std::fs::read_to_string(data_dir.join(LEDGER_FILE))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist the ledger (write-then-rename, so a crash never leaves a
/// torn file). Failures are logged and otherwise ignored: the ledger
/// is a status surface, not the enforcement point.
pub fn save(data_dir: &Path, ledger: &SpendLedger) {
    let path = data_dir.join(LEDGER_FILE);
    let tmp = data_dir.join(format!("{LEDGER_FILE}.tmp"));
    let Ok(body) = serde_json::to_string_pretty(ledger) else {
        return;
    };
    if let Err(e) = std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, &path)) {
        tracing::warn!(error = %e, "could not persist the OpenRouter spend ledger");
    }
}

/// The poller's entry point: fold one `/metrics` body into the
/// persisted ledger for the current local day.
pub fn record_scrape(data_dir: &Path, metrics: &str) {
    let sample = parse_spend(metrics);
    let mut ledger = load(data_dir);
    let before = ledger.clone();
    apply_scrape(&mut ledger, sample, chrono::Local::now().date_naive());
    if ledger != before {
        save(data_dir, &ledger);
    }
}

/// Today's spend from a ledger: zero when the ledger is for another day.
pub fn spent_on(ledger: &SpendLedger, today: NaiveDate) -> f64 {
    if ledger.day == Some(today) {
        ledger.spent_usd
    } else {
        0.0
    }
}

/// The additive `openrouter_spend` object on `gateway_status`.
///
/// - `spent_today_usd`: the app ledger's figure for the local day.
/// - `cap_usd`: the configured daily cap.
/// - `tier_active`: the generated config carries OpenRouter
///   deployments (tier enabled, cap > 0, non-empty allowlist).
/// - `capped`: no OpenRouter traffic will be sent — the tier is
///   enabled but the cap is zero, or today's spend reached the cap.
/// - `warn`: spend is at or above [`WARN_FRACTION`] of the cap.
/// - `fraction`: spend / cap (null when the cap is zero).
/// - `resets`: when the app ledger resets ("local_midnight") and the
///   gateway's own window (`budget_window`).
pub fn status_value(
    cloud: &manager_config::CloudTierConfig,
    tier_active: bool,
    spent_today_usd: f64,
) -> Value {
    let cap = cloud.daily_cap_usd;
    let cap_allows = manager_gateway::openrouter_cap_allows_spend(cap);
    let fraction = if cap_allows {
        Some(spent_today_usd / cap)
    } else {
        None
    };
    let capped = cloud.enabled && (!cap_allows || spent_today_usd >= cap);
    let warn = cloud.enabled && fraction.is_some_and(|f| f >= WARN_FRACTION);
    json!({
        "enabled": cloud.enabled,
        "tier_active": tier_active,
        "spent_today_usd": spent_today_usd,
        "cap_usd": cap,
        "fraction": fraction,
        "capped": capped,
        "warn": warn,
        "resets": "local_midnight",
        "budget_window": manager_gateway::OPENROUTER_BUDGET_PERIOD,
    })
}

/// [`status_value`] for the current local day, from the persisted ledger.
pub fn status_for(data_dir: &Path, config: &manager_config::AppConfig) -> Value {
    let ledger = load(data_dir);
    let spent = spent_on(&ledger, chrono::Local::now().date_naive());
    status_value(
        &config.gateway.openrouter,
        manager_gateway::openrouter_tier_active(config),
        spent,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, d).unwrap()
    }

    const LABELS: &str =
        r#"end_user="",hashed_api_key="abc",model="qwen/qwen3-coder",user_agent="x{y}""#;

    fn exposition(value: f64, created: f64) -> String {
        format!(
            "# HELP litellm_spend_metric_total Total spend on LLM requests\n\
             # TYPE litellm_spend_metric_total counter\n\
             litellm_spend_metric_total{{{LABELS}}} {value}\n\
             litellm_spend_metric_created{{{LABELS}}} {created}\n\
             litellm_deployment_success_responses_total{{api_base=\"http://127.0.0.1:8081/v1\"}} 4.0\n"
        )
    }

    fn sample(value: f64, created: f64) -> BTreeMap<String, SeriesMark> {
        parse_spend(&exposition(value, created))
    }

    fn cloud(enabled: bool, cap: f64) -> manager_config::CloudTierConfig {
        manager_config::CloudTierConfig {
            enabled,
            cred_ref: Some("openrouter".to_string()),
            daily_cap_usd: cap,
            model_allowlist: vec!["qwen/qwen3-coder".to_string()],
            overflow: Default::default(),
        }
    }

    #[test]
    fn parses_spend_series_with_created_and_braced_labels() {
        let s = sample(0.125, 1_700_000_000.5);
        assert_eq!(s.len(), 1);
        let mark = s.get(LABELS).expect("series keyed by its label block");
        assert_eq!(mark.value, 0.125);
        assert_eq!(mark.created, Some(1_700_000_000.5));
        // Other metrics are ignored.
        assert!(parse_spend("litellm_requests_metric_total{a=\"b\"} 3\n").is_empty());
    }

    #[test]
    fn deltas_accumulate_within_a_day() {
        let mut l = SpendLedger::default();
        apply_scrape(&mut l, sample(0.5, 1.0), day(12));
        assert_eq!(l.spent_usd, 0.5, "a first sighting counts from zero");
        apply_scrape(&mut l, sample(1.25, 1.0), day(12));
        assert!((l.spent_usd - 1.25).abs() < 1e-9);
        apply_scrape(&mut l, sample(1.25, 1.0), day(12));
        assert!((l.spent_usd - 1.25).abs() < 1e-9, "no change, no spend");
    }

    #[test]
    fn gateway_restart_counts_the_new_counter_from_zero() {
        let mut l = SpendLedger::default();
        apply_scrape(&mut l, sample(2.0, 1.0), day(12));
        // Restart: new creation time, counter starts over. The new
        // process's spend adds to the day; nothing is lost.
        apply_scrape(&mut l, sample(0.75, 2.0), day(12));
        assert!((l.spent_usd - 2.75).abs() < 1e-9);
        // A decreasing value with no creation time is also a reset.
        let mut m = SpendLedger::default();
        let mut s = sample(2.0, 1.0);
        s.values_mut().for_each(|v| v.created = None);
        apply_scrape(&mut m, s.clone(), day(12));
        s.values_mut().for_each(|v| v.value = 0.5);
        apply_scrape(&mut m, s, day(12));
        assert!((m.spent_usd - 2.5).abs() < 1e-9);
    }

    #[test]
    fn local_midnight_resets_the_day_but_keeps_baselines() {
        let mut l = SpendLedger::default();
        apply_scrape(&mut l, sample(4.0, 1.0), day(12));
        assert_eq!(spent_on(&l, day(12)), 4.0);
        // The next day: only spend after the boundary counts.
        apply_scrape(&mut l, sample(4.5, 1.0), day(13));
        assert_eq!(l.day, Some(day(13)));
        assert!((l.spent_usd - 0.5).abs() < 1e-9);
        // Reading a stale ledger on a later day reports zero.
        assert_eq!(spent_on(&l, day(14)), 0.0);
    }

    #[test]
    fn status_capped_warn_and_under_cap() {
        let under = status_value(&cloud(true, 5.0), true, 1.0);
        assert_eq!(under["capped"], false);
        assert_eq!(under["warn"], false);
        assert_eq!(under["fraction"], 0.2);
        assert_eq!(under["budget_window"], "1d");

        let warn = status_value(&cloud(true, 5.0), true, 4.0);
        assert_eq!(warn["capped"], false);
        assert_eq!(warn["warn"], true);

        let capped = status_value(&cloud(true, 5.0), true, 5.0);
        assert_eq!(capped["capped"], true);
        assert_eq!(capped["warn"], true);

        // Zero cap: never spend — capped from the start.
        let zero = status_value(&cloud(true, 0.0), false, 0.0);
        assert_eq!(zero["capped"], true);
        assert!(zero["fraction"].is_null());

        // Tier disabled: nothing to cap or warn about.
        let off = status_value(&cloud(false, 5.0), false, 9.0);
        assert_eq!(off["capped"], false);
        assert_eq!(off["warn"], false);
    }

    #[test]
    fn ledger_round_trips_through_the_data_dir() {
        let dir = std::env::temp_dir().join(format!("llmdil-spend-ledger-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(load(&dir), SpendLedger::default());
        let mut l = SpendLedger::default();
        apply_scrape(&mut l, sample(1.5, 1.0), day(12));
        save(&dir, &l);
        assert_eq!(load(&dir), l);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
