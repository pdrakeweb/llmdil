//! Output parsers: llama-bench (JSON or markdown table), llama-server
//! timing lines, and CUDA/ggml error blocks; plus the stderr excerpt
//! an agent reads instead of megabytes of raw output. Output a parser
//! does not understand stays raw text.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// llama-bench
// ---------------------------------------------------------------------------

/// One llama-bench result row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchRow {
    /// `pp512`, `tg128`, `pp512 @ d4096`.
    pub test: String,
    pub n_prompt: u64,
    pub n_gen: u64,
    pub n_depth: u64,
    /// Mean tokens/second.
    pub avg_ts: f64,
    pub stddev_ts: Option<f64>,
    #[serde(default)]
    pub flash_attn: Option<String>,
    #[serde(default)]
    pub type_k: Option<String>,
    #[serde(default)]
    pub type_v: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchParse {
    /// `json`, `markdown`, or `none`.
    pub format: String,
    pub rows: Vec<BenchRow>,
}

/// Scan `text` for complete top-level JSON objects (llama-bench's
/// `-o json` prints one object per finished test inside an array; a
/// crash leaves the array unterminated, but every finished test is
/// still a complete object).
fn json_objects(text: &str) -> Vec<serde_json::Value> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'{' {
            i += 1;
            continue;
        }
        let start = i;
        let mut depth = 0i32;
        let mut in_str = false;
        let mut esc = false;
        let mut end = None;
        while i < b.len() {
            let c = b[i];
            if in_str {
                if esc {
                    esc = false;
                } else if c == b'\\' {
                    esc = true;
                } else if c == b'"' {
                    in_str = false;
                }
            } else if c == b'"' {
                in_str = true;
            } else if c == b'{' {
                depth += 1;
            } else if c == b'}' {
                depth -= 1;
                if depth == 0 {
                    end = Some(i);
                    break;
                }
            }
            i += 1;
        }
        match end {
            Some(e) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text[start..=e]) {
                    out.push(v);
                }
                i = e + 1;
            }
            None => break,
        }
    }
    out
}

fn num(v: &serde_json::Value, k: &str) -> Option<f64> {
    v.get(k).and_then(|x| x.as_f64())
}

fn str_of(v: &serde_json::Value, k: &str) -> Option<String> {
    match v.get(k) {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(serde_json::Value::Bool(b)) => Some(if *b { "1" } else { "0" }.to_string()),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn test_label(n_prompt: u64, n_gen: u64, n_depth: u64) -> String {
    let base = if n_prompt > 0 && n_gen > 0 {
        format!("pp{n_prompt}+tg{n_gen}")
    } else if n_prompt > 0 {
        format!("pp{n_prompt}")
    } else {
        format!("tg{n_gen}")
    };
    if n_depth > 0 {
        format!("{base} @ d{n_depth}")
    } else {
        base
    }
}

/// Parse a test label (`pp512 @ d4096`, `tg128`, `pp512+tg128`).
fn parse_test_label(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim();
    let (core, depth) = match s.split_once('@') {
        Some((c, d)) => {
            let d = d.trim().trim_start_matches('d');
            (c.trim(), d.parse::<u64>().ok()?)
        }
        None => (s, 0),
    };
    let mut pp = 0;
    let mut tg = 0;
    for part in core.split('+') {
        let part = part.trim();
        if let Some(n) = part.strip_prefix("pp") {
            pp = n.parse().ok()?;
        } else {
            tg = part.strip_prefix("tg")?.parse().ok()?;
        }
    }
    Some((pp, tg, depth))
}

fn parse_ts(cell: &str) -> Option<(f64, Option<f64>)> {
    let mut parts = cell.split('±');
    let avg = parts.next()?.trim().parse::<f64>().ok()?;
    let sd = parts.next().and_then(|s| s.trim().parse::<f64>().ok());
    Some((avg, sd))
}

/// Parse llama-bench output: JSON objects first, else the markdown
/// table.
pub fn parse_llama_bench(stdout: &str) -> BenchParse {
    let rows: Vec<BenchRow> = json_objects(stdout)
        .into_iter()
        .filter_map(|v| {
            let avg_ts = num(&v, "avg_ts")?;
            let n_prompt = num(&v, "n_prompt").unwrap_or(0.0) as u64;
            let n_gen = num(&v, "n_gen").unwrap_or(0.0) as u64;
            let n_depth = num(&v, "n_depth").unwrap_or(0.0) as u64;
            Some(BenchRow {
                test: test_label(n_prompt, n_gen, n_depth),
                n_prompt,
                n_gen,
                n_depth,
                avg_ts,
                stddev_ts: num(&v, "stddev_ts"),
                flash_attn: str_of(&v, "flash_attn"),
                type_k: str_of(&v, "type_k"),
                type_v: str_of(&v, "type_v"),
            })
        })
        .collect();
    if !rows.is_empty() {
        return BenchParse {
            format: "json".into(),
            rows,
        };
    }
    // Markdown table.
    let mut header: Option<Vec<String>> = None;
    let mut rows = Vec::new();
    for line in stdout.lines() {
        let t = line.trim();
        if !t.starts_with('|') {
            continue;
        }
        let cells: Vec<String> = t
            .trim_matches('|')
            .split('|')
            .map(|c| c.trim().to_string())
            .collect();
        if cells
            .iter()
            .all(|c| c.chars().all(|ch| ch == '-' || ch == ':'))
        {
            continue;
        }
        let h = match &header {
            None => {
                if cells.iter().any(|c| c == "test") && cells.iter().any(|c| c == "t/s") {
                    header = Some(cells);
                }
                continue;
            }
            Some(h) => h,
        };
        let col = |name: &str| h.iter().position(|c| c == name).and_then(|i| cells.get(i));
        let Some((pp, tg, depth)) = col("test").and_then(|c| parse_test_label(c)) else {
            continue;
        };
        let Some((avg, sd)) = col("t/s").and_then(|c| parse_ts(c)) else {
            continue;
        };
        rows.push(BenchRow {
            test: test_label(pp, tg, depth),
            n_prompt: pp,
            n_gen: tg,
            n_depth: depth,
            avg_ts: avg,
            stddev_ts: sd,
            flash_attn: col("fa").cloned(),
            type_k: col("type_k").cloned(),
            type_v: col("type_v").cloned(),
        });
    }
    BenchParse {
        format: if rows.is_empty() { "none" } else { "markdown" }.into(),
        rows,
    }
}

/// Expand a llama-bench list argument (`0,4096,8192`,
/// `0-8192+4096`, `512-4096*2`) into its values.
pub fn expand_bench_list(s: &str) -> Vec<u64> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if let Some((range, step)) = part.split_once('+') {
            if let (Some((a, b)), Ok(step)) = (range.split_once('-'), step.parse::<u64>()) {
                if let (Ok(a), Ok(b)) = (a.parse::<u64>(), b.parse::<u64>()) {
                    let mut v = a;
                    while v <= b && step > 0 && out.len() < 1024 {
                        out.push(v);
                        v += step;
                    }
                    continue;
                }
            }
        }
        if let Some((range, mult)) = part.split_once('*') {
            if let (Some((a, b)), Ok(m)) = (range.split_once('-'), mult.parse::<u64>()) {
                if let (Ok(a), Ok(b)) = (a.parse::<u64>(), b.parse::<u64>()) {
                    let mut v = a.max(1);
                    while v <= b && m > 1 && out.len() < 1024 {
                        out.push(v);
                        v *= m;
                    }
                    continue;
                }
            }
        }
        if let Ok(v) = part.parse::<u64>() {
            out.push(v);
        }
    }
    out
}

/// Prompt-processing t/s per depth (the pp rows).
pub fn pp_per_depth(rows: &[BenchRow]) -> BTreeMap<u64, f64> {
    rows.iter()
        .filter(|r| r.n_prompt > 0 && r.n_gen == 0)
        .map(|r| (r.n_depth, r.avg_ts))
        .collect()
}

/// The first requested depth with no prompt-processing result — the
/// depth a crashed run died at. `None` when every requested depth
/// produced a row (or none was requested).
pub fn failing_depth(requested: &[u64], rows: &[BenchRow]) -> Option<u64> {
    let done = pp_per_depth(rows);
    let any_pp = rows.iter().any(|r| r.n_prompt > 0);
    let done_tg: Vec<u64> = rows
        .iter()
        .filter(|r| r.n_gen > 0)
        .map(|r| r.n_depth)
        .collect();
    requested.iter().copied().find(|d| {
        if any_pp || done_tg.is_empty() {
            !done.contains_key(d)
        } else {
            !done_tg.contains(d)
        }
    })
}

// ---------------------------------------------------------------------------
// llama-server timings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Timing {
    pub ms: f64,
    pub tokens: u64,
    pub tokens_per_second: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ServerTimings {
    pub prompt_eval: Vec<Timing>,
    pub eval: Vec<Timing>,
    /// The last `prompt processing, n_tokens = N, progress = P` line.
    pub last_progress: Option<(u64, f64)>,
}

fn parse_timing(rest: &str) -> Option<Timing> {
    // "=    1234.56 ms /  7680 tokens (    0.16 ms per token,  6220.40 tokens per second)"
    let rest = rest.trim().trim_start_matches('=').trim();
    let (ms_part, after) = rest.split_once("ms")?;
    let ms = ms_part.trim().parse::<f64>().ok()?;
    let after = after.trim().trim_start_matches('/').trim();
    let (tok_part, tail) = after.split_once("tokens")?;
    let tokens = tok_part.trim().parse::<u64>().ok()?;
    let tps = tail.split(',').nth(1).and_then(|s| {
        s.split_whitespace()
            .next()
            .and_then(|n| n.parse::<f64>().ok())
    });
    Some(Timing {
        ms,
        tokens,
        tokens_per_second: tps,
    })
}

pub fn parse_server_timings(text: &str) -> ServerTimings {
    let mut out = ServerTimings::default();
    for line in text.lines() {
        if let Some(i) = line.find("prompt eval time") {
            if let Some(t) = parse_timing(&line[i + "prompt eval time".len()..]) {
                out.prompt_eval.push(t);
            }
        } else if let Some(i) = line.find("eval time") {
            if let Some(t) = parse_timing(&line[i + "eval time".len()..]) {
                out.eval.push(t);
            }
        }
        if let Some(i) = line.find("prompt processing, n_tokens =") {
            let rest = &line[i + "prompt processing, n_tokens =".len()..];
            let n = rest
                .split(',')
                .next()
                .and_then(|s| s.trim().parse::<u64>().ok());
            let p = rest
                .split("progress =")
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.trim_end_matches(',').parse::<f64>().ok());
            if let (Some(n), Some(p)) = (n, p) {
                out.last_progress = Some((n, p));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// CUDA / ggml error blocks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CudaError {
    /// `ggml-cuda.cu:109` — where the abort was raised.
    pub abort_site: Option<String>,
    /// `invalid argument`.
    pub error: Option<String>,
    pub device: Option<u32>,
    /// `ggml_cuda_kernel_launch`.
    pub function: Option<String>,
    /// `...\ggml-cuda\common.cuh:1716`.
    pub location: Option<String>,
    /// The failing call expression (`cudaGetLastError()`).
    pub call: Option<String>,
    /// A `GGML_ASSERT(...)` condition when the abort was an assert.
    pub assert: Option<String>,
    /// Index of the anchor line in the scanned text.
    pub line_index: usize,
}

/// `path:123: message` → (`path:123`, `message`). The path may itself
/// contain a drive colon (`D:\a\b.cu:12: msg`).
fn split_site(line: &str) -> Option<(String, String)> {
    let t = line.trim();
    let bytes = t.as_bytes();
    let mut i = 0;
    while let Some(off) = t[i..].find(':') {
        let p = i + off;
        // digits after the colon, then ':'
        let digits: String = t[p + 1..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !digits.is_empty() && bytes.get(p + 1 + digits.len()) == Some(&b':') {
            let site = t[..p + 1 + digits.len()].to_string();
            let msg = t[p + 2 + digits.len()..].trim().to_string();
            let file = &t[..p];
            if file.contains('.') && !file.contains(' ') {
                return Some((short_path(&site), msg));
            }
        }
        i = p + 1;
    }
    None
}

/// The last path component(s) of a site: keep it readable.
fn short_path(site: &str) -> String {
    site.rsplit(['/', '\\']).next().unwrap_or(site).to_string()
}

/// Find CUDA/ggml error blocks in `text` (stderr), first first.
pub fn parse_cuda_errors(text: &str) -> Vec<CudaError> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<CudaError> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        if let Some(rest) = line.strip_prefix("CUDA error:") {
            let mut e = CudaError {
                error: Some(rest.trim().to_string()),
                line_index: i,
                ..Default::default()
            };
            if i > 0 {
                if let Some((site, msg)) = split_site(lines[i - 1]) {
                    if msg.contains("CUDA error") {
                        e.abort_site = Some(site);
                        e.line_index = i - 1;
                    }
                }
            }
            let mut j = i + 1;
            while j < lines.len() && j <= i + 4 {
                let l = lines[j].trim();
                if let Some(rest) = l.strip_prefix("current device:") {
                    let (dev, after) = rest.split_once(',').unwrap_or((rest, ""));
                    e.device = dev.trim().parse().ok();
                    if let Some(f) = after.trim().strip_prefix("in function") {
                        let (func, loc) = f.split_once(" at ").unwrap_or((f, ""));
                        e.function = Some(func.trim().to_string());
                        if !loc.trim().is_empty() {
                            e.location = Some(loc.trim().to_string());
                        }
                    }
                } else if e.function.is_some() && e.call.is_none() && !l.is_empty() {
                    if l.contains('(') && !l.contains("CUDA error") {
                        e.call = Some(l.to_string());
                    }
                    break;
                }
                j += 1;
            }
            out.push(e);
            i = j.max(i + 1);
            continue;
        }
        if let Some(pos) = line.find("GGML_ASSERT(") {
            let cond = line[pos + "GGML_ASSERT(".len()..]
                .rsplit_once(") failed")
                .map(|(c, _)| c.to_string())
                .unwrap_or_else(|| line[pos..].to_string());
            out.push(CudaError {
                abort_site: split_site(line).map(|(s, _)| s),
                assert: Some(cond),
                line_index: i,
                ..Default::default()
            });
        }
        i += 1;
    }
    out
}

/// The index of the most informative error line in `lines`: a parsed
/// CUDA/ggml error first, else the last line that looks like an error.
pub fn error_anchor(text: &str) -> Option<usize> {
    if let Some(e) = parse_cuda_errors(text).first() {
        return Some(e.line_index);
    }
    let lines: Vec<&str> = text.lines().collect();
    lines.iter().rposition(|l| {
        let l = l.to_ascii_lowercase();
        l.contains("error")
            || l.contains("abort")
            || l.contains("fatal")
            || l.contains("exception")
            || l.contains("failed")
            || l.contains("out of memory")
    })
}

/// An excerpt around the error (`before` lines above the anchor,
/// `after` below), or the last `before + after` lines when nothing
/// looks like an error. Lines are capped at 400 chars.
pub fn stderr_excerpt(text: &str, before: usize, after: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    let (start, end) = match error_anchor(text) {
        Some(a) => (a.saturating_sub(before), (a + after + 1).min(lines.len())),
        None => (lines.len().saturating_sub(before + after), lines.len()),
    };
    lines[start..end]
        .iter()
        .map(|l| {
            if l.len() > 400 {
                let mut cut = 400;
                while !l.is_char_boundary(cut) {
                    cut -= 1;
                }
                format!("{}…", &l[..cut])
            } else {
                (*l).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const CRASH_STDERR: &str = r"... prompt processing, n_tokens = 7680, progress = 0.93 ...
ggml-cuda.cu:109: CUDA error
CUDA error: invalid argument
  current device: 0, in function ggml_cuda_kernel_launch at D:\a\llama.cpp\llama.cpp\ggml\src\ggml-cuda\common.cuh:1716
  cudaGetLastError()
[lifecycle] backend:planner-moe: crashed: exited unexpectedly (exit code: 0xc0000409)";

    #[test]
    fn parses_the_motivating_cuda_error_block() {
        let errs = parse_cuda_errors(CRASH_STDERR);
        assert_eq!(errs.len(), 1);
        let e = &errs[0];
        assert_eq!(e.abort_site.as_deref(), Some("ggml-cuda.cu:109"));
        assert_eq!(e.error.as_deref(), Some("invalid argument"));
        assert_eq!(e.device, Some(0));
        assert_eq!(e.function.as_deref(), Some("ggml_cuda_kernel_launch"));
        assert!(e.location.as_deref().unwrap().ends_with("common.cuh:1716"));
        assert_eq!(e.call.as_deref(), Some("cudaGetLastError()"));
        assert_eq!(e.line_index, 1);
        let ex = stderr_excerpt(CRASH_STDERR, 1, 3);
        assert!(ex.starts_with("... prompt processing"));
        assert!(ex.ends_with("cudaGetLastError()"));
        let t = parse_server_timings(CRASH_STDERR);
        assert_eq!(t.last_progress, Some((7680, 0.93)));
    }

    #[test]
    fn parses_ggml_assert() {
        let e = parse_cuda_errors(
            r"D:\a\llama.cpp\ggml\src\ggml-cuda\fattn.cu:290: GGML_ASSERT(Q->ne[3] == 1) failed",
        );
        assert_eq!(e[0].abort_site.as_deref(), Some("fattn.cu:290"));
        assert_eq!(e[0].assert.as_deref(), Some("Q->ne[3] == 1"));
    }

    #[test]
    fn llama_bench_json_survives_a_crash_mid_array() {
        let out = r#"[
  {
    "build_commit": "abc", "model_type": "qwen3moe ?B Q4_K - Medium",
    "n_prompt": 512, "n_gen": 0, "n_depth": 0, "type_k": "q8_0", "type_v": "q8_0",
    "flash_attn": true, "avg_ts": 812.5, "stddev_ts": 3.25, "samples_ts": [812.5]
  },
  {
    "n_prompt": 512, "n_gen": 0, "n_depth": 4096, "type_k": "q8_0", "type_v": "q8_0",
    "flash_attn": true, "avg_ts": 640.25, "stddev_ts": 2.0
  },
"#;
        let p = parse_llama_bench(out);
        assert_eq!(p.format, "json");
        assert_eq!(p.rows.len(), 2);
        assert_eq!(p.rows[1].test, "pp512 @ d4096");
        assert_eq!(p.rows[0].flash_attn.as_deref(), Some("1"));
        let depths = expand_bench_list("0,4096,8192");
        assert_eq!(failing_depth(&depths, &p.rows), Some(8192));
        assert_eq!(pp_per_depth(&p.rows)[&4096], 640.25);
    }

    #[test]
    fn llama_bench_markdown_table() {
        let out = "| model | size | params | backend | ngl | fa | test | t/s |\n| --- | ---: | ---: | --- | --: | -: | ---: | ---: |\n| qwen3moe 30B.A3B Q4_K - Medium | 17.28 GiB | 30.53 B | CUDA | 99 | 1 | pp512 | 1234.56 ± 12.34 |\n| qwen3moe 30B.A3B Q4_K - Medium | 17.28 GiB | 30.53 B | CUDA | 99 | 1 | pp512 @ d8192 | 456.78 ± 1.00 |\n| qwen3moe 30B.A3B Q4_K - Medium | 17.28 GiB | 30.53 B | CUDA | 99 | 1 | tg128 | 88.10 ± 0.50 |\n\nbuild: abc (1234)";
        let p = parse_llama_bench(out);
        assert_eq!(p.format, "markdown");
        assert_eq!(p.rows.len(), 3);
        assert_eq!(p.rows[1].n_depth, 8192);
        assert_eq!(p.rows[1].avg_ts, 456.78);
        assert_eq!(p.rows[2].n_gen, 128);
        assert_eq!(p.rows[0].stddev_ts, Some(12.34));
        assert_eq!(failing_depth(&[0, 8192], &p.rows), None);
        assert_eq!(parse_llama_bench("garbage").format, "none");
    }

    #[test]
    fn server_timing_lines() {
        let t = parse_server_timings("prompt eval time =    1234.56 ms /  7680 tokens (    0.16 ms per token,  6220.40 tokens per second)\n       eval time =     789.00 ms /   128 tokens (    6.16 ms per token,   162.23 tokens per second)\n      total time =    2023.56 ms /  7808 tokens");
        assert_eq!(t.prompt_eval[0].tokens, 7680);
        assert_eq!(t.prompt_eval[0].tokens_per_second, Some(6220.40));
        assert_eq!(t.eval[0].ms, 789.0);
        assert_eq!(expand_bench_list("0-8192+4096"), vec![0, 4096, 8192]);
        assert_eq!(expand_bench_list("512-4096*2"), vec![512, 1024, 2048, 4096]);
    }
}
