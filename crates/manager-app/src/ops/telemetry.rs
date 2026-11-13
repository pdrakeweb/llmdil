//! GPU telemetry, telemetry settings, and the gateway / backend metrics
//! scrapers.

use crate::*;

/// Current GPU telemetry snapshot; also emits `telemetry-update`.
///
/// The command payload is `{samples, processes, at_ms, has_polled,
/// source_error}`. `at_ms` is when the store last polled successfully
/// (epoch ms; the newest sample's timestamp as a fallback, `null` when
/// no poll has completed yet) — it describes the data, not the moment
/// this command ran. `has_polled` says whether any poll has succeeded.
/// `processes` is the latest snapshot's per-process VRAM attribution
/// (each row carries its GPU index; `backend_id` is filled in here by
/// matching the pid against the supervisor's live backend pids, `null`
/// for processes the app does not supervise). `source_error` is the last
/// poll error's message while polls are failing and `null` when the last
/// poll succeeded — a failing source must not look like a GPU-less
/// machine returning an empty snapshot.
pub async fn op_get_gpu_telemetry(state: &Arc<AppState>, sink: &Arc<dyn EventSink>) -> ApiResult {
    // Adoption first: the pid -> backend map below is how telemetry
    // processes get attributed, and an unadopted orphan attributes
    // to nothing.
    let adopted = adopt_orphan_backends(state).await;
    emit_adopted(sink.as_ref(), &adopted);
    let store = state.telemetry().await;
    let (samples, raw_processes, source_error, last_poll_at) = {
        let guard = lock_telemetry(&store);
        (
            guard.snapshot(),
            guard.last_processes().to_vec(),
            guard.last_poll_error().map(|e| e.message.clone()),
            guard.last_successful_poll_at(),
        )
    };
    let payload = TelemetryUpdatePayload {
        samples: samples.clone(),
        at_ms: now_ms(),
    };
    sink.emit(
        events::TELEMETRY_UPDATE,
        serde_json::to_value(&payload).unwrap_or(Value::Null),
    );
    // pid -> backend id from the supervisor's live status (the same data
    // list_backends reads; reap first so a just-exited backend's pid is
    // not attributed to a recycled process).
    let pid_to_backend: HashMap<u32, String> = {
        let mut sup = state.supervisor().await.lock().await;
        sup.reap_exited();
        sup.status()
            .into_iter()
            .filter_map(|s| s.pid.map(|pid| (pid, s.id)))
            .collect()
    };
    let processes: Vec<Value> = raw_processes
        .iter()
        .map(|p| {
            let mut v = serde_json::to_value(p)?;
            v["backend_id"] = json!(pid_to_backend.get(&p.pid));
            Ok(v)
        })
        .collect::<Result<_, ApiError>>()?;
    // The command return enriches each sample with the store's peak
    // temperature mark (the event payload keeps the crate shape) and
    // the store's per-GPU history as sparkline arrays — the history
    // rings exist for exactly this (dashboard Trends), and without
    // this wiring the live payload never carried a series.
    let enriched: Vec<Value> = {
        let guard = lock_telemetry(&store);
        samples
            .iter()
            .map(|s| {
                let mut v = serde_json::to_value(s)?;
                let peak = guard.peaks(s.index).and_then(|p| p.temp_c).map(|m| m.value);
                v["temp_peak_c"] = json!(peak);
                let history = guard.history(s.index);
                if history.len() >= 2 {
                    v["util_spark"] = json!(history
                        .iter()
                        .map(|h| h.utilization_pct)
                        .collect::<Vec<_>>());
                    v["temp_spark"] = json!(history.iter().map(|h| h.temp_c).collect::<Vec<_>>());
                    let power: Vec<f32> = history.iter().filter_map(|h| h.power_w).collect();
                    if power.len() >= 2 {
                        v["power_spark"] = json!(power);
                    }
                }
                Ok(v)
            })
            .collect::<Result<_, ApiError>>()?
    };
    // The command's at_ms stamps the DATA: the store's last successful
    // poll time (fallback: the newest sample's own timestamp; null
    // until a poll has completed). The emitted event keeps its
    // emit-time stamp.
    let at_ms: Option<i64> = last_poll_at
        .map(|t| t.timestamp_millis())
        .or_else(|| samples.iter().map(|s| s.ts.timestamp_millis()).max());
    Ok(json!({
        "samples": enriched,
        "processes": processes,
        "at_ms": at_ms,
        "has_polled": last_poll_at.is_some(),
        "source_error": source_error,
    }))
}

/// Fetch the gateway's Prometheus exposition (`GET /metrics`, the
/// master key as Bearer), following redirects. Redirect following
/// is load-bearing, not a nicety: LiteLLM's prometheus route is
/// registered with a trailing slash on current versions, so
/// `GET /metrics` answers 307 to `/metrics/` — a fetcher that does
/// not follow the redirect never receives a body. That defect left
/// the Dashboard's gateway statistics permanently empty in the
/// field (every poll 307'd and was silently skipped) while the
/// banner blamed a gateway restart that could not help.
///
/// The body arrives with chunked transfer encoding: LiteLLM's
/// metrics app streams the exposition with no content length, so
/// chunked is the normal case, and [`metrics_http_get`] de-chunks
/// it strictly (a truncated or malformed stream fails the cycle —
/// partial counters would corrupt the window deltas). An earlier
/// version of this fetcher skipped chunked bodies outright on the
/// theory that they would corrupt the parse; since the gateway
/// answers ONLY chunked, that discarded every scrape and left the
/// statistics permanently empty again (#58). Returns the body only
/// on a final HTTP 200; anything else — gateway down, the metrics
/// callback not active (config predates v0.1.18 until the next
/// gateway start), an auth failure, a redirect loop — is None and
/// the poll cycle is skipped.
pub(crate) fn fetch_gateway_metrics(host: &str, port: u16, master_key: &str) -> Option<String> {
    const MAX_REDIRECTS: usize = 3;
    let mut path = "/metrics".to_string();
    for _ in 0..=MAX_REDIRECTS {
        let resp = metrics_http_get(host, port, &path, master_key)?;
        match resp.status {
            200 => return Some(resp.body),
            301 | 302 | 303 | 307 | 308 => {
                path = metrics_redirect_path(&path, resp.location.as_deref()?)?;
            }
            _ => return None,
        }
    }
    None
}

/// One gateway-metrics poll cycle: resolve the probe host and port,
/// read the stored master key (never generate one here — polling
/// must not create credentials), fetch and record a snapshot. Any
/// missing piece skips the cycle silently.
pub async fn poll_gateway_metrics_once(state: &AppState) {
    let (host, port) = {
        let cfg = state.config.read().await;
        (
            manager_wizard::steps::gateway_probe_host(&cfg),
            cfg.gateway.port,
        )
    };
    let Some(key) = RealSystemOps::default()
        .credential_read(manager_wizard::LITELLM_MASTER_KEY_CRED_TARGET)
        .await
    else {
        return;
    };
    if key.is_empty() {
        return;
    }
    let body = tokio::task::spawn_blocking(move || fetch_gateway_metrics(&host, port, &key))
        .await
        .ok()
        .flatten();
    if let Some(body) = body {
        let snap = gateway_stats::parse_metrics(&body, std::time::Instant::now());
        state.gateway_stats.lock().await.record(snap);
        // OpenRouter daily spend ledger (spec §7) from the same scrape.
        openrouter_spend::record_scrape(&state.data_dir, &body);
    }
}

/// One direct-`/slots` poll cycle: for every configured backend,
/// GET its llama-server `/slots` endpoint and record the
/// processing slots' `n_decoded` counters (see
/// [`gateway_stats::DirectSlotsStore`]). Runs on the same 15 s
/// loop as [`poll_gateway_metrics_once`].
///
/// No authentication: app-spawned backends run with no
/// `--api-key`, and the health probe already GETs `/health`
/// unauthenticated. A missing or failed fetch (backend down,
/// port unanswered, `/slots` disabled with
/// `--slots-endpoint-disable`, non-200, unparseable body)
/// records no snapshot for that backend: its direct rate goes
/// stale and the tile falls back to the gateway rate — an
/// unavailable signal is never rendered as 0. Any failure
/// skips that backend's cycle silently, as with the gateway
/// poller.
pub async fn poll_direct_slots_once(state: &AppState) {
    let backends: Vec<(String, u16)> = {
        let cfg = state.config.read().await;
        cfg.backends
            .iter()
            .map(|b| (b.id.clone(), b.port))
            .collect()
    };
    let bodies = tokio::task::spawn_blocking(move || {
        backends
            .iter()
            .map(|(id, port)| (id.clone(), fetch_backend_slots(*port)))
            .collect::<Vec<(String, Option<String>)>>()
    })
    .await
    .unwrap_or_default();
    let now = std::time::Instant::now();
    let mut store = state.direct_slots.lock().await;
    for (id, body) in bodies {
        if let Some(body) = body {
            if let Some(snap) = gateway_stats::parse_slots(&body, now) {
                store.record(&id, snap);
            }
        }
    }
}

/// Fetch one backend's `/slots` body over plain HTTP on
/// localhost, no authorization header. Status must be 200 and
/// the body must decode cleanly (same strict response decoding
/// as the gateway metrics fetch — a truncated body is None,
/// never a partial slot list, which would read as a counter
/// drop and overcount). Any transport failure is None.
pub(crate) fn fetch_backend_slots(port: u16) -> Option<String> {
    use std::io::{Read, Write};
    let addr = std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    let mut stream =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(800)).ok()?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
    stream
        .write_all(b"GET /slots HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut raw: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > 8 * 1024 * 1024 {
                    return None;
                }
            }
            Err(_) => break,
        }
    }
    let resp = decode_metrics_response(&raw)?;
    if resp.status != 200 {
        return None;
    }
    Some(resp.body)
}

/// Set the telemetry poll interval. Persisted immediately via
/// `manager_config::save`; the running poller's cadence is fixed when
/// the poller starts (PollHandle exposes no re-cadence), so the new
/// interval takes effect on the next app start.
pub async fn op_telemetry_set_interval(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    ms: u64,
) -> ApiResult {
    // Persisted immediately. The running poller's cadence is fixed when
    // the poller starts (PollHandle exposes no re-cadence), so the new
    // interval takes effect on the next app start; the GPUs view labels
    // the control accordingly.
    let mut cfg = state.config.write().await;
    cfg.telemetry.poll_interval_ms = ms;
    manager_config::save(&cfg, &state.config_path)?;
    Ok(json!({ "ok": true, "poll_interval_ms": ms }))
}

/// Reset peak trackers (spec §5: peak marks are resettable).
pub async fn op_telemetry_reset_peaks(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    lock_telemetry(&state.telemetry().await).reset_peaks();
    Ok(json!({ "ok": true }))
}

/// Write the current snapshot to the data dir.
pub async fn op_telemetry_save_snapshot(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let samples: Vec<GpuSample> = lock_telemetry(&state.telemetry().await).snapshot();
    let path = state
        .data_dir
        .join(format!("telemetry-snapshot-{}.json", now_ms()));
    std::fs::write(&path, serde_json::to_string_pretty(&samples)?).map_err(ApiError::from)?;
    Ok(json!({ "path": path }))
}

pub(crate) struct MetricsHttpResponse {
    pub(crate) status: u16,
    pub(crate) location: Option<String>,
    /// The message body with any chunked transfer encoding already
    /// decoded (see [`decode_metrics_response`]).
    pub(crate) body: String,
}

/// One `GET` against the gateway over a raw socket. The metrics
/// fetcher is hand-rolled to stay dependency-free, so it must handle
/// response parsing — and redirects (see `fetch_gateway_metrics`) —
/// itself. None on any transport or parse failure.
pub(crate) fn metrics_http_get(
    host: &str,
    port: u16,
    path: &str,
    master_key: &str,
) -> Option<MetricsHttpResponse> {
    use std::io::{Read, Write};
    let ip: std::net::IpAddr = host.parse().ok()?;
    let addr = std::net::SocketAddr::new(ip, port);
    let mut stream =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(800)).ok()?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {master_key}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut raw: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > 8 * 1024 * 1024 {
                    return None;
                }
            }
            Err(_) => break,
        }
    }
    decode_metrics_response(&raw)
}

/// Decode one raw HTTP response (status line, headers, body) read
/// off the gateway socket into status, `Location`, and the message
/// body with any chunked transfer encoding removed.
///
/// Strict on purpose: a chunked body must carry well-formed chunk
/// framing through its terminating zero chunk, and a
/// content-length body must be complete. A truncated or malformed
/// response is None, never a partial read — a partial exposition
/// parses as smaller counters, and the window math would read the
/// drop as a gateway restart and overcount (#58).
pub(crate) fn decode_metrics_response(raw: &[u8]) -> Option<MetricsHttpResponse> {
    let head_end = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let status: u16 = head.split_whitespace().nth(1)?.parse().ok()?;
    let header = |name: &str| -> Option<String> {
        head.lines().skip(1).find_map(|line| {
            let (k, v) = line.split_once(':')?;
            if k.trim().eq_ignore_ascii_case(name) {
                Some(v.trim().to_string())
            } else {
                None
            }
        })
    };
    let body_bytes = &raw[head_end + 4..];
    let decoded = if header("transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false)
    {
        dechunk(body_bytes)?
    } else if let Some(len) = header("content-length") {
        let len: usize = len.parse().ok()?;
        if body_bytes.len() < len {
            return None;
        }
        body_bytes[..len].to_vec()
    } else {
        // Close-delimited body (the request says Connection: close).
        body_bytes.to_vec()
    };
    Some(MetricsHttpResponse {
        status,
        location: header("location"),
        body: String::from_utf8(decoded).ok()?,
    })
}

/// Remove HTTP chunked transfer framing: repeated
/// `<hex size>[;extensions]\r\n<size bytes>\r\n`, ended by a zero
/// chunk (trailers may follow and are ignored). None on any
/// malformation — bad hex, a short chunk, a missing CRLF, or no
/// terminating zero chunk (i.e. the stream was cut short).
pub(crate) fn dechunk(mut rest: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = rest.windows(2).position(|w| w == b"\r\n")?;
        let line = std::str::from_utf8(&rest[..line_end]).ok()?;
        let size = usize::from_str_radix(line.split(';').next()?.trim(), 16).ok()?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        if rest.len() < size + 2 || &rest[size..size + 2] != b"\r\n" {
            return None;
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

/// Resolve a redirect `Location` against the path just requested,
/// returning the next path (with query) to GET on the same gateway
/// origin. Handles absolute URLs (only the path is kept — the
/// fetcher never leaves the origin it dialed), absolute paths, and
/// relative references.
pub(crate) fn metrics_redirect_path(current_path: &str, location: &str) -> Option<String> {
    let location = location.trim();
    if location.is_empty() {
        return None;
    }
    if let Some(rest) = location
        .strip_prefix("http://")
        .or_else(|| location.strip_prefix("https://"))
    {
        return Some(match rest.find('/') {
            Some(i) => rest[i..].to_string(),
            None => "/".to_string(),
        });
    }
    if location.starts_with('/') {
        return Some(location.to_string());
    }
    // Relative reference: resolve against the current path's
    // directory.
    let base = current_path.split('?').next().unwrap_or(current_path);
    let dir = base.rfind('/').map(|i| &base[..=i]).unwrap_or("/");
    Some(format!("{dir}{location}"))
}

#[cfg(test)]
mod tests;
