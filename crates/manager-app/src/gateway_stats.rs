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
//! Limits (by design): the cumulative counters move only when a
//! request COMPLETES, so a request still in flight contributes no
//! tokens yet — and LiteLLM's `litellm_in_flight_requests` gauge
//! cannot fill that gap per backend: it is a bare, label-free,
//! worker-scoped number (no `model` label is ever attached), so
//! any per-backend reading of it is a false attribution (the
//! Nov 3, 2026 throughput-0 diagnosis: the Oct 22 fix's in-flight
//! guard assumed a labeled gauge LiteLLM never emits, and its
//! per-backend branch is retired here). The gauge is recorded
//! only as a gateway-wide total, never per backend. The live
//! mid-generation rate instead comes from the backends
//! themselves: [`DirectSlotsStore`] polls each backend's
//! llama-server `/slots` endpoint (see below) and derives tok/s
//! from `n_decoded` deltas while slots are processing. The
//! gateway live rate below is the fallback when a backend's
//! `/slots` is unavailable, and it is computed over a short
//! recent window, because dividing a few seconds of generation
//! by a trailing hour renders a live backend as
//! "Throughput 0 tok/s" (the Oct 22, 2026 field bug this module's
//! `live_throughput` fixes; the trailing-hour figures remain
//! requests/errors/health only).

use std::collections::{HashMap, HashSet, VecDeque};
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
    /// Gateway-wide total of `litellm_in_flight_requests` across
    /// all exposition lines. The gauge carries no labels, so this
    /// number is scoped to the gateway worker, never to a backend
    /// or group: it must never be attributed per backend (see the
    /// module docs). It is recorded for diagnostics only; the
    /// per-backend live rate does not use it.
    pub in_flight_total: f64,
}

/// Trailing-window figures for one backend.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackendWindow {
    /// Total completed requests in the window (successes +
    /// failures). Every consumer treats this as the total: the
    /// dashboard's success share is `1 - errors / requests`, the
    /// model card's succeeded count is `requests - errors`. (This
    /// field originally carried successes only, which made both
    /// derivations wrong — the share understated and the succeeded
    /// count could go negative.)
    pub requests: u64,
    pub errors: u64,
    /// Total tokens over the window (0 when the token series is
    /// absent for the backend).
    pub tokens: u64,
    /// Seconds between the baseline and latest snapshots — the
    /// actual window the deltas cover (up to one hour).
    pub window_secs: f64,
}

/// One backend's live throughput verdict (see
/// [`GatewayStatsStore::live_throughput`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LiveThroughput {
    /// Tokens per second over the live window, or None when the
    /// current rate is not measured (a missed attribution) —
    /// absent renders as no tile, never as 0. Note this gateway
    /// rate alone cannot tell a mid-generation backend from an
    /// idle one (the counters move only on completion, and the
    /// in-flight gauge is gateway-wide, not per backend); the
    /// tile precedence in `list_backends` prefers the direct
    /// `/slots` rate ([`DirectSlotsStore`]) when it is available,
    /// and this rate is only its fallback.
    pub rate: Option<f64>,
    /// Requests completed in the live window but the backend's
    /// token series was absent from both snapshots: the
    /// attribution lookup missed, so even the token total is
    /// unknown. Surfaced (and logged once per episode by the
    /// store) instead of silently reading as zero traffic.
    pub attribution_missed: bool,
}

/// The live rate's window: long enough to smooth the burst of
/// tokens a completion lands in one poll interval (8 polls at
/// the 15 s cadence), short enough that a backend that starts
/// generating reads as generating now, not as an hour's average.
const LIVE_WINDOW: Duration = Duration::from_secs(120);
/// A live rate needs at least this much covered span; with less,
/// one completion burst would read as an absurd spike, so the
/// rate stays unmeasured until the span exists.
const LIVE_MIN_SPAN: Duration = Duration::from_secs(30);
/// The latest snapshot must be at most this old for a rate to
/// be called live (six missed polls at the 15 s cadence).
const LIVE_STALE_AFTER: Duration = Duration::from_secs(90);

/// Snapshot ring: two hours retained at the 15 s poll cadence.
#[derive(Debug, Default)]
pub struct GatewayStatsStore {
    snapshots: VecDeque<MetricsSnapshot>,
    /// Backend ids whose missed token attribution has already
    /// been logged (see [`GatewayStatsStore::live_throughput`]):
    /// a miss logs once per episode, not once per poll.
    miss_reported: HashSet<String>,
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
                    // Total = success delta + failure delta (each
                    // counter's restart handling is independent).
                    requests: delta(ls, bs) + delta(lf, bf),
                    errors: delta(lf, bf),
                    tokens: delta(lt, bt),
                    window_secs,
                },
            );
        }
        Some(out)
    }

    /// The LIVE per-backend token rate for the cards'
    /// Throughput tile: tokens counted over the recent
    /// [`LIVE_WINDOW`] divided by the span actually covered.
    /// This is deliberately NOT the trailing-hour figure
    /// [`Self::window_stats`] reports: an hour denominator
    /// dilutes seconds of live generation to a rounded 0.0 —
    /// the Oct 22, 2026 "Throughput 0 tok/s while generating"
    /// bug. `backends` maps backend id -> port (as in
    /// `window_stats`).
    ///
    /// Per backend the result is a [`LiveThroughput`]: a
    /// measured rate when tokens moved in the window; a
    /// measured 0 when nothing moved; ABSENT (`rate: None` —
    /// unknown is not zero, design principle 12) when
    /// attribution missed (see the struct). This gateway rate
    /// is only the fallback behind the direct `/slots` rate
    /// ([`DirectSlotsStore`]): on its own it cannot tell a
    /// mid-generation backend from an idle one, because
    /// LiteLLM counts a request's tokens only when it
    /// completes and the in-flight gauge is gateway-wide, not
    /// per backend. Returns None overall until two snapshots
    /// span at least [`LIVE_MIN_SPAN`], and when the latest
    /// snapshot is stale (the poller has stopped landing
    /// scrapes — a frozen rate is not a live one).
    pub fn live_throughput(
        &mut self,
        backends: &[(String, u16)],
    ) -> Option<HashMap<String, LiveThroughput>> {
        if self.snapshots.len() < 2 {
            return None;
        }
        let latest = self.snapshots.back()?;
        if latest.at.elapsed() > LIVE_STALE_AFTER {
            return None;
        }
        let target = latest.at.checked_sub(LIVE_WINDOW).unwrap_or(latest.at);
        // Baseline: the last snapshot at or before the window
        // start (the rate then covers at least the window);
        // when polling began inside the window, the earliest
        // snapshot (the rate covers what exists).
        let baseline = match self.snapshots.iter().rev().find(|s| s.at <= target) {
            Some(b) if !std::ptr::eq(b, latest) => b,
            _ => self.snapshots.front()?,
        };
        if std::ptr::eq(baseline, latest) {
            return None;
        }
        let span_secs = latest.at.duration_since(baseline.at).as_secs_f64();
        if span_secs < LIVE_MIN_SPAN.as_secs_f64() {
            return None;
        }
        let delta = |now: u64, then: u64| -> u64 {
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
            let tokens_delta = delta(
                latest.tokens.get(&model).copied().unwrap_or(0),
                baseline.tokens.get(&model).copied().unwrap_or(0),
            );
            let (ls, _) = latest.deployments.get(&api_base).copied().unwrap_or((0, 0));
            let (bs, _) = baseline
                .deployments
                .get(&api_base)
                .copied()
                .unwrap_or((0, 0));
            let successes_delta = delta(ls, bs);
            // No per-backend in-flight term: the gauge is
            // gateway-wide and label-free (see the module docs),
            // so attributing any of it to this backend would
            // fabricate the generating/idle distinction.
            let token_series_seen =
                latest.tokens.contains_key(&model) || baseline.tokens.contains_key(&model);
            let attribution_missed = tokens_delta == 0 && successes_delta > 0 && !token_series_seen;
            if attribution_missed {
                if self.miss_reported.insert(id.clone()) {
                    eprintln!(
                        "gateway stats: requests completed for backend '{id}' but its token \
                         series (model=\"{model}\") is absent from the gateway metrics — \
                         throughput for '{id}' is not measured (check the token-metric \
                         attribution)"
                    );
                }
            } else {
                self.miss_reported.remove(id);
            }
            let rate = if tokens_delta > 0 {
                Some(tokens_delta as f64 / span_secs)
            } else if attribution_missed {
                None
            } else {
                Some(0.0)
            };
            out.insert(
                id.clone(),
                LiveThroughput {
                    rate,
                    attribution_missed,
                },
            );
        }
        Some(out)
    }

    /// Per-backend rate series for the dashboard sparklines,
    /// derived from consecutive snapshot pairs: for each interval,
    /// requests per minute (total counter delta, rate-normalized)
    /// and tokens per second. Capped at the most recent
    /// [`SERIES_POINTS`] intervals. Returns None until two
    /// snapshots exist (same condition as [`Self::window_stats`]).
    pub fn series_stats(
        &self,
        backends: &[(String, u16)],
    ) -> Option<HashMap<String, BackendSeries>> {
        if self.snapshots.len() < 2 {
            return None;
        }
        let delta = |now: u64, then: u64| -> u64 {
            if now >= then {
                now - then
            } else {
                now
            }
        };
        let mut out: HashMap<String, BackendSeries> = backends
            .iter()
            .map(|(id, _)| {
                (
                    id.clone(),
                    BackendSeries {
                        req_per_min: Vec::new(),
                        toks_per_sec: Vec::new(),
                    },
                )
            })
            .collect();
        let snaps: Vec<&MetricsSnapshot> = self.snapshots.iter().collect();
        for pair in snaps.windows(2) {
            let (prev, cur) = (pair[0], pair[1]);
            let dt = cur.at.duration_since(prev.at).as_secs_f64();
            if dt <= 0.0 {
                continue;
            }
            for (id, port) in backends {
                let api_base = format!("http://127.0.0.1:{port}/v1");
                let model = format!("openai/{id}");
                let (cs, cf) = cur.deployments.get(&api_base).copied().unwrap_or((0, 0));
                let (ps, pf) = prev.deployments.get(&api_base).copied().unwrap_or((0, 0));
                let reqs = delta(cs, ps) + delta(cf, pf);
                let toks = delta(
                    cur.tokens.get(&model).copied().unwrap_or(0),
                    prev.tokens.get(&model).copied().unwrap_or(0),
                );
                let series = out.get_mut(id).expect("seeded above");
                series.req_per_min.push(reqs as f64 * 60.0 / dt);
                series.toks_per_sec.push(toks as f64 / dt);
            }
        }
        for series in out.values_mut() {
            for v in [&mut series.req_per_min, &mut series.toks_per_sec] {
                if v.len() > SERIES_POINTS {
                    v.drain(..v.len() - SERIES_POINTS);
                }
            }
        }
        Some(out)
    }
}

/// One llama-server `/slots` scrape for one backend: the
/// `n_decoded` counter of every slot currently processing.
/// Slots that are idle (or absent from the response) are not
/// recorded: an idle slot decodes nothing, and a slot that
/// finished between polls contributes nothing further — its
/// completion tokens are the gateway counters' job, and the
/// direct rate's job is the generation in progress now.
#[derive(Debug, Clone)]
pub struct SlotsSnapshot {
    pub at: Instant,
    /// Slot id -> tokens decoded so far by that slot's current
    /// request.
    pub decoded: HashMap<u64, u64>,
}

/// Per-backend live token rate polled directly from each
/// backend's llama-server `/slots` endpoint, on the same 15 s
/// cadence as the gateway poller (the caller polls and records;
/// this store only holds snapshots and derives rates).
///
/// Why this exists: the gateway's counters move only when a
/// request completes, so a backend mid-generation shows zero
/// counted tokens for the whole generation. `/slots` reports
/// `n_decoded` while generation is in progress, so the delta
/// over the same [`LIVE_WINDOW`] window is a genuine live rate.
/// A failed or missing `/slots` fetch records no snapshot: the
/// rate then goes stale (past [`LIVE_STALE_AFTER`]) and the
/// caller falls back to the gateway rate — an unavailable
/// signal renders as "—" via that fallback's absence, never as
/// a fabricated 0. No authentication is used: app-spawned
/// backends run with no `--api-key`, and the health probe
/// already GETs `/health` unauthenticated.
#[derive(Debug, Default)]
pub struct DirectSlotsStore {
    snapshots: HashMap<String, VecDeque<SlotsSnapshot>>,
}

impl DirectSlotsStore {
    pub fn record(&mut self, backend_id: &str, snap: SlotsSnapshot) {
        let ring = self.snapshots.entry(backend_id.to_string()).or_default();
        while let Some(front) = ring.front() {
            if snap.at.duration_since(front.at) > RETENTION {
                ring.pop_front();
            } else {
                break;
            }
        }
        ring.push_back(snap);
    }

    /// The live direct rate for one backend, in tokens per
    /// second over the recent [`LIVE_WINDOW`], or None when it
    /// is not measured: fewer than two snapshots, a covered
    /// span below [`LIVE_MIN_SPAN`], or a latest snapshot older
    /// than [`LIVE_STALE_AFTER`] (the poller stopped landing
    /// successful `/slots` fetches for this backend).
    ///
    /// Delta rule, per slot id (the same reset-tolerant rule as
    /// the gateway counters): a slot present in both the
    /// baseline and latest snapshots contributes
    /// `latest - baseline`, or `latest` alone when the counter
    /// decreased — a decreased counter means the slot was
    /// reused for a new request, which started from zero. A
    /// slot present only in the latest snapshot contributes
    /// its full count (its request started from zero inside
    /// the window, or before the first snapshot of it). A slot
    /// present only in the baseline contributes nothing: it
    /// finished, and its remaining tokens are unobserved.
    pub fn live_rate(&self, backend_id: &str) -> Option<f64> {
        let ring = self.snapshots.get(backend_id)?;
        if ring.len() < 2 {
            return None;
        }
        let latest = ring.back()?;
        if latest.at.elapsed() > LIVE_STALE_AFTER {
            return None;
        }
        let target = latest.at.checked_sub(LIVE_WINDOW).unwrap_or(latest.at);
        let baseline = match ring.iter().rev().find(|s| s.at <= target) {
            Some(b) if !std::ptr::eq(b, latest) => b,
            _ => ring.front()?,
        };
        if std::ptr::eq(baseline, latest) {
            return None;
        }
        let span_secs = latest.at.duration_since(baseline.at).as_secs_f64();
        if span_secs < LIVE_MIN_SPAN.as_secs_f64() {
            return None;
        }
        let mut tokens: u64 = 0;
        for (slot, now) in &latest.decoded {
            let then = baseline.decoded.get(slot).copied().unwrap_or(0);
            tokens += if *now >= then { now - then } else { *now };
        }
        Some(tokens as f64 / span_secs)
    }
}

/// Parse a llama-server `/slots` JSON body into a snapshot
/// (timestamp left at the epoch placeholder; the caller stamps
/// it). The body is a JSON array of slot objects; only slots
/// with `is_processing: true` and an integer `n_decoded`
/// contribute. Anything else — a non-array body, a slot
/// without those fields, a malformed entry — yields no
/// snapshot (None), never a partial one: a partial snapshot
/// would read as a counter drop and overcount on the next
/// poll. Returns None (not an empty snapshot) when the body
/// is not a JSON array at all, so the caller can tell an
/// unavailable endpoint from an idle backend: an idle backend
/// answers with an array whose slots are all non-processing
/// and records an empty (measured-idle) snapshot.
pub fn parse_slots(body: &str, at: Instant) -> Option<SlotsSnapshot> {
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    let slots = parsed.as_array()?;
    let mut decoded = HashMap::new();
    for slot in slots {
        let obj = slot.as_object()?;
        if obj
            .get("is_processing")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            continue;
        }
        let id = obj.get("id").and_then(serde_json::Value::as_u64)?;
        let n = obj.get("n_decoded").and_then(serde_json::Value::as_u64)?;
        decoded.insert(id, n);
    }
    Some(SlotsSnapshot { at, decoded })
}

/// Sparkline series for one backend (see
/// [`GatewayStatsStore::series_stats`]).
#[derive(Debug, Clone, PartialEq)]
pub struct BackendSeries {
    /// Requests per minute per poll interval, oldest first.
    pub req_per_min: Vec<f64>,
    /// Tokens per second per poll interval, oldest first.
    pub toks_per_sec: Vec<f64>,
}

/// Sparkline points retained per backend (~12 minutes at the
/// 15 s poll cadence).
const SERIES_POINTS: usize = 48;

/// Parse Prometheus text exposition into a snapshot (timestamp left
/// at the epoch placeholder; the caller stamps it). Unknown series
/// are ignored; malformed lines are skipped, never fatal.
pub fn parse_metrics(text: &str, at: Instant) -> MetricsSnapshot {
    let mut snap = MetricsSnapshot {
        at,
        deployments: HashMap::new(),
        tokens: HashMap::new(),
        in_flight_total: 0.0,
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
            "litellm_deployment_success_responses" | "litellm_deployment_failure_responses" => {
                let Some(api_base) = label_value(labels, "api_base") else {
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
                let Some(model) = label_value(labels, "model") else {
                    continue;
                };
                *snap.tokens.entry(model).or_insert(0) += value;
            }
            "litellm_in_flight_requests" => {
                // The gauge carries no labels in real LiteLLM
                // exposition (a bare `litellm_in_flight_requests
                // <n>` line); sum every line into the
                // gateway-wide total and never key it by a
                // label — there is no per-backend number here.
                let _ = labels;
                snap.in_flight_total += value as f64;
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
litellm_in_flight_requests 2.0
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
        // The in-flight gauge is bare and label-free: it parses
        // into the gateway-wide total, never a per-backend map.
        assert_eq!(snap.in_flight_total, 2.0);
    }

    #[test]
    fn window_deltas_use_oldest_snapshot_within_retention_as_baseline() {
        let t0 = Instant::now();
        let mut store = GatewayStatsStore::default();
        let mk = |at: Instant, success: u64, failure: u64, tokens: u64| MetricsSnapshot {
            at,
            deployments: HashMap::from([("http://127.0.0.1:8081/v1".into(), (success, failure))]),
            tokens: HashMap::from([("openai/planner".into(), tokens)]),
            in_flight_total: 0.0,
        };
        store.record(mk(t0, 10, 1, 1000));
        // 30 minutes later: still inside the hour, baseline stays t0.
        store.record(mk(t0 + Duration::from_secs(1800), 25, 2, 4600));
        let stats = store
            .window_stats(&[("planner".into(), 8081)])
            .expect("two snapshots");
        let w = stats["planner"];
        // Requests is the TOTAL: +15 successes and +1 failure.
        assert_eq!(w.requests, 16);
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
            in_flight_total: 0.0,
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
            in_flight_total: 0.0,
        };
        store.record(mk(t0, 900));
        store.record(mk(t0 + Duration::from_secs(60), 7));
        let stats = store
            .window_stats(&[("planner".into(), 8081)])
            .expect("snapshots");
        assert_eq!(stats["planner"].requests, 7);
    }

    #[test]
    fn series_rates_come_from_consecutive_deltas() {
        let t0 = Instant::now();
        let mut store = GatewayStatsStore::default();
        let mk = |at: Instant, success: u64, failure: u64, tokens: u64| MetricsSnapshot {
            at,
            deployments: HashMap::from([("http://127.0.0.1:8081/v1".into(), (success, failure))]),
            tokens: HashMap::from([("openai/planner".into(), tokens)]),
            in_flight_total: 0.0,
        };
        store.record(mk(t0, 10, 0, 600));
        // 60 s later: +30 successes, +6 failures, +3600 tokens.
        store.record(mk(t0 + Duration::from_secs(60), 40, 6, 4200));
        // 30 s later: no further traffic.
        store.record(mk(t0 + Duration::from_secs(90), 40, 6, 4200));
        let series = store
            .series_stats(&[("planner".into(), 8081)])
            .expect("three snapshots");
        let s = &series["planner"];
        assert_eq!(s.req_per_min, vec![36.0, 0.0]);
        assert_eq!(s.toks_per_sec, vec![60.0, 0.0]);
    }

    #[test]
    fn one_snapshot_is_not_a_window() {
        let mut store = GatewayStatsStore::default();
        store.record(MetricsSnapshot {
            at: Instant::now(),
            deployments: HashMap::new(),
            tokens: HashMap::new(),
            in_flight_total: 0.0,
        });
        assert!(store.window_stats(&[("planner".into(), 8081)]).is_none());
    }

    fn live_snap(at: Instant, success: u64, tokens: u64) -> MetricsSnapshot {
        MetricsSnapshot {
            at,
            deployments: HashMap::from([("http://127.0.0.1:8081/v1".into(), (success, 0))]),
            tokens: HashMap::from([("openai/planner".into(), tokens)]),
            in_flight_total: 0.0,
        }
    }

    #[test]
    fn live_rate_reflects_recent_generation_after_an_idle_hour() {
        // The Oct 22, 2026 field scenario: an idle hour of polls,
        // then the backend starts generating — 1,500 tokens
        // counted over the last 120 s (12.5 tok/s). The live rate
        // must read that activity; the trailing-hour formula the
        // tile used before (window tokens / window_secs, the
        // value `window_stats` still feeds requests/errors)
        // dilutes the same activity to 0.4 tok/s, and to a
        // rounded 0.0 whenever under ~180 tokens land in the
        // hour (the repro this fix replaces).
        // Times are offsets from a base instant (the pattern the
        // other tests here use): subtracting an hour from
        // Instant::now() can underflow a young monotonic clock.
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let mut store = GatewayStatsStore::default();
        store.record(live_snap(at(0), 40, 100_000));
        store.record(live_snap(at(100), 40, 100_000));
        store.record(live_snap(at(3580), 40, 100_000));
        store.record(live_snap(at(3640), 44, 100_750));
        store.record(live_snap(at(3700), 48, 101_500));
        let live = store
            .live_throughput(&[("planner".into(), 8081)])
            .expect("fresh snapshots spanning the live window");
        assert_eq!(live["planner"].rate, Some(12.5));
        assert!(!live["planner"].attribution_missed);
        let w = store
            .window_stats(&[("planner".into(), 8081)])
            .expect("window")["planner"];
        let hour_tile = (w.tokens as f64 / w.window_secs * 10.0).round() / 10.0;
        assert!(
            hour_tile < 12.5 / 10.0,
            "the hour formula understates live generation by >10x (got {hour_tile})"
        );
    }

    #[test]
    fn gateway_wide_in_flight_does_not_mask_a_per_backend_rate() {
        // The retired per-backend branch read the gateway-wide,
        // label-free gauge as if it belonged to one backend.
        // Re-scoped: a nonzero gateway-wide total must NOT make
        // any single backend's gateway rate unmeasured — the
        // gauge cannot say which backend (if any) is generating.
        // Mid-generation distinction is the direct /slots
        // store's job (tests below), not this gauge's.
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let mut store = GatewayStatsStore::default();
        for secs in [0, 60, 120] {
            let mut snap = live_snap(at(secs), 40, 100_000);
            snap.in_flight_total = 2.0;
            store.record(snap);
        }
        let live = store
            .live_throughput(&[("planner".into(), 8081)])
            .expect("snapshots");
        assert_eq!(
            live["planner"].rate,
            Some(0.0),
            "gateway-wide in-flight is not a per-backend signal"
        );
        assert!(!live["planner"].attribution_missed);
    }

    #[test]
    fn direct_slots_rate_comes_from_processing_slot_deltas() {
        // The Nov 3, 2026 case: a backend mid-generation, whose
        // gateway counters have not moved (no completion yet),
        // reads a live rate from /slots: slot 0 decodes 600
        // tokens over the 120 s window = 5.0 tok/s, while an
        // idle slot's entry is ignored.
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let mut store = DirectSlotsStore::default();
        let snap = |secs: u64, decoded: &[(u64, u64)]| SlotsSnapshot {
            at: at(secs),
            decoded: decoded.iter().map(|(k, v)| (*k, *v)).collect(),
        };
        store.record("planner", snap(0, &[(0, 100)]));
        store.record("planner", snap(120, &[(0, 700)]));
        assert_eq!(store.live_rate("planner"), Some(5.0));
        // Idle backend: processing slots absent from both
        // snapshots, but the snapshots themselves landed — a
        // measured 0, not unknown.
        let mut idle = DirectSlotsStore::default();
        idle.record("planner", snap(0, &[]));
        idle.record("planner", snap(120, &[]));
        assert_eq!(idle.live_rate("planner"), Some(0.0));
    }

    #[test]
    fn direct_slots_rate_handles_slot_reuse_resets() {
        // Slot 0 finishes one request and is reused for another
        // inside the window: its counter drops (900 -> 50), and
        // the delta counts from zero (50), not as a negative
        // or as the drop — the same reset rule as the gateway
        // counters.
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let mut store = DirectSlotsStore::default();
        let snap = |secs: u64, decoded: &[(u64, u64)]| SlotsSnapshot {
            at: at(secs),
            decoded: decoded.iter().map(|(k, v)| (*k, *v)).collect(),
        };
        store.record("planner", snap(0, &[(0, 900)]));
        store.record("planner", snap(120, &[(0, 50)]));
        assert_eq!(store.live_rate("planner"), Some(50.0 / 120.0));
    }

    #[test]
    fn direct_slots_rate_is_absent_without_a_usable_window() {
        // One snapshot only, and a too-short span, both leave
        // the rate unmeasured — the caller falls back to the
        // gateway rate, which may itself be absent ("—").
        let t0 = Instant::now();
        let mut store = DirectSlotsStore::default();
        let snap = |secs: u64, decoded: &[(u64, u64)]| SlotsSnapshot {
            at: t0 + Duration::from_secs(secs),
            decoded: decoded.iter().map(|(k, v)| (*k, *v)).collect(),
        };
        store.record("planner", snap(0, &[(0, 10)]));
        assert_eq!(store.live_rate("planner"), None);
        store.record("planner", snap(15, &[(0, 200)]));
        assert_eq!(
            store.live_rate("planner"),
            None,
            "span below the 30 s minimum"
        );
    }

    #[test]
    fn parse_slots_counts_processing_slots_only() {
        let t0 = Instant::now();
        let body = r#"[{"id":0,"n_decoded":123,"is_processing":true},{"id":1,"n_decoded":999,"is_processing":false},{"id":2,"n_decoded":5,"is_processing":true}]"#;
        let snap = parse_slots(body, t0).expect("array body parses");
        assert_eq!(snap.decoded.get(&0), Some(&123));
        assert_eq!(snap.decoded.get(&2), Some(&5));
        assert!(!snap.decoded.contains_key(&1), "idle slots decode nothing");
        // An idle backend answers an array with no processing
        // slots: an empty snapshot (measured idle), not None.
        let idle = parse_slots(r#"[{"id":0,"n_decoded":0,"is_processing":false}]"#, t0)
            .expect("idle array parses");
        assert!(idle.decoded.is_empty());
        // A non-array body (error page, disabled endpoint
        // HTML, truncated JSON) is unavailable, not idle.
        assert!(parse_slots("not json", t0).is_none());
        assert!(parse_slots(r#"{"error":"slots disabled"}"#, t0).is_none());
        assert!(parse_slots(r#"[{"id":0,"is_processing":true}]"#, t0).is_none());
    }

    #[test]
    fn live_rate_flags_a_missed_token_attribution() {
        // Requests complete (the deployment counter, keyed by
        // api_base, moves) but the backend's token series never
        // appears in either snapshot: the attribution lookup
        // missed. That must surface as unmeasured + flagged —
        // never as a silent 0 (a new variant of the #19/#58
        // failure would look exactly like this).
        let t0 = Instant::now();
        let mk = |at: Instant, success: u64| MetricsSnapshot {
            at,
            deployments: HashMap::from([("http://127.0.0.1:8081/v1".into(), (success, 0))]),
            tokens: HashMap::new(),
            in_flight_total: 0.0,
        };
        let mut store = GatewayStatsStore::default();
        store.record(mk(t0, 40));
        store.record(mk(t0 + Duration::from_secs(120), 47));
        let live = store
            .live_throughput(&[("planner".into(), 8081)])
            .expect("snapshots");
        assert_eq!(live["planner"].rate, None);
        assert!(live["planner"].attribution_missed);
    }

    #[test]
    fn live_rate_needs_span_and_fresh_snapshots() {
        let now = Instant::now();
        // Two snapshots 15 s apart: span below the minimum, so a
        // single completion burst cannot read as a spike rate.
        let mut store = GatewayStatsStore::default();
        store.record(live_snap(now, 40, 100_000));
        store.record(live_snap(now + Duration::from_secs(15), 41, 100_300));
        assert!(store.live_throughput(&[("planner".into(), 8081)]).is_none());
        // Snapshots exist and span enough, but the latest is
        // 100 s old — past the staleness bound: the poller
        // stopped landing scrapes, and a frozen figure is not a
        // live rate.
        let mut store = GatewayStatsStore::default();
        store.record(live_snap(now - Duration::from_secs(200), 40, 100_000));
        store.record(live_snap(now - Duration::from_secs(100), 41, 100_300));
        assert!(store.live_throughput(&[("planner".into(), 8081)]).is_none());
    }

    #[test]
    fn live_rate_counts_a_counter_reset_from_zero() {
        // Gateway restarted inside the live window: the token
        // counter restarted from zero, and everything counted
        // since is inside the window.
        let t0 = Instant::now();
        let mut store = GatewayStatsStore::default();
        store.record(live_snap(t0, 40, 90_000));
        store.record(live_snap(t0 + Duration::from_secs(120), 3, 600));
        let live = store
            .live_throughput(&[("planner".into(), 8081)])
            .expect("snapshots");
        assert_eq!(live["planner"].rate, Some(5.0));
    }
}
