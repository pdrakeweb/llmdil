use super::*;
use crate::test_support::*;

#[test]
fn evaluate_backend_offload_places_and_overrides() {
    let mut cfg = test_cfg();
    let b = serve_model(
        &mut cfg,
        "coder",
        "qwen3-coder-30b",
        "models/Qwen3-Coder-Next-UD-Q4_K_M.gguf",
        Some("--n-cpu-moe 46"),
    );
    // The coder backend is unsplit in the test config, so it
    // places on GPU 0; make GPU 0 a Pascal card.
    let mut gpus = test_gpus();
    gpus[0].compute_capability = (6, 0);
    let journal = activation::ActivationJournal::default();
    let eval =
        evaluate_backend_offload(&cfg, &b, &gpus, &journal).expect("coder backend evaluates");
    assert_eq!(eval.layers, 46);
    assert_eq!(eval.moe_layers, 48);
    assert_eq!(eval.status, manager_models::OffloadStatus::Blocked);
    assert_eq!(eval.restricting_cc, Some((6, 0)));
    assert_eq!(eval.nearest_verified_layers, 48);
    assert!(!eval.has_override);
    assert!(!eval.overridden);

    // A journaled override for this backend, position, and
    // placed-CC set flips overridden (and only that).
    let journal = activation::ActivationJournal {
        offload_overrides: vec![activation::OffloadOverride {
            backend_id: "coder".to_string(),
            layers: 46,
            placed_ccs: vec![(6, 0)],
            ts: "2026-10-17T00:00:00Z".to_string(),
        }],
        ..Default::default()
    };
    let eval =
        evaluate_backend_offload(&cfg, &b, &gpus, &journal).expect("coder backend evaluates");
    assert!(eval.has_override);
    assert!(eval.overridden);

    // Same position on an unrestricted GPU: no restriction
    // matches, nothing is blocked.
    let gpus = test_gpus();
    let eval =
        evaluate_backend_offload(&cfg, &b, &gpus, &journal).expect("coder backend evaluates");
    assert_eq!(eval.restricting_cc, None);
    assert_ne!(eval.status, manager_models::OffloadStatus::Blocked);
}

/// State for the guardrail-override op tests: the `coder`
/// backend serves Coder-Next at a raw-flags position, with the
/// telemetry inventory pinned so GPU 0 is a Pascal (6,0) card —
/// the #37 field configuration.
fn guardrail_override_test_state(raw_flags: &str) -> (Arc<AppState>, PathBuf) {
    let mut cfg = test_cfg();
    serve_model(
        &mut cfg,
        "coder",
        "qwen3-coder-30b",
        "models/Qwen3-Coder-Next-UD-Q4_K_M.gguf",
        Some(raw_flags),
    );
    // The preflight refuses a disabled backend before it ever
    // reaches the guardrail gate; the test fleet's coder ships
    // disabled, so enable it here.
    cfg.backends
        .iter_mut()
        .find(|b| b.id == "coder")
        .unwrap()
        .enabled = true;
    let (state, dir) = advanced_test_state(cfg);
    let mut gpus = test_gpus();
    gpus[0].compute_capability = (6, 0);
    let store = state
        .telemetry
        .get()
        .expect("advanced_test_state pins telemetry");
    lock_telemetry(store).set_inventory(gpus);
    (state, dir)
}

/// The launch preflight's verdict on the guardrail alone, for
/// the saved `coder` backend: `Some(reason)` when the start is
/// refused for the guardrail reason. Later preflight gates
/// (llama-server presence, the model file) fail in this test
/// environment for unrelated reasons, so the assertion is on
/// the guardrail reason's presence, never on overall success.
async fn guardrail_preflight_reason(state: &Arc<AppState>) -> Option<String> {
    let cfg = state.config.read().await.clone();
    let b = cfg
        .backends
        .iter()
        .find(|b| b.id == "coder")
        .unwrap()
        .clone();
    match backend_start_preflight_for(state, &cfg, &b, false).await {
        Err(e) => {
            let msg = e.to_string();
            msg.contains("outside the verified range").then_some(msg)
        }
        Ok(()) => None,
    }
}

#[tokio::test]
async fn offload_override_ops_round_trip_set_then_clear() {
    // The deliberate-override round trip (guardrail-override
    // UI): a saved blocked position refuses starts; setting the
    // override for exactly that position lets the preflight's
    // guardrail gate pass; clearing it blocks starts again.
    let (state, dir) = guardrail_override_test_state("--n-cpu-moe 46");
    let sink = test_sink();
    assert!(
        guardrail_preflight_reason(&state).await.is_some(),
        "46 of 48 on (6,0) refuses starts before any override"
    );
    let r = op_backend_set_offload_override(
        &state,
        &sink,
        "coder".to_string(),
        46,
        vec!["6.0".to_string()],
    )
    .await
    .unwrap();
    assert_eq!(r["backend_id"], "coder");
    assert_eq!(r["layers"], 46);
    assert_eq!(r["moe_layers"], 48);
    assert_eq!(r["placed_ccs"], json!(["6.0"]));
    assert_eq!(r["has_override"], true);
    assert_eq!(r["overridden"], true);
    // The journal record's shape: exactly the #37 key plus its
    // RFC 3339 timestamp, and nothing else was recorded.
    let journal = activation::load_journal(&dir);
    assert_eq!(journal.offload_overrides.len(), 1);
    let record = &journal.offload_overrides[0];
    assert_eq!(record.backend_id, "coder");
    assert_eq!(record.layers, 46);
    assert_eq!(record.placed_ccs, vec![(6, 0)]);
    assert!(
        chrono::DateTime::parse_from_rfc3339(&record.ts).is_ok(),
        "override timestamp is RFC 3339: {}",
        record.ts
    );
    // The op writes no config: the backend's flags are
    // untouched in the live config (and no config save was
    // needed to record the override).
    let cfg_now = state.config.read().await.clone();
    let b = cfg_now.backends.iter().find(|b| b.id == "coder").unwrap();
    assert_eq!(b.raw_flags.as_deref(), Some("--n-cpu-moe 46"));
    assert!(
        guardrail_preflight_reason(&state).await.is_none(),
        "the overridden position passes the preflight guardrail gate"
    );
    let r = op_backend_clear_offload_override(&state, &sink, "coder".to_string())
        .await
        .unwrap();
    assert_eq!(r["cleared"], true);
    assert_eq!(r["has_override"], false);
    assert_eq!(r["overridden"], false);
    let journal = activation::load_journal(&dir);
    assert!(journal.offload_overrides.is_empty());
    assert!(
        guardrail_preflight_reason(&state).await.is_some(),
        "after the clear the position is blocked again"
    );
    // Clearing with nothing standing is an idempotent success.
    let r = op_backend_clear_offload_override(&state, &sink, "coder".to_string())
        .await
        .unwrap();
    assert_eq!(r["cleared"], false);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn offload_override_set_refuses_key_mismatch() {
    // An override is keyed to exactly (backend, layers, placed
    // CC set): naming any other key at the set boundary refuses
    // and records nothing, and a recorded key covers no other.
    let (state, dir) = guardrail_override_test_state("--n-cpu-moe 46");
    let sink = test_sink();
    let r = op_backend_set_offload_override(
        &state,
        &sink,
        "coder".to_string(),
        45,
        vec!["6.0".to_string()],
    )
    .await;
    match r {
        Err(ApiError::InvalidInput(msg)) => {
            assert!(msg.contains("do not match"), "{msg}")
        }
        other => panic!("expected the layers-mismatch refusal, got {other:?}"),
    }
    let r = op_backend_set_offload_override(
        &state,
        &sink,
        "coder".to_string(),
        46,
        vec!["8.6".to_string()],
    )
    .await;
    match r {
        Err(ApiError::InvalidInput(msg)) => {
            assert!(msg.contains("does not match"), "{msg}")
        }
        other => panic!("expected the placed-CC mismatch refusal, got {other:?}"),
    }
    // A superset placement is a different key, not a wider one.
    let r = op_backend_set_offload_override(
        &state,
        &sink,
        "coder".to_string(),
        46,
        vec!["6.0".to_string(), "8.6".to_string()],
    )
    .await;
    assert!(matches!(r, Err(ApiError::InvalidInput(_))), "{r:?}");
    for bad in ["6", "6.0.1", "x.y", ""] {
        let r = op_backend_set_offload_override(
            &state,
            &sink,
            "coder".to_string(),
            46,
            vec![bad.to_string()],
        )
        .await;
        assert!(
            matches!(r, Err(ApiError::InvalidInput(_))),
            "{bad:?}: {r:?}"
        );
    }
    let journal = activation::load_journal(&dir);
    assert!(
        journal.offload_overrides.is_empty(),
        "refused sets record nothing"
    );
    // The exact key records; neighboring keys do not cover.
    op_backend_set_offload_override(
        &state,
        &sink,
        "coder".to_string(),
        46,
        vec!["6.0".to_string()],
    )
    .await
    .unwrap();
    let journal = activation::load_journal(&dir);
    assert!(activation::offload_override_covers(
        &journal,
        "coder",
        46,
        &[(6, 0)]
    ));
    assert!(!activation::offload_override_covers(
        &journal,
        "coder",
        45,
        &[(6, 0)]
    ));
    assert!(!activation::offload_override_covers(
        &journal,
        "coder",
        46,
        &[(8, 6)]
    ));
    assert!(!activation::offload_override_covers(
        &journal,
        "coder",
        46,
        &[(6, 0), (8, 6)]
    ));
    assert!(!activation::offload_override_covers(
        &journal,
        "planner",
        46,
        &[(6, 0)]
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn offload_override_set_refuses_non_blocked_and_unknown() {
    let sink = test_sink();
    // An endpoint position is verified on (6,0): there is
    // nothing to override, and the op refuses rather than
    // leaving a standing record no reader could see.
    let (state, dir) = guardrail_override_test_state("-ot exps=CPU");
    let r = op_backend_set_offload_override(
        &state,
        &sink,
        "coder".to_string(),
        48,
        vec!["6.0".to_string()],
    )
    .await;
    match r {
        Err(ApiError::InvalidInput(msg)) => {
            assert!(msg.contains("not guardrail-blocked"), "{msg}")
        }
        other => panic!("expected the non-blocked refusal, got {other:?}"),
    }
    let journal = activation::load_journal(&dir);
    assert!(journal.offload_overrides.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
    // Unknown backend: NotFound from both operations.
    let (state, dir) = guardrail_override_test_state("--n-cpu-moe 46");
    let r = op_backend_set_offload_override(
        &state,
        &sink,
        "nope".to_string(),
        46,
        vec!["6.0".to_string()],
    )
    .await;
    assert!(matches!(r, Err(ApiError::NotFound(_))), "{r:?}");
    let r = op_backend_clear_offload_override(&state, &sink, "nope".to_string()).await;
    assert!(matches!(r, Err(ApiError::NotFound(_))), "{r:?}");
    // A backend whose model is not offload-capable has no
    // position to override.
    let r = op_backend_set_offload_override(
        &state,
        &sink,
        "planner".to_string(),
        1,
        vec!["6.0".to_string()],
    )
    .await;
    match r {
        Err(ApiError::InvalidInput(msg)) => {
            assert!(msg.contains("does not support expert offload"), "{msg}")
        }
        other => panic!("expected the dense-backend refusal, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn offload_guardrail_payload_names_placed_ccs() {
    // The card's confirmation and the override operations carry
    // the placed-CC set the payload names.
    let mut cfg = test_cfg();
    let b = serve_model(
        &mut cfg,
        "coder",
        "qwen3-coder-30b",
        "models/Qwen3-Coder-Next-UD-Q4_K_M.gguf",
        Some("--n-cpu-moe 46"),
    );
    let mut gpus = test_gpus();
    gpus[0].compute_capability = (6, 0);
    let journal = activation::ActivationJournal::default();
    let eval =
        evaluate_backend_offload(&cfg, &b, &gpus, &journal).expect("coder backend evaluates");
    let info = backend_offload_info(&cfg, &b).expect("coder info");
    let row = offload_row_json(&info, Some(&eval));
    let guardrail = &row["guardrail"];
    assert_eq!(guardrail["status"], "blocked");
    assert_eq!(guardrail["placed_ccs"], json!(["6.0"]));
    assert_eq!(guardrail["has_override"], false);
    assert_eq!(guardrail["overridden"], false);
}

#[test]
fn backend_set_offload_journals_a_backend_scope_entry() {
    // record_backend_change is the producer backend_set_offload
    // journals through: the entry lands in the activation
    // journal with backend scope and the backend as its target.
    let dir = std::env::temp_dir().join(format!(
        "llm-mgr-offload-journal-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let pre = test_cfg();
    let mut post = pre.clone();
    post.backends
        .iter_mut()
        .find(|b| b.id == "coder")
        .unwrap()
        .raw_flags = Some("--n-cpu-moe 24".to_string());
    // record_backend_change (the producer backend_set_offload
    // journals through) is a thin wrapper over
    // activation::record_change with backend scope; exercise
    // that directly — building an AppState for the wrapper is
    // all supervisor and locks, no journal behavior.
    let configured: std::collections::HashSet<String> =
        post.backends.iter().map(|b| b.id.clone()).collect();
    activation::record_change(
        &dir,
        &pre,
        &post,
        "backend_set_offload",
        "Backend `coder` expert offload set to 24 of 48 MoE layers".to_string(),
        "backend",
        vec!["coder".to_string()],
        &std::collections::HashMap::new(),
        &configured,
        &std::collections::HashSet::new(),
    )
    .unwrap();
    let journal = activation::load_journal(&dir);
    let entry = journal
        .entries
        .iter()
        .find(|e| e.kind == "backend_set_offload")
        .expect("journal carries the offload entry");
    assert_eq!(entry.scope, "backend");
    assert_eq!(entry.targets, vec!["coder".to_string()]);
    assert!(entry.summary.contains("24 of 48"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Stateful loopback stub for the offload-probe sequence
/// tests: answers GET /health and POST /v1/chat/completions
/// from per-path status scripts (each request pops the next
/// scripted status; the last one repeats). Returns the base
/// URL and the per-path request counters.
fn offload_probe_stub(
    health_script: Vec<u16>,
    chat_script: Vec<u16>,
) -> (
    String,
    std::sync::Arc<std::sync::Mutex<usize>>,
    std::sync::Arc<std::sync::Mutex<usize>>,
) {
    use std::io::{Read, Write};
    fn next_status(script: &std::sync::Mutex<Vec<u16>>) -> u16 {
        let mut script = script.lock().unwrap();
        if script.len() > 1 {
            script.remove(0)
        } else {
            script[0]
        }
    }
    fn stub_body(path: &str, status: u16) -> String {
        match (path, status) {
            ("/health", 200) => "{\"status\":\"ok\"}".to_string(),
            ("/health", _) => "{\"status\":\"loading\"}".to_string(),
            (_, 200) => "{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"ok\"}}],\"usage\":{\"prompt_tokens\":4000,\"completion_tokens\":16}}".to_string(),
            (_, 503) => "{\"error\":{\"message\":\"Loading model\",\"type\":\"unavailable_error\",\"code\":503}}".to_string(),
            (_, s) => format!(
                "{{\"error\":{{\"message\":\"stub failure\",\"type\":\"server_error\",\"code\":{s}}}}}"
            ),
        }
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let health_script = std::sync::Arc::new(std::sync::Mutex::new(health_script));
    let chat_script = std::sync::Arc::new(std::sync::Mutex::new(chat_script));
    let health_count = std::sync::Arc::new(std::sync::Mutex::new(0usize));
    let chat_count = std::sync::Arc::new(std::sync::Mutex::new(0usize));
    {
        let health_script = std::sync::Arc::clone(&health_script);
        let chat_script = std::sync::Arc::clone(&chat_script);
        let health_count = std::sync::Arc::clone(&health_count);
        let chat_count = std::sync::Arc::clone(&chat_count);
        std::thread::spawn(move || {
            for _ in 0..500 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                // Read the request head, then the body per
                // Content-Length, before responding.
                let mut buf: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 8192];
                let mut content_length = 0usize;
                let mut head_end: Option<usize> = None;
                loop {
                    if head_end.is_none() {
                        head_end = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
                        if let Some(he) = head_end {
                            let head = String::from_utf8_lossy(&buf[..he]).into_owned();
                            for line in head.lines() {
                                let lower = line.to_ascii_lowercase();
                                if let Some(v) = lower.strip_prefix("content-length:") {
                                    content_length = v.trim().parse().unwrap_or(0);
                                }
                            }
                        }
                    }
                    if let Some(he) = head_end {
                        if buf.len() >= he + content_length {
                            break;
                        }
                    }
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        Err(_) => break,
                    }
                }
                let req = String::from_utf8_lossy(&buf).into_owned();
                let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
                let status = if path == "/health" {
                    *health_count.lock().unwrap() += 1;
                    next_status(&health_script)
                } else {
                    *chat_count.lock().unwrap() += 1;
                    next_status(&chat_script)
                };
                let body = stub_body(&path, status);
                let reason = match status {
                    200 => "OK",
                    500 => "Internal Server Error",
                    503 => "Service Unavailable",
                    _ => "Status",
                };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
    }
    (format!("http://127.0.0.1:{port}"), health_count, chat_count)
}

fn offload_probe_test_timing() -> OffloadProbeTiming {
    OffloadProbeTiming {
        ready_budget: Duration::from_secs(10),
        ready_poll: Duration::from_millis(10),
        retry_window: Duration::from_secs(10),
        retry_poll: Duration::from_millis(10),
    }
}

#[tokio::test]
async fn offload_probe_waits_out_loading_503s() {
    // The tracker #46 field shape: /health reports 503 while
    // the model loads, then 200. The probe must not send its
    // chat request until readiness, and must then pass — the
    // old status-blind "ping" gate fired the probe request
    // into the loading 503 and reported a failure.
    let (base, health_count, chat_count) = offload_probe_stub(vec![503, 503, 200], vec![200]);
    let mut alive = || {
        Box::pin(async { true })
            as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
    };
    let outcome = offload_probe_http_sequence(&base, offload_probe_test_timing(), &mut alive).await;
    assert_eq!(outcome, OffloadProbeHttpOutcome::Passed);
    assert!(*health_count.lock().unwrap() >= 3);
    assert_eq!(*chat_count.lock().unwrap(), 1);
}

#[tokio::test]
async fn offload_probe_reports_not_ready_without_sending_the_probe() {
    // The backend never becomes ready: the outcome is the
    // distinct NotReady, and the probe request is never sent —
    // a not-ready backend must not read as a probe failure.
    let (base, _health_count, chat_count) = offload_probe_stub(vec![503], vec![200]);
    let timing = OffloadProbeTiming {
        ready_budget: Duration::from_millis(300),
        ..offload_probe_test_timing()
    };
    let mut alive = || {
        Box::pin(async { true })
            as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
    };
    let outcome = offload_probe_http_sequence(&base, timing, &mut alive).await;
    assert_eq!(outcome, OffloadProbeHttpOutcome::NotReady);
    assert_eq!(*chat_count.lock().unwrap(), 0);
}

#[tokio::test]
async fn offload_probe_retries_transient_503_after_readiness() {
    // Ready, but the first probe attempts still draw 503s
    // (startup work racing the probe): retried within the
    // window, then the probe passes.
    let (base, _health_count, chat_count) = offload_probe_stub(vec![200], vec![503, 503, 200]);
    let mut alive = || {
        Box::pin(async { true })
            as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
    };
    let outcome = offload_probe_http_sequence(&base, offload_probe_test_timing(), &mut alive).await;
    assert_eq!(outcome, OffloadProbeHttpOutcome::Passed);
    assert_eq!(*chat_count.lock().unwrap(), 3);
}

#[tokio::test]
async fn offload_probe_fails_on_error_status_after_readiness() {
    // A non-transient error status after readiness is a real
    // probe failure, reported immediately (no retry).
    let (base, _health_count, chat_count) = offload_probe_stub(vec![200], vec![500]);
    let mut alive = || {
        Box::pin(async { true })
            as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
    };
    let outcome = offload_probe_http_sequence(&base, offload_probe_test_timing(), &mut alive).await;
    assert_eq!(
        outcome,
        OffloadProbeHttpOutcome::FailedAfterReady("HTTP 500".to_string())
    );
    assert_eq!(*chat_count.lock().unwrap(), 1);
}

#[tokio::test]
async fn offload_probe_persistent_503_after_readiness_is_terminal() {
    // A 503 that outlasts the retry window is reported as a
    // failure after readiness — transient handling must not
    // mask a backend that stays unavailable.
    let (base, _health_count, chat_count) = offload_probe_stub(vec![200], vec![503]);
    let timing = OffloadProbeTiming {
        retry_window: Duration::from_millis(300),
        ..offload_probe_test_timing()
    };
    let mut alive = || {
        Box::pin(async { true })
            as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
    };
    let outcome = offload_probe_http_sequence(&base, timing, &mut alive).await;
    assert_eq!(
        outcome,
        OffloadProbeHttpOutcome::FailedAfterReady("HTTP 503".to_string())
    );
    assert!(*chat_count.lock().unwrap() >= 2);
}

#[test]
fn offload_block_reason_partial_on_p100_is_blocked() {
    // Tracker #37: the field failure — Coder-Next at the
    // position `--n-cpu-moe 46` spells, on a Pascal (6,0) GPU.
    let restrictions = catalog_restrictions("qwen3-coder-30b");
    let position = manager_models::expert_offload(Some("--n-cpu-moe 46"));
    let reason = offload_block_reason(
        &restrictions,
        position.layers(48),
        48,
        &[(6, 0)],
        false,
        false,
    )
    .expect("46 of 48 on compute capability 6.0 is blocked");
    assert!(reason.contains("offload of 46 of 48 layers"), "{reason}");
    assert!(reason.contains("compute capability 6.0"), "{reason}");
    assert!(
        reason.contains("set the offload to 48 of 48 layers"),
        "{reason}"
    );
}

#[test]
fn offload_block_reason_overridden_position_is_allowed() {
    let restrictions = catalog_restrictions("qwen3-coder-30b");
    assert_eq!(
        offload_block_reason(&restrictions, 46, 48, &[(6, 0)], false, true),
        None
    );
}

#[test]
fn offload_block_reason_endpoints_are_allowed() {
    let restrictions = catalog_restrictions("qwen3-coder-30b");
    for layers in [0, 48] {
        assert_eq!(
            offload_block_reason(&restrictions, layers, 48, &[(6, 0)], false, false),
            None,
            "endpoint {layers} is verified on 6.0"
        );
    }
}

#[test]
fn offload_block_reason_other_gpu_is_allowed() {
    // Placement re-evaluation: the same saved position on a
    // newer GPU carries no pinned restriction.
    let restrictions = catalog_restrictions("qwen3-coder-30b");
    assert_eq!(
        offload_block_reason(&restrictions, 46, 48, &[(8, 6)], false, false),
        None
    );
}

#[test]
fn offload_block_reason_blocks_nemotron_partial_on_pascal() {
    let restrictions = catalog_restrictions("nemotron-3.5-lightning-30b");
    let reason = |layers, cc| offload_block_reason(&restrictions, layers, 24, &[cc], false, false);
    assert!(reason(20, (6, 0)).is_some());
    assert_eq!(reason(24, (6, 0)), None);
    assert_eq!(reason(20, (8, 6)), None);
}

#[test]
fn offload_block_reason_unrestricted_model_is_allowed() {
    let restrictions = catalog_restrictions("qwen3-8b");
    assert!(restrictions.is_empty());
    assert_eq!(
        offload_block_reason(&restrictions, 16, 24, &[(6, 0)], false, false),
        None
    );
}

#[test]
fn offload_block_reason_cpu_only_is_allowed() {
    let restrictions = catalog_restrictions("qwen3-coder-30b");
    assert_eq!(
        offload_block_reason(&restrictions, 46, 48, &[], true, false),
        None
    );
}
