//! Streaming, structured-output chat completion for the Optimize
//! consultation (docs/optimize-design.md §3 and §7 Phase 3).
//!
//! [`chat_completion`](crate::chat_completion) is one
//! non-streaming request: the page could show elapsed time only.
//! [`consult_completion`] asks with `stream: true` and a
//! caller-supplied `response_format` (the plan's JSON schema),
//! accumulates the SSE deltas, and reports progress on every
//! chunk so the app can surface tokens-so-far and partial-answer
//! size while a multi-minute generation runs.
//!
//! Fallbacks, because llama.cpp builds differ (the design's
//! reason v1 shipped without either):
//!
//! - A request the server **rejects** (an immediate 4xx, or a
//!   500/501 before any generation) is retried with the next
//!   weaker shape: streaming without `response_format`, then
//!   non-streaming with it, then the v1 request (neither). A
//!   transport failure (refused connection, timeout) is not a
//!   rejection and fails at once — retrying cannot fix it.
//! - A server that accepts `stream: true` but answers with a plain
//!   JSON completion is read as one (no progress beyond the end).
//! - A server that accepts `response_format` but ignores it simply
//!   produces text; the caller's salvage parser handles that.
//!
//! The finished answer goes through the same interpretation as the
//! non-streaming call ([`interpret_chat_completion`]), so an empty
//! answer, a reasoning-only answer, and a length-truncated answer
//! fail with the same diagnosable errors either way.
//!
//! Cancellation is by future drop, exactly like `chat_completion`:
//! the Optimize consult slot `select!`s this future against its
//! abort handle, and dropping it closes the connection, which
//! llama-server treats as a cancelled generation.

use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

use crate::{interpret_chat_completion, ChatCompletionResult, ChatMessage, SupervisorError};

/// Live progress of one streaming completion, reported after every
/// chunk that carried text.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct StreamProgress {
    /// Content and reasoning delta chunks received so far.
    /// llama-server emits one chunk per sampled token, so this is
    /// the tokens-so-far figure (labelled as chunks where the
    /// distinction matters).
    pub chunks: u64,
    /// Answer text accumulated so far, in bytes.
    pub content_bytes: usize,
    /// Reasoning text accumulated so far, in bytes (a thinking
    /// model that ignores `enable_thinking: false`).
    pub reasoning_bytes: usize,
    /// Time since the request was sent.
    pub elapsed_ms: u64,
    /// Time to the first chunk carrying text, once seen.
    pub first_token_ms: Option<u64>,
}

/// How a consultation completion was finally obtained.
#[derive(Debug, Clone)]
pub struct ConsultCompletion {
    pub result: ChatCompletionResult,
    /// The answer arrived as an event stream (progress was live).
    pub streamed: bool,
    /// The accepted request carried `response_format`. Whether the
    /// server honoured it is not observable from the response; the
    /// salvage parser reads the answer either way.
    pub structured: bool,
    /// One line per rejected request shape, in order — the
    /// fallback trail for the Optimize log.
    pub fallbacks: Vec<String>,
}

/// One request shape on the fallback ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    stream: bool,
    structured: bool,
}

/// The ladder for a request: strongest shape first, the v1 shape
/// (no stream, no schema) last.
fn ladder(want_structured: bool) -> Vec<Shape> {
    let mut out = vec![Shape {
        stream: true,
        structured: want_structured,
    }];
    if want_structured {
        out.push(Shape {
            stream: true,
            structured: false,
        });
        out.push(Shape {
            stream: false,
            structured: true,
        });
    }
    out.push(Shape {
        stream: false,
        structured: false,
    });
    out
}

/// Whether an HTTP status means "this server does not accept this
/// request shape" (try a weaker one) rather than "this server
/// cannot serve" (fail). llama-server answers an unsupported or
/// unconvertible `response_format` with 400 (some builds 500), and
/// an unknown parameter on stricter OpenAI-compatible servers with
/// 400/422. Auth, not-found, timeout, and rate-limit statuses are
/// not shape problems.
fn is_shape_rejection(status: u16) -> bool {
    match status {
        401 | 403 | 404 | 408 | 429 => false,
        400..=499 => true,
        500 | 501 => true,
        _ => false,
    }
}

enum AttemptError {
    Rejected { status: u16, excerpt: String },
    Fatal(SupervisorError),
}

fn request_body(
    model: &str,
    messages: &[ChatMessage],
    temperature: f32,
    max_tokens: u32,
    shape: Shape,
    response_format: Option<&Value>,
) -> Value {
    let mut body = json!({
        "model": model,
        "messages": messages,
        "temperature": temperature,
        "max_tokens": max_tokens,
        "stream": shape.stream,
        // Same rationale as chat_completion: the consultation
        // wants an answer, not a reasoning trace.
        "chat_template_kwargs": { "enable_thinking": false },
    });
    if shape.structured {
        if let Some(rf) = response_format {
            body["response_format"] = rf.clone();
        }
    }
    body
}

/// Accumulator for an SSE answer.
#[derive(Default)]
struct StreamAcc {
    content: String,
    reasoning: String,
    finish_reason: Option<String>,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    error: Option<String>,
    progress: StreamProgress,
}

impl StreamAcc {
    /// Absorb one SSE line. Returns true when the line carried
    /// text (progress changed).
    fn absorb_line(&mut self, line: &str, start: Instant) -> bool {
        let Some(data) = line.trim().strip_prefix("data:") else {
            return false;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return false;
        }
        let Ok(json) = serde_json::from_str::<Value>(data) else {
            return false;
        };
        // A mid-stream error event (llama-server sends
        // `data: {"error": {...}}` when generation fails after the
        // 200 header went out).
        if let Some(err) = json.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| err.to_string());
            self.error = Some(msg);
            return false;
        }
        if let Some(fr) = json
            .pointer("/choices/0/finish_reason")
            .and_then(|v| v.as_str())
        {
            self.finish_reason = Some(fr.to_string());
        }
        if let Some(n) = json
            .pointer("/usage/prompt_tokens")
            .and_then(|v| v.as_u64())
        {
            self.prompt_tokens = Some(n);
        }
        if let Some(n) = json
            .pointer("/usage/completion_tokens")
            .and_then(|v| v.as_u64())
        {
            self.completion_tokens = Some(n);
        }
        // llama.cpp's own timings (final chunk) carry the predicted
        // token count even when no usage object is sent.
        if self.completion_tokens.is_none() {
            if let Some(n) = json
                .pointer("/timings/predicted_n")
                .and_then(|v| v.as_u64())
            {
                self.completion_tokens = Some(n);
            }
        }
        let mut grew = false;
        for (field, is_content) in [("content", true), ("reasoning_content", false)] {
            if let Some(delta) = json
                .pointer(&format!("/choices/0/delta/{field}"))
                .and_then(|v| v.as_str())
            {
                if !delta.is_empty() {
                    if is_content {
                        self.content.push_str(delta);
                    } else {
                        self.reasoning.push_str(delta);
                    }
                    grew = true;
                }
            }
        }
        if grew {
            let elapsed = start.elapsed().as_millis() as u64;
            self.progress.chunks += 1;
            self.progress.content_bytes = self.content.len();
            self.progress.reasoning_bytes = self.reasoning.len();
            self.progress.elapsed_ms = elapsed;
            if self.progress.first_token_ms.is_none() {
                self.progress.first_token_ms = Some(elapsed);
            }
        }
        grew
    }

    /// The accumulated answer as a non-streaming response body, so
    /// it goes through the one interpretation both paths share.
    fn as_completion_body(&self) -> String {
        let mut usage = serde_json::Map::new();
        if let Some(n) = self.prompt_tokens {
            usage.insert("prompt_tokens".into(), json!(n));
        }
        if let Some(n) = self.completion_tokens {
            usage.insert("completion_tokens".into(), json!(n));
        }
        json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": self.content,
                    "reasoning_content": self.reasoning,
                },
                "finish_reason": self.finish_reason,
            }],
            "usage": Value::Object(usage),
        })
        .to_string()
    }
}

#[allow(clippy::too_many_arguments)]
async fn attempt(
    client: &reqwest::Client,
    url: &str,
    model: &str,
    messages: &[ChatMessage],
    temperature: f32,
    max_tokens: u32,
    shape: Shape,
    response_format: Option<&Value>,
    on_progress: &(dyn Fn(&StreamProgress) + Send + Sync),
) -> Result<(ChatCompletionResult, bool), AttemptError> {
    let body = request_body(
        model,
        messages,
        temperature,
        max_tokens,
        shape,
        response_format,
    );
    let body_str = serde_json::to_string(&body)
        .map_err(|e| AttemptError::Fatal(SupervisorError::Http(e.to_string())))?;
    let start = Instant::now();
    let mut resp = client
        .post(url)
        .header("content-type", "application/json")
        .body(body_str)
        .send()
        .await
        .map_err(|e| AttemptError::Fatal(SupervisorError::Http(e.to_string())))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        let text = resp.text().await.unwrap_or_default();
        let excerpt: String = text.chars().take(300).collect();
        return Err(if is_shape_rejection(status) {
            AttemptError::Rejected { status, excerpt }
        } else {
            AttemptError::Fatal(SupervisorError::Http(format!(
                "backend returned HTTP {status}: {excerpt}"
            )))
        });
    }
    let is_event_stream = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);
    if !is_event_stream {
        let text = resp
            .text()
            .await
            .map_err(|e| AttemptError::Fatal(SupervisorError::Http(e.to_string())))?;
        let latency_ms = start.elapsed().as_millis() as u64;
        return interpret_chat_completion(status, &text, latency_ms, max_tokens)
            .map(|r| (r, false))
            .map_err(AttemptError::Fatal);
    }

    let mut acc = StreamAcc::default();
    let mut buffer: Vec<u8> = Vec::new();
    let mut transport_bytes = 0usize;
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                transport_bytes += chunk.len();
                buffer.extend_from_slice(&chunk);
                // Split on newlines at the byte level, so a UTF-8
                // sequence split across network chunks is decoded
                // only once whole.
                while let Some(pos) = buffer.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buffer.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&line);
                    if acc.absorb_line(&line, start) {
                        on_progress(&acc.progress);
                    }
                }
            }
            Ok(None) => break,
            Err(e) => return Err(AttemptError::Fatal(SupervisorError::Http(e.to_string()))),
        }
    }
    if !buffer.is_empty() {
        let line = String::from_utf8_lossy(&buffer).into_owned();
        if acc.absorb_line(&line, start) {
            on_progress(&acc.progress);
        }
    }
    if let Some(err) = acc.error {
        return Err(AttemptError::Fatal(SupervisorError::Http(format!(
            "backend reported an error mid-stream: {err}"
        ))));
    }
    let latency_ms = start.elapsed().as_millis() as u64;
    let mut result =
        interpret_chat_completion(status, &acc.as_completion_body(), latency_ms, max_tokens)
            .map_err(AttemptError::Fatal)?;
    // Transport size is the bytes that crossed the wire, not the
    // synthesized body's.
    result.response_bytes = transport_bytes;
    Ok((result, true))
}

/// A consultation completion: streaming with progress, structured
/// output when `response_format` is given, and the fallback ladder
/// described in the module docs. `POST {base_url}/v1/chat/completions`.
#[allow(clippy::too_many_arguments)]
pub async fn consult_completion(
    base_url: &str,
    model: &str,
    messages: &[ChatMessage],
    temperature: f32,
    max_tokens: u32,
    response_format: Option<&Value>,
    timeout: Duration,
    on_progress: &(dyn Fn(&StreamProgress) + Send + Sync),
) -> Result<ConsultCompletion, SupervisorError> {
    let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| SupervisorError::Http(e.to_string()))?;
    let shapes = ladder(response_format.is_some());
    let mut fallbacks: Vec<String> = Vec::new();
    for (i, shape) in shapes.iter().enumerate() {
        match attempt(
            &client,
            &url,
            model,
            messages,
            temperature,
            max_tokens,
            *shape,
            response_format,
            on_progress,
        )
        .await
        {
            Ok((result, streamed)) => {
                return Ok(ConsultCompletion {
                    result,
                    streamed,
                    structured: shape.structured,
                    fallbacks,
                })
            }
            Err(AttemptError::Fatal(e)) => return Err(e),
            Err(AttemptError::Rejected { status, excerpt }) => {
                let last = i + 1 == shapes.len();
                if last {
                    return Err(SupervisorError::Http(format!(
                        "backend returned HTTP {status}: {excerpt}"
                    )));
                }
                fallbacks.push(format!(
                    "request with stream={} response_format={} rejected (HTTP {status}: {excerpt})",
                    shape.stream, shape.structured
                ));
            }
        }
    }
    unreachable!("the ladder always ends with a terminal attempt")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Read one HTTP request (head + Content-Length body).
    async fn read_request(sock: &mut tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 8192];
        loop {
            let n = sock.read(&mut tmp).await.unwrap();
            assert!(n > 0, "client closed before a full request");
            buf.extend_from_slice(&tmp[..n]);
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                let len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if buf.len() >= end + 4 + len {
                    return String::from_utf8_lossy(&buf).into_owned();
                }
            }
        }
    }

    /// What the fake server does with one request body.
    type Responder = Arc<dyn Fn(&Value) -> FakeReply + Send + Sync>;

    enum FakeReply {
        Status(u16, String),
        Json(String),
        /// SSE events, each written as its own network chunk, with
        /// an optional pause before closing (to exercise
        /// cancellation).
        Sse(Vec<String>, Option<Duration>),
    }

    /// A fake backend answering up to `n` requests; records each
    /// request body.
    async fn fake_server(n: usize, responder: Responder) -> (String, Arc<Mutex<Vec<Value>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        tokio::spawn(async move {
            for _ in 0..n {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let req = read_request(&mut sock).await;
                assert!(req.starts_with("POST /v1/chat/completions "), "{req}");
                let body_start = req.find("\r\n\r\n").unwrap() + 4;
                let body: Value = serde_json::from_str(&req[body_start..]).unwrap();
                seen2.lock().unwrap().push(body.clone());
                match responder(&body) {
                    FakeReply::Status(status, text) => {
                        let resp = format!(
                            "HTTP/1.1 {status} x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                            text.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                    }
                    FakeReply::Json(text) => {
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                            text.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                    }
                    FakeReply::Sse(events, hold) => {
                        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
                        let _ = sock.write_all(head.as_bytes()).await;
                        for e in events {
                            let _ = sock.write_all(format!("data: {e}\n\n").as_bytes()).await;
                            let _ = sock.flush().await;
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                        if let Some(d) = hold {
                            tokio::time::sleep(d).await;
                        }
                        let _ = sock.write_all(b"data: [DONE]\n\n").await;
                    }
                }
            }
        });
        (format!("http://127.0.0.1:{port}"), seen)
    }

    fn delta(text: &str) -> String {
        json!({ "choices": [{ "delta": { "content": text }, "finish_reason": null }] }).to_string()
    }

    fn final_chunk() -> String {
        json!({
            "choices": [{ "delta": {}, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 100, "completion_tokens": 3 },
            "timings": { "predicted_n": 3 },
        })
        .to_string()
    }

    fn msgs() -> Vec<ChatMessage> {
        vec![ChatMessage {
            role: "user".into(),
            content: "plan".into(),
        }]
    }

    fn schema_rf() -> Value {
        json!({ "type": "json_schema", "json_schema": { "name": "p", "schema": { "type": "object" } } })
    }

    #[tokio::test]
    async fn streams_deltas_with_progress_and_schema() {
        let (url, seen) = fake_server(
            1,
            Arc::new(|_| {
                FakeReply::Sse(
                    vec![
                        delta("{\"summary\""),
                        delta(":\"s\",\"items\""),
                        delta(":[]}"),
                        final_chunk(),
                    ],
                    None,
                )
            }),
        )
        .await;
        let progress: Arc<Mutex<Vec<StreamProgress>>> = Arc::default();
        let p2 = progress.clone();
        let cb = move |p: &StreamProgress| p2.lock().unwrap().push(p.clone());
        let rf = schema_rf();
        let out = consult_completion(
            &url,
            "m",
            &msgs(),
            0.2,
            4096,
            Some(&rf),
            Duration::from_secs(10),
            &cb,
        )
        .await
        .unwrap();
        assert_eq!(out.result.content, "{\"summary\":\"s\",\"items\":[]}");
        assert!(out.streamed);
        assert!(out.structured);
        assert!(out.fallbacks.is_empty());
        assert_eq!(out.result.finish_reason.as_deref(), Some("stop"));
        assert_eq!(out.result.completion_tokens, Some(3));
        assert_eq!(out.result.prompt_tokens, Some(100));
        let req = &seen.lock().unwrap()[0];
        assert_eq!(req["stream"], json!(true));
        assert_eq!(req["response_format"], rf);
        assert_eq!(req["chat_template_kwargs"]["enable_thinking"], json!(false));
        let p = progress.lock().unwrap();
        assert_eq!(p.len(), 3, "one progress report per text chunk");
        assert_eq!(
            p.iter().map(|x| x.chunks).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(p
            .windows(2)
            .all(|w| w[0].content_bytes < w[1].content_bytes));
        assert_eq!(p[2].content_bytes, out.result.content.len());
        assert!(p[0].first_token_ms.is_some());
    }

    /// A build that rejects `response_format` is retried streaming
    /// without it; the answer then comes back as prose for the
    /// salvage parser.
    #[tokio::test]
    async fn schema_rejection_falls_back_to_plain_streaming() {
        let (url, seen) = fake_server(
            2,
            Arc::new(|body| {
                if body.get("response_format").is_some() {
                    FakeReply::Status(400, "{\"error\":\"response_format not supported\"}".into())
                } else {
                    FakeReply::Sse(
                        vec![delta("```json\n{\"items\":[]}\n```"), final_chunk()],
                        None,
                    )
                }
            }),
        )
        .await;
        let rf = schema_rf();
        let out = consult_completion(
            &url,
            "m",
            &msgs(),
            0.2,
            4096,
            Some(&rf),
            Duration::from_secs(10),
            &|_| {},
        )
        .await
        .unwrap();
        assert!(out.streamed);
        assert!(!out.structured);
        assert_eq!(out.fallbacks.len(), 1);
        assert!(out.fallbacks[0].contains("HTTP 400"), "{:?}", out.fallbacks);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[1].get("response_format").is_none());
        assert_eq!(seen[1]["stream"], json!(true));
    }

    /// A server that rejects streaming altogether lands on the
    /// non-streaming shapes, schema first.
    #[tokio::test]
    async fn stream_rejection_falls_back_to_non_streaming() {
        let body = json!({
            "choices": [{ "message": { "content": "{\"items\":[]}" }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 4 },
        })
        .to_string();
        let (url, seen) = fake_server(
            3,
            Arc::new(move |req| {
                if req["stream"] == json!(true) {
                    FakeReply::Status(422, "stream unsupported".into())
                } else {
                    FakeReply::Json(body.clone())
                }
            }),
        )
        .await;
        let rf = schema_rf();
        let out = consult_completion(
            &url,
            "m",
            &msgs(),
            0.2,
            4096,
            Some(&rf),
            Duration::from_secs(10),
            &|_| {},
        )
        .await
        .unwrap();
        assert!(!out.streamed);
        assert!(
            out.structured,
            "the non-streaming schema shape was accepted"
        );
        assert_eq!(out.fallbacks.len(), 2);
        assert_eq!(out.result.content, "{\"items\":[]}");
        let seen = seen.lock().unwrap();
        assert_eq!(seen[2]["stream"], json!(false));
        assert_eq!(seen[2]["response_format"], rf);
    }

    /// `stream: true` answered with a plain JSON body is read as a
    /// non-streaming completion, not an error.
    #[tokio::test]
    async fn plain_json_answer_to_a_stream_request_is_accepted() {
        let body = json!({
            "choices": [{ "message": { "content": "{\"items\":[]}" }, "finish_reason": "stop" }],
        })
        .to_string();
        let (url, _) = fake_server(1, Arc::new(move |_| FakeReply::Json(body.clone()))).await;
        let out = consult_completion(
            &url,
            "m",
            &msgs(),
            0.2,
            4096,
            None,
            Duration::from_secs(10),
            &|_| {},
        )
        .await
        .unwrap();
        assert!(!out.streamed);
        assert!(!out.structured);
        assert_eq!(out.result.content, "{\"items\":[]}");
    }

    /// Every shape rejected: the last rejection is the error.
    #[tokio::test]
    async fn exhausted_ladder_reports_the_last_rejection() {
        let (url, seen) = fake_server(4, Arc::new(|_| FakeReply::Status(400, "nope".into()))).await;
        let rf = schema_rf();
        let err = consult_completion(
            &url,
            "m",
            &msgs(),
            0.2,
            4096,
            Some(&rf),
            Duration::from_secs(10),
            &|_| {},
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("HTTP 400"), "{err}");
        assert_eq!(seen.lock().unwrap().len(), 4);
    }

    /// A non-shape status fails at once (no retries).
    #[tokio::test]
    async fn non_shape_status_fails_without_fallback() {
        let (url, seen) = fake_server(
            1,
            Arc::new(|_| FakeReply::Status(503, "loading model".into())),
        )
        .await;
        let rf = schema_rf();
        let err = consult_completion(
            &url,
            "m",
            &msgs(),
            0.2,
            4096,
            Some(&rf),
            Duration::from_secs(10),
            &|_| {},
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("HTTP 503"), "{err}");
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    /// A reasoning-only stream fails with the same diagnosis the
    /// non-streaming interpretation gives.
    #[tokio::test]
    async fn reasoning_only_stream_is_an_error() {
        let reasoning =
            json!({ "choices": [{ "delta": { "reasoning_content": "thinking..." } }] }).to_string();
        let end = json!({ "choices": [{ "delta": {}, "finish_reason": "length" }] }).to_string();
        let (url, _) = fake_server(
            1,
            Arc::new(move |_| FakeReply::Sse(vec![reasoning.clone(), end.clone()], None)),
        )
        .await;
        let progress: Arc<Mutex<Option<StreamProgress>>> = Arc::default();
        let p2 = progress.clone();
        let cb = move |p: &StreamProgress| *p2.lock().unwrap() = Some(p.clone());
        let err = consult_completion(
            &url,
            "m",
            &msgs(),
            0.2,
            16,
            None,
            Duration::from_secs(10),
            &cb,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("reasoning"), "{err}");
        let p = progress.lock().unwrap().clone().unwrap();
        assert_eq!(p.content_bytes, 0);
        assert!(p.reasoning_bytes > 0);
    }

    #[tokio::test]
    async fn mid_stream_error_event_is_an_error() {
        let err_event = json!({ "error": { "message": "kv cache full" } }).to_string();
        let (url, _) = fake_server(
            1,
            Arc::new(move |_| FakeReply::Sse(vec![delta("{"), err_event.clone()], None)),
        )
        .await;
        let err = consult_completion(
            &url,
            "m",
            &msgs(),
            0.2,
            16,
            None,
            Duration::from_secs(10),
            &|_| {},
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("kv cache full"), "{err}");
    }

    /// Cancellation by drop: a consult racing an abort signal stops
    /// mid-stream as soon as the signal fires, with the progress
    /// seen so far intact.
    #[tokio::test]
    async fn dropping_the_future_cancels_mid_stream() {
        let (url, _) = fake_server(
            1,
            Arc::new(|_| FakeReply::Sse(vec![delta("{\"it")], Some(Duration::from_secs(30)))),
        )
        .await;
        let progress: Arc<Mutex<Option<StreamProgress>>> = Arc::default();
        let p2 = progress.clone();
        let cb = move |p: &StreamProgress| *p2.lock().unwrap() = Some(p.clone());
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let p3 = progress.clone();
        tokio::spawn(async move {
            // Fire once the first chunk has been seen.
            for _ in 0..500 {
                if p3.lock().unwrap().is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let _ = tx.send(());
        });
        let started = Instant::now();
        let messages = msgs();
        let fut = consult_completion(
            &url,
            "m",
            &messages,
            0.2,
            16,
            None,
            Duration::from_secs(60),
            &cb,
        );
        let outcome = tokio::select! {
            r = fut => Some(r),
            _ = rx => None,
        };
        assert!(outcome.is_none(), "the abort must win");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(progress.lock().unwrap().as_ref().unwrap().chunks, 1);
    }
}
