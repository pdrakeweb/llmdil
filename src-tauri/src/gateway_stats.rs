//! Gateway request statistics from LiteLLM's Prometheus endpoint.
//!
//! The generated gateway config enables LiteLLM's `prometheus`
//! success callback, which mounts `/metrics` on the gateway port
//! (master-key authenticated). A background poller in main.rs
//! scrapes it on the telemetry cadence and records snapshots here.
//! The Dashboard's per-backend requests/errors/throughput are
//! trailing-window deltas between snapshots — LiteLLM exposes only
//! cumulative counters (since gateway start), so the window math is
//! the app's.
//!
//! Attribution: the deployment counters
//! (`litellm_deployment_success_responses`,
//! `litellm_deployment_failure_responses`) carry an `api_base` label
//! (`http://127.0.0.1:<port>/v1` for local backends), which maps to a
//! backend by port. The token counters
//! (`litellm_input_tokens_metric`, `litellm_output_tokens_metric`)
//! carry a `model` label (`openai/<backend-id>`), which maps to a
//! backend by id. Series names gain a `_total` suffix under
//! prometheus_client 0.26 exposition rules, and every counter also
//! emits a `*_created` series — both handled in the parser.
//!
//! Limits (by design): completed requests only; counters reset when
//! the gateway restarts, which the delta math absorbs by treating a
//! decreased counter as restarted from zero.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// One scrape of the gateway's cumulative counters.
#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    pub at: Instant,
    /// api_base -> (successful responses, failed responses).
    pub deployments: HashMap<String, (u64, u64)>,
    /// Deployment model label (`openai/<backend-id>`) -> total
    /// tokens (input + output summed at parse time).
    pub tokens: HashMap<String, u64>,
}

/// Trailing-window figures for one backend.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackendWindow {
    pub requests: u64,
    pub errors: u64,
    /// Total tokens over the window (0 when the token series is
    /// absent for the backend).
    pub tokens: u64,
    /// Seconds between the baseline and latest snapshots — the
    /// actual window the deltas cover (up to one hour).
    pub window_secs: f64,
}

/// Snapshot ring: two hours retained at the 15 s poll cadence.
#[derive(Debug, Default)]
pub struct GatewayStatsStore {
    snapshots: VecDeque<MetricsSnapshot>,
}

const RETENTION: Duration = Duration::from_secs(2 * 3600);
const WINDOW: Duration = Duration::from_secs(3600);

impl GatewayStatsStore {
    pub fn record(&mut self, snap: MetricsSnapshot) {
        while let Some(front) = self.snapshots.front() {
            if snap.at.duration_since(front.at) > RETENTION {
                self.snapshots.pop_front();
            } else {
                break;
            }
        }
        self.snapshots.push_back(snap);
    }

    /// Trailing-window stats for every backend, keyed by backend id.
    /// `backends` maps backend id -> port (for the api_base and model
    /// label lookups). Returns None until two snapshots exist.
    pub fn window_stats(
        &self,
        backends: &[(String, u16)],
    ) -> Option<HashMap<String, BackendWindow>> {
        if self.snapshots.len() < 2 {
            return None;
        }
        let latest = self.snapshots.back()?;
        let target = latest.at.checked_sub(WINDOW).unwrap_or(latest.at);
        // Baseline: the earliest snapshot inside the window (the
        // closest reading at or after the window start). When the
        // latest is the only snapshot inside — polls just crossed
        // the edge — fall back to the previous snapshot, which
        // over-covers by at most one poll interval.
        let baseline = match self.snapshots.iter().find(|s| s.at >= target) {
            Some(b) if !std::ptr::eq(b, latest) => b,
            _ => self.snapshots.iter().rev().nth(1)?,
        };
        let window_secs = latest.at.duration_since(baseline.at).as_secs_f64();
        if window_secs <= 0.0 {
            return None;
        }
        let delta = |now: u64, then: u64| -> u64 {
            // A decreased counter means the gateway restarted and the
            // counter restarted from zero: everything counted since
            // is inside the window.
            if now >= then {
                now - then
            } else {
                now
            }
        };
        let mut out = HashMap::new();
        for (id, port) in backends {
            let api_base = format!("http://127.0.0.1:{port}/v1");
            let model = format!("openai/{id}");
            let (ls, lf) = latest.deployments.get(&api_base).copied().unwrap_or((0, 0));
            let (bs, bf) = baseline
                .deployments
                .get(&api_base)
                .copied()
                .unwrap_or((0, 0));
            let lt = latest.tokens.get(&model).copied().unwrap_or(0);
            let bt = baseline.tokens.get(&model).copied().unwrap_or(0);
            out.insert(
                id.clone(),
                BackendWindow {
                    requests: delta(ls, bs),
                    errors: delta(lf, bf),
                    tokens: delta(lt, bt),
                    window_secs,
                },
            );
        }
        Some(out)
    }
}

/// Parse Prometheus text exposition into a snapshot (timestamp left
/// at the epoch placeholder; the caller stamps it). Unknown series
/// are ignored; malformed lines are skipped, never fatal.
pub fn parse_metrics(text: &str, at: Instant) -> MetricsSnapshot {
    let mut snap = MetricsSnapshot {
        at,
        deployments: HashMap::new(),
        tokens: HashMap::new(),
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, labels, value)) = parse_series(line) else {
            continue;
        };
        let base = name.strip_suffix("_total").unwrap_or(name);
        if base.ends_with("_created") {
            continue;
        }
        match base {
            "litellm_deployment_success_responses"
            | "litellm_deployment_failure_responses" => {
                let Some(api_base) = label_value(&labels, "api_base") else {
                    continue;
                };
                let entry = snap.deployments.entry(api_base).or_insert((0, 0));
                if base.ends_with("success_responses") {
                    entry.0 += value;
                } else {
                    entry.1 += value;
                }
            }
            "litellm_input_tokens_metric" | "litellm_output_tokens_metric" => {
                let Some(model) = label_value(&labels, "model") else {
                    continue;
                };
                *snap.tokens.entry(model).or_insert(0) += value;
            }
            _ => {}
        }
    }
    snap
}

/// Split one exposition line into (series name, raw label block,
/// integer value). The value is a float in the text; counters are
/// integral, so it is rounded.
fn parse_series(line: &str) -> Option<(&str, &str, u64)> {
    let (head, value_str) = line.rsplit_once(' ')?;
    let value: f64 = value_str.trim().parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let value = value.round() as u64;
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

/// Extract one label's value from a raw label block, handling the
/// `\"` and `\\` escapes Prometheus uses inside quoted values.
fn label_value(labels: &str, key: &str) -> Option<String> {
    // Split on commas that are outside quoted strings.
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut escaped = false;
    for ch in labels.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_quotes => {
                cur.push(ch);
                escaped = true;
            }
            '"' => {
                in_quotes = !in_quotes;
                cur.push(ch);
            }
            ',' if !in_quotes => {
                parts.push(std::mem::take(&mut cur));
            }
            _ => cur.push(ch),
        }
    }
    parts.push(cur);
    for part in parts {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        if k.trim() == key {
            let v = v.trim().strip_prefix('"')?.strip_suffix('"')?;
            return Some(v.replace("\\\"", "\"").replace("\\\\", "\\"));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# HELP litellm_deployment_success_responses_total Total successful responses
# TYPE litellm_deployment_success_responses_total counter
litellm_deployment_success_responses_total{api_base=\"http://127.0.0.1:8081/v1\",exception_status=\"200\",exception_type=\"None\",hash=\"a1\",llm_provider=\"openai\",model=\"openai/planner\",requested_model=\"planner\"} 41.0
litellm_deployment_success_responses_created{api_base=\"http://127.0.0.1:8081/v1\",model=\"openai/planner\"} 1.7605e+09
litellm_deployment_failure_responses_total{api_base=\"http://127.0.0.1:8081/v1\",model=\"openai/planner\"} 3.0
litellm_deployment_failure_responses_total{api_base=\"http://127.0.0.1:8082/v1\",model=\"openai/coder\"} 1.0
litellm_input_tokens_metric_total{model=\"openai/planner\",requested_model=\"planner\"} 12000.0
litellm_output_tokens_metric_total{model=\"openai/planner\"} 3400.0
litellm_input_tokens_metric_total{model=\"openai/coder\"} 500.0
litellm_in_flight_requests{model=\"planner\"} 2.0
";

    #[test]
    fn parses_deployment_and_token_counters() {
        let snap = parse_metrics(SAMPLE, Instant::now());
        assert_eq!(
            snap.deployments.get("http://127.0.0.1:8081/v1"),
            Some(&(41, 3))
        );
        assert_eq!(
            snap.deployments.get("http://127.0.0.1:8082/v1"),
            Some(&(0, 1))
        );
        // Input + output summed per model label; _created ignored.
        assert_eq!(snap.tokens.get("openai/planner"), Some(&15400));
        assert_eq!(snap.tokens.get("openai/coder"), Some(&500));
    }

    #[test]
    fn window_deltas_use_oldest_snapshot_within_retention_as_baseline() {
        let t0 = Instant::now();
        let mut store = GatewayStatsStore::default();
        let mk = |at: Instant, success: u64, failure: u64, tokens: u64| MetricsSnapshot {
            at,
            deployments: HashMap::from([("http://127.0.0.1:8081/v1".into(), (success, failure))]),
            tokens: HashMap::from([("openai/planner".into(), tokens)]),
        };
        store.record(mk(t0, 10, 1, 1000));
        // 30 minutes later: still inside the hour, baseline stays t0.
        store.record(mk(t0 + Duration::from_secs(1800), 25, 2, 4600));
        let stats = store
            .window_stats(&[("planner".into(), 8081)])
            .expect("two snapshots");
        let w = stats["planner"];
        assert_eq!(w.requests, 15);
        assert_eq!(w.errors, 1);
        assert_eq!(w.tokens, 3600);
        assert_eq!(w.window_secs, 1800.0);
    }

    #[test]
    fn window_baseline_rolls_to_the_hour_edge() {
        let t0 = Instant::now();
        let mut store = GatewayStatsStore::default();
        let mk = |at: Instant, success: u64| MetricsSnapshot {
            at,
            deployments: HashMap::from([("http://127.0.0.1:8081/v1".into(), (success, 0))]),
            tokens: HashMap::new(),
        };
        store.record(mk(t0, 100));
        store.record(mk(t0 + Duration::from_secs(3500), 160));
        store.record(mk(t0 + Duration::from_secs(3700), 175));
        let stats = store
            .window_stats(&[("planner".into(), 8081)])
            .expect("snapshots");
        // Baseline is the t0+3500 snapshot (newest at or before the
        // hour edge at t0+100), so the delta covers only 200 s.
        assert_eq!(stats["planner"].requests, 15);
        assert_eq!(stats["planner"].window_secs, 200.0);
    }

    #[test]
    fn counter_reset_counts_from_zero() {
        let t0 = Instant::now();
        let mut store = GatewayStatsStore::default();
        let mk = |at: Instant, success: u64| MetricsSnapshot {
            at,
            deployments: HashMap::from([("http://127.0.0.1:8081/v1".into(), (success, 0))]),
            tokens: HashMap::new(),
        };
        store.record(mk(t0, 900));
        store.record(mk(t0 + Duration::from_secs(60), 7));
        let stats = store
            .window_stats(&[("planner".into(), 8081)])
            .expect("snapshots");
        assert_eq!(stats["planner"].requests, 7);
    }

    #[test]
    fn one_snapshot_is_not_a_window() {
        let mut store = GatewayStatsStore::default();
        store.record(MetricsSnapshot {
            at: Instant::now(),
            deployments: HashMap::new(),
            tokens: HashMap::new(),
        });
        assert!(store.window_stats(&[("planner".into(), 8081)]).is_none());
    }
}
