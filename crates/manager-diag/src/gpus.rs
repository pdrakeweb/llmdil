//! Which GPUs a resolved command line needs.
//!
//! llama.cpp tools see the devices `CUDA_VISIBLE_DEVICES` leaves
//! visible (all of them by default) and, with layers on the GPU
//! (`-ngl` > 0, the default for llama-bench), split across every
//! visible device unless `-ts`, `-dev`, or `-sm none` + `-mg` narrow
//! it. Device positions in those flags are relative to the visible
//! list, so they are mapped back to physical indices here.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::argv;

/// The inventory facts inference needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuRef {
    pub index: u32,
    pub uuid: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GpuNeed {
    pub gpus: BTreeSet<u32>,
    /// How the set was derived, for the plan view.
    pub reasons: Vec<String>,
}

/// Is `exe` (a resolved path or name) a GPU-using llama.cpp tool?
pub fn is_gpu_tool(exe: &str) -> bool {
    let file = exe
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(exe)
        .to_ascii_lowercase();
    let stem = file.trim_end_matches(".exe");
    stem.starts_with("llama-")
        && !matches!(
            stem,
            "llama-gguf" | "llama-gguf-split" | "llama-quantize" | "llama-tokenize"
        )
}

fn visible(env: &BTreeMap<String, String>, inventory: &[GpuRef]) -> (Vec<u32>, Option<String>) {
    let all: Vec<u32> = if inventory.is_empty() {
        vec![0]
    } else {
        inventory.iter().map(|g| g.index).collect()
    };
    let Some(cvd) = env
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("CUDA_VISIBLE_DEVICES"))
        .map(|(_, v)| v.clone())
    else {
        return (all, None);
    };
    let mut out = Vec::new();
    for e in cvd.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        if let Ok(i) = e.parse::<u32>() {
            out.push(i);
        } else if let Some(g) = inventory.iter().find(|g| {
            !g.uuid.is_empty() && (g.uuid.eq_ignore_ascii_case(e) || e.starts_with(&g.uuid))
        }) {
            out.push(g.index);
        }
    }
    (out, Some(format!("CUDA_VISIBLE_DEVICES={cvd}")))
}

/// Infer the GPU set one command needs.
pub fn infer(
    exe: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    inventory: &[GpuRef],
) -> GpuNeed {
    let mut need = GpuNeed::default();
    if !is_gpu_tool(exe) {
        return need;
    }
    let (vis, vis_reason) = visible(env, inventory);
    if let Some(r) = vis_reason {
        need.reasons.push(r);
    }
    if vis.is_empty() {
        need.reasons.push("no visible CUDA device".into());
        return need;
    }
    if let Some(ngl) = argv::flag_value(args, &["-ngl", "--n-gpu-layers", "--gpu-layers"]) {
        let all_zero = ngl
            .split([',', '/'])
            .all(|v| v.trim().parse::<i64>().map(|n| n == 0).unwrap_or(false));
        if all_zero {
            need.reasons.push(format!("-ngl {ngl}: CPU only"));
            return need;
        }
    }
    let at = |pos: usize| vis.get(pos).copied();
    if let Some(dev) = argv::flag_value(args, &["-dev", "--device"]) {
        if dev.eq_ignore_ascii_case("none") {
            need.reasons.push("--device none: CPU only".into());
            return need;
        }
        for d in dev.split([',', '/']) {
            let d = d.trim().to_ascii_uppercase();
            if let Some(n) = d.strip_prefix("CUDA").and_then(|n| n.parse::<usize>().ok()) {
                if let Some(i) = at(n) {
                    need.gpus.insert(i);
                }
            }
        }
        need.reasons.push(format!("--device {dev}"));
        return need;
    }
    if let Some(ts) = argv::flag_value(args, &["-ts", "--tensor-split"]) {
        // llama-bench: `,` separates test values, `/` the split itself.
        let tests: Vec<Vec<&str>> = if ts.contains('/') {
            ts.split(',').map(|t| t.split('/').collect()).collect()
        } else {
            vec![ts.split(',').collect()]
        };
        for parts in tests {
            for (pos, p) in parts.iter().enumerate() {
                if p.trim().parse::<f64>().map(|f| f > 0.0).unwrap_or(false) {
                    if let Some(i) = at(pos) {
                        need.gpus.insert(i);
                    }
                }
            }
        }
        need.reasons.push(format!("--tensor-split {ts}"));
        return need;
    }
    let sm = argv::flag_value(args, &["-sm", "--split-mode"]).unwrap_or_default();
    if sm.eq_ignore_ascii_case("none") {
        let mg = argv::flag_value(args, &["-mg", "--main-gpu"])
            .and_then(|v| {
                v.split(',')
                    .next()
                    .and_then(|x| x.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        if let Some(i) = at(mg) {
            need.gpus.insert(i);
        }
        need.reasons
            .push(format!("--split-mode none, main GPU {mg}"));
        return need;
    }
    need.gpus.extend(vis.iter().copied());
    need.reasons
        .push("layers on GPU with no device selection: every visible GPU".into());
    need
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inv() -> Vec<GpuRef> {
        vec![
            GpuRef {
                index: 0,
                uuid: "GPU-aaaa".into(),
            },
            GpuRef {
                index: 1,
                uuid: "GPU-bbbb".into(),
            },
        ]
    }

    fn a(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn infers_from_visibility_split_and_devices() {
        let none = BTreeMap::new();
        let cvd: BTreeMap<String, String> =
            [("CUDA_VISIBLE_DEVICES".to_string(), "GPU-aaaa".to_string())].into();
        let bench = "llama-bench.exe";
        assert_eq!(
            infer(bench, &a("-m x -ngl 99"), &none, &inv()).gpus,
            BTreeSet::from([0, 1])
        );
        assert_eq!(
            infer(bench, &a("-m x -ngl 99"), &cvd, &inv()).gpus,
            BTreeSet::from([0])
        );
        assert!(infer(bench, &a("-ngl 0"), &none, &inv()).gpus.is_empty());
        assert_eq!(
            infer(bench, &a("-ts 1/0"), &none, &inv()).gpus,
            BTreeSet::from([0])
        );
        assert_eq!(
            infer("llama-server", &a("--tensor-split 0,1"), &none, &inv()).gpus,
            BTreeSet::from([1])
        );
        assert_eq!(
            infer(bench, &a("-dev CUDA1"), &none, &inv()).gpus,
            BTreeSet::from([1])
        );
        assert_eq!(
            infer(bench, &a("-sm none -mg 1"), &none, &inv()).gpus,
            BTreeSet::from([1])
        );
        let cvd1: BTreeMap<String, String> =
            [("CUDA_VISIBLE_DEVICES".to_string(), "1".to_string())].into();
        assert_eq!(
            infer(bench, &a("-dev CUDA0"), &cvd1, &inv()).gpus,
            BTreeSet::from([1])
        );
        assert!(infer("nvidia-smi", &a("-q"), &none, &inv()).gpus.is_empty());
        assert!(infer("llama-gguf.exe", &a("x r"), &none, &inv())
            .gpus
            .is_empty());
    }
}
