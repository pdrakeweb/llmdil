//! `BackendConfig.restart_policy`: a backend that exited unexpectedly is
//! started again with exponential back-off, up to `max_retries` times.
//!
//! [`on_failed_run`] (the monitor tick) schedules an attempt on the backend's
//! registry record; [`run_due`] (the shell loop, after each tick) starts
//! it through the normal start path, preflight included. An operator
//! stop, or any other lifecycle operation on the backend, drops the
//! record and with it the pending attempt. Temporary bench/optimize
//! backends are not configured, and service-supervised backends are the
//! SCM's to restart: neither ever gets a record.

use std::sync::Arc;
use std::time::{Duration, Instant};

use manager_config::RestartPolicy;
use manager_supervisor::{BackoffPolicy, DEVICE_UNAVAILABLE_REASON};

use crate::backend_lifecycle::{start_backend, BackendOpOpts};
use crate::lifecycle::{self, backend_key, AutoRestart, Failure, Intent, LifecycleState, Origin};
use crate::{now_ms, ApiError, AppState, EventSink};

/// A restarted backend that ran this long is stable: the next crash
/// starts counting from zero.
const STABLE_RUN: Duration = Duration::from_secs(10 * 60);

/// Back-off ceiling between two attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// Retry delay when another operation held the backend.
const BUSY_RETRY_MS: i64 = 5_000;

/// What follows a failed run (pure): the next attempt, or giving up once
/// `max_retries` attempts were made. `None` when the policy is off.
fn next_attempt(
    prev: Option<AutoRestart>,
    policy: &RestartPolicy,
    now_ms: i64,
) -> Option<AutoRestart> {
    let max = policy.max_retries;
    if max == 0 {
        return None;
    }
    let made = prev.map_or(0, |p| p.attempt);
    let gave_up = made >= max;
    let backoff = BackoffPolicy {
        base: Duration::from_secs(policy.backoff_base_secs),
        max: MAX_BACKOFF,
        max_retries: max,
    };
    let delay = backoff.delay_for_attempt(made).as_millis() as i64;
    Some(AutoRestart {
        attempt: if gave_up { made } else { made + 1 },
        max,
        next_at_ms: (!gave_up).then_some(now_ms + delay),
        gave_up,
        began: None,
    })
}

/// Backend `id` exited unexpectedly, or an attempt to restart it failed:
/// schedule the next attempt, or give up. Returns the failure reason,
/// extended when no further attempt will be made.
pub(super) async fn on_failed_run(state: &AppState, id: &str, reason: String) -> String {
    let policy = {
        let cfg = state.config.read().await;
        cfg.backends
            .iter()
            .find(|b| b.id == id && b.enabled)
            .map(|b| b.restart_policy.clone())
    };
    let key = backend_key(id);
    let now = now_ms() as i64;
    let mut next = policy.and_then(|p| next_attempt(state.lifecycle.auto_restart(&key), &p, now));
    // The device does not come back on a timer (see
    // `Supervisor::crash_delay`): the remedy is the GPU, then a start.
    if reason.contains(DEVICE_UNAVAILABLE_REASON) {
        if let Some(n) = &mut next {
            n.next_at_ms = None;
            n.gave_up = true;
            n.attempt = n.attempt.saturating_sub(1);
        }
    }
    state.lifecycle.set_auto_restart(&key, next);
    let Some(next) = next else {
        return reason;
    };
    let mark = |event: &str, detail: &str| {
        lifecycle::mark(
            &state.data_dir,
            &key,
            event,
            Some(detail),
            Some(Origin::RestartPolicy),
        );
    };
    match next.next_at_ms {
        Some(at) => {
            mark(
                "automatic restart scheduled",
                &format!(
                    "attempt {} of {} in {} s",
                    next.attempt,
                    next.max,
                    (at - now) / 1000
                ),
            );
            reason
        }
        None => {
            let reason = format!(
                "{reason} (gave up after {} automatic restart{})",
                next.attempt,
                if next.attempt == 1 { "" } else { "s" }
            );
            mark("automatic restarts ended", &reason);
            state
                .lifecycle
                .set_failure(&key, Failure::now(reason.clone(), None));
            reason
        }
    }
}

/// Start every attempt that is due, drop records that no longer apply,
/// and forget the attempts of a backend that ran [`STABLE_RUN`].
pub async fn run_due(state: &Arc<AppState>, sink: &Arc<dyn EventSink>) {
    let records: Vec<_> = state
        .lifecycle
        .records()
        .into_iter()
        .filter(|(_, r)| r.auto_restart.is_some() && r.transition.is_none())
        .collect();
    if records.is_empty() || state.lifecycle.app_exiting() {
        return;
    }
    let cfg = state.config.read().await.clone();
    let live = lifecycle::backend_liveness_for(state, &cfg).await;
    for (key, rec) in records {
        let (Some(id), Some(auto)) = (lifecycle::backend_id_of(&key), rec.auto_restart) else {
            continue;
        };
        let wanted = rec.intent != Some(Intent::Stopped)
            && cfg.backends.iter().any(|b| b.id == id && b.enabled);
        let Some(row) = live.get(id).filter(|_| wanted) else {
            state.lifecycle.set_auto_restart(&key, None);
            continue;
        };
        match auto.next_at_ms {
            // Brought back some other way (adopted, or a service now).
            Some(_) if row.service_managed || row.state.is_live() => {
                state.lifecycle.set_auto_restart(&key, None);
            }
            Some(at) if at <= now_ms() as i64 => {
                let begun = AutoRestart {
                    next_at_ms: None,
                    began: Some(Instant::now()),
                    ..auto
                };
                state.lifecycle.set_auto_restart(&key, Some(begun));
                let (state, sink, id) = (Arc::clone(state), Arc::clone(sink), id.to_string());
                tokio::spawn(async move { attempt(&state, &sink, &id).await });
            }
            None if row.state == LifecycleState::Running
                && auto.began.is_some_and(|b| b.elapsed() >= STABLE_RUN) =>
            {
                state.lifecycle.set_auto_restart(&key, None);
            }
            _ => {}
        }
    }
}

/// One attempt: the normal start, then the next step when it failed.
async fn attempt(state: &Arc<AppState>, sink: &Arc<dyn EventSink>, id: &str) {
    let Err(e) = start_backend(state, sink, id, BackendOpOpts::new(Origin::RestartPolicy)).await
    else {
        return;
    };
    let key = backend_key(id);
    let Some(current) = state.lifecycle.auto_restart(&key) else {
        return; // cancelled
    };
    if matches!(e, ApiError::Busy(_)) {
        let retry = AutoRestart {
            next_at_ms: Some(now_ms() as i64 + BUSY_RETRY_MS),
            ..current
        };
        state.lifecycle.set_auto_restart(&key, Some(retry));
        return;
    }
    let reason = format!("automatic restart failed: {e}");
    lifecycle::mark(
        &state.data_dir,
        &key,
        "automatic restart failed",
        Some(&e.to_string()),
        Some(Origin::RestartPolicy),
    );
    let reason = on_failed_run(state, id, reason).await;
    lifecycle::emit(
        sink.as_ref(),
        &key,
        "failed",
        LifecycleState::Failed,
        Some(Origin::RestartPolicy),
        Some(&reason),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{advanced_test_state, test_cfg, test_sink};

    fn policy(max_retries: u32) -> RestartPolicy {
        RestartPolicy {
            max_retries,
            backoff_base_secs: 2,
        }
    }

    #[test]
    fn attempts_back_off_exponentially_and_end_at_the_maximum() {
        let mut record = None;
        let mut delays = Vec::new();
        for n in 1..=3 {
            record = next_attempt(record, &policy(3), 1_000);
            let r = record.unwrap();
            assert_eq!((r.attempt, r.max, r.gave_up), (n, 3, false));
            delays.push(r.next_at_ms.unwrap() - 1_000);
        }
        assert_eq!(delays, [2_000, 4_000, 8_000]);
        let r = next_attempt(record, &policy(3), 1_000).unwrap();
        assert_eq!((r.attempt, r.next_at_ms, r.gave_up), (3, None, true));
        assert_eq!(next_attempt(None, &policy(0), 0), None, "0 disables");
        let far = AutoRestart { attempt: 60, ..r };
        let r = next_attempt(Some(far), &policy(99), 0).unwrap();
        assert_eq!(r.next_at_ms, Some(MAX_BACKOFF.as_millis() as i64));
    }

    #[test]
    fn the_row_payload_has_exactly_the_documented_fields() {
        let r = next_attempt(None, &policy(5), 0).unwrap();
        assert_eq!(
            serde_json::to_value(r).unwrap(),
            serde_json::json!({
                "attempt": 1, "max": 5, "next_at_ms": 2_000, "gave_up": false
            })
        );
    }

    /// A crashed backend with a stopped-by-nobody intent, and its id.
    async fn crashed() -> (Arc<AppState>, std::path::PathBuf, String) {
        let mut cfg = test_cfg();
        cfg.backends.truncate(1);
        cfg.backends[0].enabled = true;
        cfg.backends[0].restart_policy = policy(2);
        let id = cfg.backends[0].id.clone();
        let (state, dir) = advanced_test_state(cfg);
        let key = backend_key(&id);
        state.lifecycle.set_intent(&key, Intent::Running);
        state
            .lifecycle
            .set_failure(&key, Failure::now("exited unexpectedly", None));
        (state, dir, id)
    }

    fn make_due(state: &AppState, key: &str) {
        let r = state.lifecycle.auto_restart(key).unwrap();
        state.lifecycle.set_auto_restart(
            key,
            Some(AutoRestart {
                next_at_ms: Some(0),
                ..r
            }),
        );
    }

    async fn settled(state: &AppState, key: &str, what: &str, f: impl Fn(AutoRestart) -> bool) {
        for _ in 0..500 {
            if state.lifecycle.auto_restart(key).is_some_and(&f) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out: {what}: {:?}", state.lifecycle.auto_restart(key));
    }

    /// The fixture has no llama-server, so every attempt is refused by
    /// the normal preflight: the policy retries, then gives up as failed
    /// with the reason.
    #[tokio::test]
    async fn a_crash_is_retried_through_the_preflight_until_giving_up() {
        let (state, dir, id) = crashed().await;
        let (sink, key) = (test_sink(), backend_key(&id));
        let reason = on_failed_run(&state, &id, "exited unexpectedly".into()).await;
        assert_eq!(reason, "exited unexpectedly");
        let rows = crate::op_list_backends(&state, &sink).await.unwrap();
        assert_eq!(rows["backends"][0]["auto_restart"]["attempt"], 1);
        assert_eq!(rows["backends"][0]["auto_restart"]["max"], 2);
        // Not due yet: nothing starts.
        run_due(&state, &sink).await;
        assert!(state
            .lifecycle
            .auto_restart(&key)
            .unwrap()
            .next_at_ms
            .is_some());
        make_due(&state, &key);
        run_due(&state, &sink).await;
        settled(&state, &key, "attempt 2 scheduled", |r| {
            r.attempt == 2 && r.next_at_ms.is_some()
        })
        .await;
        make_due(&state, &key);
        run_due(&state, &sink).await;
        settled(&state, &key, "gave up", |r| r.gave_up).await;
        let rec = state.lifecycle.record(&key);
        let reason = rec.failure.unwrap().reason;
        assert!(
            reason.contains("gave up after 2 automatic restarts"),
            "{reason}"
        );
        let live = lifecycle::backend_liveness(&state).await;
        assert_eq!(live[&id].state, LifecycleState::Failed);
        let log = std::fs::read_to_string(lifecycle::log_path(&dir, &key)).unwrap();
        assert!(
            log.contains("automatic restart scheduled: attempt 1 of 2 in 2 s — by restart-policy"),
            "{log}"
        );
        assert!(log.contains("automatic restarts ended"), "{log}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_stop_or_a_disable_cancels_the_pending_restart() {
        let (state, dir, id) = crashed().await;
        let (sink, key) = (test_sink(), backend_key(&id));
        on_failed_run(&state, &id, "exited".into()).await;
        state.lifecycle.set_intent(&key, Intent::Stopped);
        assert_eq!(state.lifecycle.auto_restart(&key), None);

        state.lifecycle.set_intent(&key, Intent::Running);
        on_failed_run(&state, &id, "exited".into()).await;
        state.config.write().await.backends[0].enabled = false;
        run_due(&state, &sink).await;
        assert_eq!(state.lifecycle.auto_restart(&key), None);
        // A disabled backend that crashes is not restarted either.
        on_failed_run(&state, &id, "exited".into()).await;
        assert_eq!(state.lifecycle.auto_restart(&key), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn another_operation_supersedes_the_restarts() {
        let (state, dir, id) = crashed().await;
        let (sink, key) = (test_sink(), backend_key(&id));
        on_failed_run(&state, &id, "exited".into()).await;
        let guard = lifecycle::begin(
            &state,
            &sink,
            &key,
            lifecycle::TransitionKind::Starting,
            Origin::Ui,
            None,
        )
        .await
        .unwrap();
        drop(guard);
        assert_eq!(state.lifecycle.auto_restart(&key), None);
        // An attempt whose record is gone by the time it holds the
        // backend starts nothing.
        let opts = BackendOpOpts {
            preflight: false,
            ..BackendOpOpts::new(Origin::RestartPolicy)
        };
        let e = start_backend(&state, &sink, &id, opts).await.unwrap_err();
        assert!(e.to_string().contains("cancelled"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_unavailable_gpu_is_never_retried() {
        let (state, dir, id) = crashed().await;
        let reason = on_failed_run(
            &state,
            &id,
            format!("exited unexpectedly. {DEVICE_UNAVAILABLE_REASON}"),
        )
        .await;
        assert!(reason.contains("gave up after 0"), "{reason}");
        let r = state.lifecycle.auto_restart(&backend_key(&id)).unwrap();
        assert!(r.gave_up && r.next_at_ms.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_stable_run_resets_the_attempts() {
        let (state, dir, id) = crashed().await;
        let (sink, key) = (test_sink(), backend_key(&id));
        let began = Instant::now().checked_sub(STABLE_RUN);
        // Skipped where the monotonic clock is younger than the window.
        if let Some(began) = began {
            on_failed_run(&state, &id, "exited".into()).await;
            let pid = std::process::id();
            {
                crate::ensure_backend_specs(&state).await;
                let mut sup = state.supervisor().await.lock().await;
                sup.adopt(&id, pid).unwrap();
            }
            let r = state.lifecycle.auto_restart(&key).unwrap();
            let running = |began| AutoRestart {
                next_at_ms: None,
                began,
                ..r
            };
            state
                .lifecycle
                .set_auto_restart(&key, Some(running(Some(Instant::now()))));
            run_due(&state, &sink).await;
            assert!(state.lifecycle.auto_restart(&key).is_some());
            state
                .lifecycle
                .set_auto_restart(&key, Some(running(Some(began))));
            run_due(&state, &sink).await;
            assert_eq!(state.lifecycle.auto_restart(&key), None);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
