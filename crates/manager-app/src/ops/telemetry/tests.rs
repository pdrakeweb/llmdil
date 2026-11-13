use super::*;

#[test]
fn metrics_redirect_resolution() {
    // The field case: FastAPI's trailing-slash redirect.
    assert_eq!(
        metrics_redirect_path("/metrics", "/metrics/").as_deref(),
        Some("/metrics/")
    );
    // Absolute URL: keep only path+query, stay on the dialed origin.
    assert_eq!(
        metrics_redirect_path("/metrics", "http://127.0.0.1:4000/metrics/?x=1").as_deref(),
        Some("/metrics/?x=1")
    );
    assert_eq!(
        metrics_redirect_path("/metrics", "https://example.test").as_deref(),
        Some("/")
    );
    // Relative reference resolves against the current directory.
    assert_eq!(metrics_redirect_path("/a/b", "c").as_deref(), Some("/a/c"));
    assert_eq!(metrics_redirect_path("/metrics", "  "), None);
}

/// A realistic LiteLLM 1.105 exposition excerpt: the series
/// names carry the prometheus_client `_total` suffix, the
/// deployment counters carry the `api_base` label the generated
/// config writes (`http://127.0.0.1:<port>/v1`), and the token
/// counters carry `model` = `openai/<backend-id>` (label sets
/// per litellm/types/integrations/prometheus.py in the 1.105.1
/// wheel).
fn exposition(success: u64, failure: u64, input: u64, output: u64) -> String {
    format!(
        "# HELP litellm_deployment_success_responses_total Total successful responses\n\
         # TYPE litellm_deployment_success_responses_total counter\n\
         litellm_deployment_success_responses_total{{api_base=\"http://127.0.0.1:8081/v1\",api_provider=\"openai\",litellm_model_name=\"openai/planner\",model_group=\"planner\",requested_model=\"planner\"}} {success}.0\n\
         litellm_deployment_failure_responses_total{{api_base=\"http://127.0.0.1:8081/v1\",api_provider=\"openai\",litellm_model_name=\"openai/planner\",model_group=\"planner\",requested_model=\"planner\"}} {failure}.0\n\
         litellm_input_tokens_metric_total{{model=\"openai/planner\",requested_model=\"planner\"}} {input}.0\n\
         litellm_output_tokens_metric_total{{model=\"openai/planner\",requested_model=\"planner\"}} {output}.0\n"
    )
}

/// Frame a body the way the gateway's streaming metrics app
/// does: hex-sized chunks (small, uneven sizes here so framing
/// boundaries land mid-line) and a terminating zero chunk.
fn chunk_frame(body: &str) -> Vec<u8> {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut sizes = [7usize, 64, 13, 256, 3].into_iter().cycle();
    let mut rest = bytes;
    while !rest.is_empty() {
        let n = sizes.next().unwrap().min(rest.len());
        out.extend_from_slice(format!("{n:x}\r\n").as_bytes());
        out.extend_from_slice(&rest[..n]);
        out.extend_from_slice(b"\r\n");
        rest = &rest[n..];
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

/// The raw response exactly as it arrives off the wire from
/// LiteLLM 1.105's /metrics/ (captured from the real
/// `make_metrics_asgi_app` under uvicorn): 200, a content type,
/// `Transfer-Encoding: chunked`, no content length.
fn chunked_response(body: &str) -> Vec<u8> {
    let mut raw = Vec::new();
    raw.extend_from_slice(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/plain; version=0.0.4; charset=utf-8\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
    );
    raw.extend_from_slice(&chunk_frame(body));
    raw
}

#[test]
fn metrics_response_decodes_chunked_exposition() {
    // #58: every real scrape arrives chunked. The decoder must
    // hand the poller the plain exposition, byte for byte.
    let body = exposition(41, 3, 12000, 3400);
    let resp = decode_metrics_response(&chunked_response(&body)).expect("chunked 200 decodes");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, body);
}

#[test]
fn metrics_response_rejects_truncated_or_malformed_chunked() {
    let body = exposition(41, 3, 12000, 3400);
    let raw = chunked_response(&body);
    // Cut before the terminating zero chunk: the stream was
    // interrupted (read timeout mid-body) — a partial
    // exposition must fail the cycle, not parse as smaller
    // counters.
    assert!(decode_metrics_response(&raw[..raw.len() - 10]).is_none());
    // A non-hex chunk size is malformation, not a body.
    let mut bad = Vec::new();
    bad.extend_from_slice(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
    bad.extend_from_slice(b"zz\r\nhello\r\n0\r\n\r\n");
    assert!(decode_metrics_response(&bad).is_none());
    // A chunk claiming more bytes than the stream carries.
    let mut short = Vec::new();
    short.extend_from_slice(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
    short.extend_from_slice(b"ff\r\ntiny\r\n0\r\n\r\n");
    assert!(decode_metrics_response(&short).is_none());
}

#[test]
fn metrics_response_content_length_must_be_complete() {
    let body = exposition(1, 0, 10, 5);
    let raw = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let resp = decode_metrics_response(raw.as_bytes()).expect("complete body decodes");
    assert_eq!(resp.body, body);
    // One byte short of the declared length: truncated, skip.
    assert!(decode_metrics_response(&raw.as_bytes()[..raw.len() - 1]).is_none());
}

#[test]
fn metrics_response_redirect_keeps_location() {
    let raw =
        b"HTTP/1.1 307 Temporary Redirect\r\nlocation: /metrics/\r\ncontent-length: 0\r\n\r\n";
    let resp = decode_metrics_response(raw).expect("redirect decodes");
    assert_eq!(resp.status, 307);
    assert_eq!(resp.location.as_deref(), Some("/metrics/"));
}

#[test]
fn chunked_scrapes_populate_the_window() {
    // The whole #58 chain at unit level: two chunked scrapes,
    // decoded, parsed, and recorded, produce the trailing
    // window the dashboard reads — requests_1h is defined.
    use crate::gateway_stats::{parse_metrics, GatewayStatsStore};
    let t0 = std::time::Instant::now();
    let mut store = GatewayStatsStore::default();
    for (at, body) in [
        (t0, exposition(41, 3, 12000, 3400)),
        (
            t0 + std::time::Duration::from_secs(60),
            exposition(55, 5, 18000, 5100),
        ),
    ] {
        let resp = decode_metrics_response(&chunked_response(&body)).expect("scrape decodes");
        store.record(parse_metrics(&resp.body, at));
    }
    let windows = store
        .window_stats(&[("planner".to_string(), 8081)])
        .expect("two snapshots make a window");
    let w = windows["planner"];
    assert_eq!(w.requests, 16); // +14 successes, +2 failures
    assert_eq!(w.errors, 2);
    assert_eq!(w.tokens, 7700); // (18000+5100) - (12000+3400)
}

/// Loopback HTTP server for the metrics-fetcher tests: answers
/// each accepted connection with the canned response mapped to
/// the request's path (404 when unmapped), then closes.
/// Returns the bound port.
fn metrics_test_server(
    responses: std::collections::HashMap<String, String>,
    connections: usize,
) -> u16 {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for _ in 0..connections {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
            let resp = responses.get(&path).cloned().unwrap_or_else(|| {
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string()
            });
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    port
}

#[test]
fn metrics_fetch_follows_trailing_slash_redirect() {
    let body = "litellm_deployment_success_responses_total 3.0\n";
    let responses = std::collections::HashMap::from([
        (
            "/metrics".to_string(),
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: /metrics/\r\nContent-Length: 0\r\n\r\n"
                .to_string(),
        ),
        (
            "/metrics/".to_string(),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            ),
        ),
    ]);
    let port = metrics_test_server(responses, 2);
    let got = fetch_gateway_metrics("127.0.0.1", port, "test-key").expect("body after redirect");
    assert_eq!(got, body);
}

#[test]
fn metrics_fetch_direct_200_and_error_status() {
    let responses = std::collections::HashMap::from([(
        "/metrics".to_string(),
        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nbody".to_string(),
    )]);
    let port = metrics_test_server(responses, 1);
    assert_eq!(
        fetch_gateway_metrics("127.0.0.1", port, "k").as_deref(),
        Some("body")
    );
    // No path mapped -> the server's 404 -> None, no panic.
    let port = metrics_test_server(std::collections::HashMap::new(), 1);
    assert_eq!(fetch_gateway_metrics("127.0.0.1", port, "k"), None);
}
