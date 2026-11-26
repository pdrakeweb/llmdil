//! Step analysis and the results table.
//!
//! [`analyze`] turns one finished execution (exit, captured output)
//! into a [`StepAnalysis`]: decoded exit, parsed results, CUDA error
//! blocks, and a short stderr excerpt around the error. [`table`]
//! renders the per-variant comparison:
//!
//! | variant | exit | decoded | pp t/s per depth | failing depth | CUDA error excerpt |

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ntstatus::{self, DecodedExit};
use crate::parsers::{self, BenchParse, CudaError, ServerTimings};
use crate::recipe::{Expect, ParserKind};

/// How a step ended, judged against its `expect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Exit 0 (or a successful request).
    Ok,
    /// Crashed or failed, and the step said that is evidence.
    ExpectedFailure,
    Failed,
    TimedOut,
    Cancelled,
    /// Not run (the run was cancelled or an earlier step failed hard).
    Skipped,
}

/// Raw facts of one finished execution.
#[derive(Debug, Clone, Default)]
pub struct Finished<'a> {
    pub exit_code: Option<i64>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub stdout: &'a str,
    pub stderr: &'a str,
    pub parser: Option<ParserKind>,
    pub expect: Expect,
    /// The step's resolved args (for llama-bench `-d` lists).
    pub args: &'a [String],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepAnalysis {
    pub verdict: Verdict,
    pub exit: Option<DecodedExit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bench: Option<BenchParse>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requested_depths: Vec<u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pp_ts_per_depth: BTreeMap<u64, f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failing_depth: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timings: Option<ServerTimings>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cuda_errors: Vec<CudaError>,
    /// Lines around the error (or the tail) of stderr.
    pub stderr_excerpt: String,
}

/// Lines of context kept above/below the error anchor.
pub const EXCERPT_BEFORE: usize = 8;
pub const EXCERPT_AFTER: usize = 6;

pub fn analyze(f: &Finished<'_>) -> StepAnalysis {
    let exit = match (f.exit_code, f.signal) {
        (Some(c), _) => Some(ntstatus::decode(c)),
        (None, Some(s)) => Some(ntstatus::decode_signal(s)),
        _ => None,
    };
    let crashed = exit.as_ref().is_some_and(|e| e.crash);
    let nonzero = exit.as_ref().is_some_and(|e| e.code != Some(0));
    let verdict = if f.cancelled {
        Verdict::Cancelled
    } else if f.timed_out {
        Verdict::TimedOut
    } else if exit.is_none() {
        Verdict::Failed
    } else if !nonzero {
        Verdict::Ok
    } else {
        match f.expect {
            Expect::MayFail => Verdict::ExpectedFailure,
            Expect::MayCrash if crashed => Verdict::ExpectedFailure,
            _ => Verdict::Failed,
        }
    };
    let mut a = StepAnalysis {
        verdict,
        exit,
        bench: None,
        requested_depths: Vec::new(),
        pp_ts_per_depth: BTreeMap::new(),
        failing_depth: None,
        timings: None,
        cuda_errors: parsers::parse_cuda_errors(f.stderr),
        stderr_excerpt: parsers::stderr_excerpt(f.stderr, EXCERPT_BEFORE, EXCERPT_AFTER),
    };
    match f.parser {
        Some(ParserKind::LlamaBench) => {
            // llama-bench prints results on stdout; a JSON object or
            // table row on stderr would be unusual but harmless.
            let mut bench = parsers::parse_llama_bench(f.stdout);
            if bench.rows.is_empty() {
                bench = parsers::parse_llama_bench(f.stderr);
            }
            a.requested_depths = crate::argv::flag_value(f.args, &["-d", "--n-depth"])
                .map(|d| parsers::expand_bench_list(&d))
                .unwrap_or_default();
            a.pp_ts_per_depth = parsers::pp_per_depth(&bench.rows);
            if verdict != Verdict::Ok {
                a.failing_depth = parsers::failing_depth(&a.requested_depths, &bench.rows);
            }
            a.bench = Some(bench);
        }
        Some(ParserKind::ServerTimings) => {
            let mut t = parsers::parse_server_timings(f.stderr);
            let o = parsers::parse_server_timings(f.stdout);
            t.prompt_eval.extend(o.prompt_eval);
            t.eval.extend(o.eval);
            if t.last_progress.is_none() {
                t.last_progress = o.last_progress;
            }
            a.timings = Some(t);
        }
        _ => {}
    }
    a
}

/// One-line rendering of a CUDA/ggml error block.
pub fn cuda_one_line(e: &CudaError) -> String {
    let mut parts = Vec::new();
    if let Some(err) = &e.error {
        parts.push(format!("CUDA error: {err}"));
    }
    if let Some(a) = &e.assert {
        parts.push(format!("GGML_ASSERT({a})"));
    }
    if let Some(f) = &e.function {
        let loc = e
            .location
            .as_deref()
            .map(|l| l.rsplit(['/', '\\']).next().unwrap_or(l).to_string());
        parts.push(match loc {
            Some(l) => format!("in {f} ({l})"),
            None => format!("in {f}"),
        });
    }
    if let Some(d) = e.device {
        parts.push(format!("device {d}"));
    }
    if let Some(c) = &e.call {
        parts.push(c.clone());
    }
    if let Some(s) = &e.abort_site {
        parts.push(format!("at {s}"));
    }
    parts.join(", ")
}

/// A results-table row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub variant: String,
    pub verdict: Verdict,
    pub exit: String,
    pub decoded: String,
    pub pp_ts_per_depth: BTreeMap<u64, f64>,
    pub failing_depth: Option<u64>,
    pub cuda_error: String,
}

pub fn row(label: &str, a: &StepAnalysis) -> Row {
    Row {
        variant: label.to_string(),
        verdict: a.verdict,
        exit: a
            .exit
            .as_ref()
            .map(|e| e.display.clone())
            .unwrap_or_else(|| match a.verdict {
                Verdict::TimedOut => "timeout".into(),
                Verdict::Cancelled => "cancelled".into(),
                Verdict::Skipped => "skipped".into(),
                _ => "—".into(),
            }),
        decoded: a
            .exit
            .as_ref()
            .map(|e| e.meaning.clone())
            .unwrap_or_default(),
        pp_ts_per_depth: a.pp_ts_per_depth.clone(),
        failing_depth: a.failing_depth,
        cuda_error: a.cuda_errors.first().map(cuda_one_line).unwrap_or_default(),
    }
}

fn fmt_ts(v: f64) -> String {
    if v >= 100.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

/// Render rows as a markdown table.
pub fn table(rows: &[Row]) -> String {
    let mut out = String::from(
        "| variant | exit | decoded | pp t/s per depth | failing depth | CUDA error excerpt |\n|---|---|---|---|---|---|\n",
    );
    for r in rows {
        let pp = if r.pp_ts_per_depth.is_empty() {
            "—".to_string()
        } else {
            r.pp_ts_per_depth
                .iter()
                .map(|(d, v)| format!("d{d}: {}", fmt_ts(*v)))
                .collect::<Vec<_>>()
                .join(", ")
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            cell(&r.variant),
            cell(&r.exit),
            cell(if r.decoded.is_empty() {
                "—"
            } else {
                &r.decoded
            }),
            cell(&pp),
            r.failing_depth
                .map(|d| d.to_string())
                .unwrap_or_else(|| "—".into()),
            cell(if r.cuda_error.is_empty() {
                "—"
            } else {
                &r.cuda_error
            }),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BENCH_PARTIAL: &str = r#"[
  {"n_prompt": 512, "n_gen": 0, "n_depth": 0, "avg_ts": 812.5, "stddev_ts": 3.2},
  {"n_prompt": 512, "n_gen": 0, "n_depth": 4096, "avg_ts": 640.25, "stddev_ts": 2.0},
"#;
    const STDERR: &str = r"ggml-cuda.cu:109: CUDA error
CUDA error: invalid argument
  current device: 0, in function ggml_cuda_kernel_launch at D:\a\ggml\src\ggml-cuda\common.cuh:1716
  cudaGetLastError()";

    fn args(s: &str) -> Vec<String> {
        s.split(' ').map(str::to_string).collect()
    }

    #[test]
    fn crash_variant_reads_as_expected_evidence() {
        let a = args("-m x -d 0,4096,8192 -fa 1");
        let an = analyze(&Finished {
            exit_code: Some(-1073740791),
            stdout: BENCH_PARTIAL,
            stderr: STDERR,
            parser: Some(ParserKind::LlamaBench),
            expect: Expect::MayCrash,
            args: &a,
            ..Default::default()
        });
        assert_eq!(an.verdict, Verdict::ExpectedFailure);
        assert_eq!(an.failing_depth, Some(8192));
        assert_eq!(an.pp_ts_per_depth.len(), 2);
        let r = row("-d 0,4096,8192 -fa 1", &an);
        assert_eq!(r.exit, "0xc0000409");
        assert!(r
            .decoded
            .starts_with("abort() — GGML_ABORT/fast-fail, not memory corruption"));
        assert_eq!(
            r.cuda_error,
            "CUDA error: invalid argument, in ggml_cuda_kernel_launch (common.cuh:1716), device 0, cudaGetLastError(), at ggml-cuda.cu:109"
        );
        let t = table(&[r]);
        assert!(t.contains("| d0: 812, d4096: 640 | 8192 |"), "{t}");
    }

    #[test]
    fn verdicts_follow_expect() {
        let a: Vec<String> = Vec::new();
        let base = Finished {
            exit_code: Some(-1073740791),
            args: &a,
            ..Default::default()
        };
        assert_eq!(analyze(&base).verdict, Verdict::Failed);
        assert_eq!(
            analyze(&Finished {
                expect: Expect::MayFail,
                exit_code: Some(3),
                ..base.clone()
            })
            .verdict,
            Verdict::ExpectedFailure
        );
        assert_eq!(
            analyze(&Finished {
                expect: Expect::MayCrash,
                exit_code: Some(3),
                ..base.clone()
            })
            .verdict,
            Verdict::Failed,
            "an orderly non-zero exit is not a crash"
        );
        assert_eq!(
            analyze(&Finished {
                exit_code: Some(0),
                ..base.clone()
            })
            .verdict,
            Verdict::Ok
        );
        assert_eq!(
            analyze(&Finished {
                timed_out: true,
                exit_code: None,
                ..base.clone()
            })
            .verdict,
            Verdict::TimedOut
        );
    }
}
