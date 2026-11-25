//! The benchmark completion every measuring path uses (Optimize
//! measure / estimate / experiments / tuning, and Bench sweeps).
//!
//! Every request sends `"cache_prompt": false`: llama-server otherwise
//! reuses the previous request's prompt, so the runs after a warm-up
//! processed ~1 prompt token and reported a prompt speed equal to the
//! generation speed. `cache_prompt` is llama-server's own request
//! field; `/v1/chat/completions` passes unknown fields through to it
//! exactly as `/completion` reads it. The server's `prompt_n` /
//! `cache_n` are checked all the same, so a figure taken behind a
//! cache is dropped rather than reported.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// One benchmark run. Rates are llama.cpp's own `timings`, never
/// wall-clock token counting.
#[derive(Debug, Clone, Default)]
pub(crate) struct Sample {
    /// Request start to the first streamed token, answer or
    /// reasoning: a thinking model streams `reasoning_content` first,
    /// and that is when its first token arrived.
    pub ttft_ms: Option<f64>,
    /// `None` when the prompt was not (fully) processed.
    pub prompt_per_second: Option<f64>,
    pub predicted_per_second: Option<f64>,
    /// Prompt tokens the server processed for this request.
    pub prompt_n: Option<u32>,
    pub content: String,
    pub latency_ms: u64,
}

impl Sample {
    pub(crate) fn bench_run(&self) -> manager_optimize::BenchRun {
        manager_optimize::BenchRun {
            prompt_per_second: self.prompt_per_second,
            predicted_per_second: self.predicted_per_second,
            ttft_ms: self.ttft_ms,
            latency_ms: self.latency_ms,
            prompt_n: self.prompt_n,
        }
    }
}

/// What the stream's chunks add up to.
#[derive(Default)]
struct Accumulator {
    timings: Option<Value>,
    content: String,
    ttft_ms: Option<f64>,
}

impl Accumulator {
    fn chunk(&mut self, json: &Value, elapsed_ms: f64) {
        if let Some(t) = json.get("timings").filter(|t| t.is_object()) {
            self.timings = Some(t.clone());
        }
        let Some(delta) = json.pointer("/choices/0/delta") else {
            return;
        };
        let text = |key: &str| delta.get(key).and_then(Value::as_str).unwrap_or("");
        if !text("content").is_empty() || !text("reasoning_content").is_empty() {
            self.ttft_ms.get_or_insert(elapsed_ms);
        }
        self.content.push_str(text("content"));
    }

    fn line(&mut self, line: &str, elapsed_ms: f64) {
        let Some(data) = line.trim().strip_prefix("data:") else {
            return;
        };
        if let Ok(json) = serde_json::from_str::<Value>(data.trim()) {
            self.chunk(&json, elapsed_ms);
        }
    }

    fn sample(self, prompt_chars: usize, latency_ms: u64) -> Sample {
        let t = self.timings.unwrap_or(Value::Null);
        let num = |key: &str| t.get(key).and_then(Value::as_f64);
        let rate = |per_second: &str, ms: &str, n: &str| -> Option<f64> {
            num(per_second)
                .filter(|v| *v > 0.0)
                .or_else(|| Some(num(n)? / (num(ms)? / 1000.0)).filter(|v| *v > 0.0))
                .filter(|v| v.is_finite())
        };
        let prompt_n = num("prompt_n").map(|n| n as u32);
        let whole = prompt_processed(prompt_n, num("cache_n").map(|n| n as u32), prompt_chars);
        Sample {
            ttft_ms: self.ttft_ms.filter(|_| whole),
            prompt_per_second: rate("prompt_per_second", "prompt_ms", "prompt_n").filter(|_| whole),
            predicted_per_second: rate("predicted_per_second", "predicted_ms", "predicted_n"),
            prompt_n,
            content: self.content,
            latency_ms,
        }
    }
}

/// Whether the server processed the prompt instead of reusing a cached
/// one: nothing (or less than was processed) came from the cache, and
/// the processed count is plausible for the text. A token covers well
/// under 8 characters of any real prompt; repeated filler included.
fn prompt_processed(prompt_n: Option<u32>, cache_n: Option<u32>, prompt_chars: usize) -> bool {
    match prompt_n {
        Some(n) => n > cache_n.unwrap_or(0) && (n as usize) * 8 >= prompt_chars,
        // A server that reports no counts cannot be checked.
        None => true,
    }
}

/// One streaming completion of `prompt` at fixed settings, with the
/// prompt cache disabled. `seed: None` sends no seed. Cancellation is
/// by future drop.
pub(crate) async fn complete(
    base_url: &str,
    model: &str,
    prompt: &str,
    temperature: f32,
    max_tokens: u32,
    seed: Option<u32>,
    timeout: Duration,
) -> Result<Sample, String> {
    let mut body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": prompt }],
        "temperature": temperature,
        "max_tokens": max_tokens,
        "stream": true,
        "cache_prompt": false,
    });
    if let Some(seed) = seed {
        body["seed"] = json!(seed);
    }
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let start = Instant::now();
    let elapsed_ms = || start.elapsed().as_secs_f64() * 1000.0;
    let mut resp = client
        .post(format!(
            "{}/v1/chat/completions",
            base_url.trim_end_matches('/')
        ))
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        let text = resp.text().await.unwrap_or_default();
        let excerpt: String = text.chars().take(300).collect();
        return Err(format!("backend returned HTTP {status}: {excerpt}"));
    }
    let streamed = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"));
    let mut acc = Accumulator::default();
    if streamed {
        let mut buffer: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
            buffer.extend_from_slice(&chunk);
            while let Some(pos) = buffer.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buffer.drain(..=pos).collect();
                acc.line(&String::from_utf8_lossy(&line), elapsed_ms());
            }
        }
        acc.line(&String::from_utf8_lossy(&buffer), elapsed_ms());
    } else {
        // A plain JSON completion: no TTFT can be measured.
        let text = resp.text().await.map_err(|e| e.to_string())?;
        let json: Value =
            serde_json::from_str(&text).map_err(|e| format!("unparseable response body: {e}"))?;
        acc.chunk(&json, 0.0);
        acc.ttft_ms = None;
        acc.content = json
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
    }
    Ok(acc.sample(prompt.chars().count(), elapsed_ms() as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Serve `body` once as an event stream; returns the base URL and
    /// the request text the server received.
    async fn fake_server(body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                req.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&req).to_string();
                if n == 0 || text.contains("\"stream\":true") {
                    break;
                }
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&req).to_string()
        });
        (url, task)
    }

    async fn run(body: &'static str) -> (Sample, String) {
        let (url, task) = fake_server(body).await;
        let s = complete(
            &url,
            "m",
            "forty characters of prompt text, roughly.",
            0.0,
            16,
            Some(42),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        (s, task.await.unwrap())
    }

    #[tokio::test]
    async fn disables_the_prompt_cache_and_reads_timings() {
        let (s, req) = run(concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{}}],\"timings\":{\"cache_n\":0,\"prompt_n\":12,\"prompt_ms\":20.0,\"prompt_per_second\":600.0,\"predicted_per_second\":36.5}}\n\n",
            "data: [DONE]\n\n",
        ))
        .await;
        assert!(req.contains("\"cache_prompt\":false"), "{req}");
        assert!(req.contains("\"seed\":42"), "{req}");
        assert_eq!(s.content, "Hi");
        assert_eq!(s.prompt_per_second, Some(600.0));
        assert_eq!(s.predicted_per_second, Some(36.5));
        assert_eq!(s.prompt_n, Some(12));
        assert!(s.ttft_ms.is_some());
    }

    #[tokio::test]
    async fn a_thinking_models_first_reasoning_token_is_the_ttft() {
        let (s, _) = run(concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":null}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Let me\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{}}],\"timings\":{\"prompt_n\":12,\"prompt_ms\":24.0,\"predicted_per_second\":36.5}}\n\n",
        ))
        .await;
        assert!(s.ttft_ms.is_some());
        assert_eq!(s.content, "");
        // Derived from the counts: 12 tokens in 24 ms.
        assert_eq!(s.prompt_per_second, Some(500.0));
    }

    #[tokio::test]
    async fn cached_prompts_yield_no_prompt_figures() {
        let (s, _) = run(concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{}}],\"timings\":{\"cache_n\":11,\"prompt_n\":1,\"prompt_per_second\":36.0,\"predicted_per_second\":36.5}}\n\n",
        ))
        .await;
        assert_eq!(s.prompt_per_second, None);
        assert_eq!(s.ttft_ms, None);
        assert_eq!(s.predicted_per_second, Some(36.5));
    }

    #[test]
    fn prompt_count_must_be_plausible_for_the_text() {
        assert!(prompt_processed(Some(4_000), Some(0), 16_000));
        assert!(prompt_processed(None, None, 16_000));
        assert!(!prompt_processed(Some(1), None, 16_000));
        assert!(!prompt_processed(Some(100), Some(3_900), 600));
    }
}
