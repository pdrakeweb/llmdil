//! The shipped recipes, compiled in (like Bench's shipped prompts):
//! listed beside saved recipes, never written to disk, and a saved
//! recipe of the same name is refused so a built-in cannot be
//! shadowed.
//!
//! They lean on context variables the app provides for a backend
//! parameter (see [`crate::template::TemplateContext::alias_backend_params`]):
//! `{backend.model_path}`, `{backend.server_args}` (the backend's
//! exact llama-server argv minus `--port`), `{backend.bench_flags}`
//! (its flags translated to llama-bench spelling, see
//! [`server_flags_to_bench`]) and `{backend.cuda_visible_devices}`
//! (the visibility its real launch uses; empty — and then not set —
//! when the launch is unrestricted).

use std::collections::BTreeMap;

use serde_json::json;

use crate::recipe::{
    Expect, GgufSpec, HttpSpec, Param, ParserKind, Recipe, Requires, Step, StepKind,
};

fn param(name: &str, default: Option<&str>, description: &str) -> Param {
    Param {
        name: name.into(),
        default: default.map(str::to_string),
        description: description.into(),
    }
}

fn recipe(
    name: &str,
    title: &str,
    description: &str,
    params: Vec<Param>,
    steps: Vec<Step>,
) -> Recipe {
    Recipe {
        name: name.into(),
        title: title.into(),
        description: description.into(),
        version: crate::recipe::RECIPE_VERSION,
        params,
        env: BTreeMap::new(),
        requires: Requires::default(),
        steps,
        builtin: true,
        source: Some("builtin".into()),
    }
}

/// Every shipped recipe.
pub fn builtins() -> Vec<Recipe> {
    let mut bench = Step::exec(
        "llama-bench depth sweep",
        "{llamacpp_bin}/llama-bench",
        &[
            "-m",
            "{backend.model_path}",
            "-ngl",
            "99",
            "-p",
            "{p}",
            "-n",
            "0",
            "-d",
            "{depth}",
            "{backend.bench_flags}",
        ],
    );
    bench.description =
        "Prompt processing at increasing KV depth with the backend's current cache/attention/offload flags."
            .into();
    bench.parser = Some(ParserKind::LlamaBench);
    bench.expect = Expect::MayCrash;
    bench.timeout_secs = Some(3600);
    bench.env.insert(
        "CUDA_VISIBLE_DEVICES".into(),
        "{backend.cuda_visible_devices}".into(),
    );
    let mut depth = recipe(
        "llama-bench-depth",
        "llama-bench depth sweep (backend's current flags)",
        "Runs llama-bench against a backend's model with its current KV cache types, flash-attention and expert-offload flags, sweeping KV depth. The backend (and anything else on its GPUs) is stopped first and restored afterwards.",
        vec![
            param("backend", None, "backend id whose model and flags to bench"),
            param("depth", Some("0,4096,8192"), "llama-bench -d list"),
            param("p", Some("512"), "prompt tokens per test"),
        ],
        vec![bench],
    );
    depth.env.insert("CUDA_LAUNCH_BLOCKING".into(), "1".into());

    let mut g = Step::exec("gguf metadata", "", &[]);
    g.kind = StepKind::Gguf;
    g.gguf = Some(GgufSpec {
        path: "{model}".into(),
        keys: vec![
            "general.architecture".into(),
            "general.name".into(),
            "context_length".into(),
            "block_count".into(),
            "attention.".into(),
            "expert".into(),
            "rope".into(),
            "file_type".into(),
        ],
    });
    g.description =
        "Header metadata (architecture, attention geometry, experts, RoPE) read in-process.".into();
    let gguf = recipe(
        "gguf-metadata",
        "GGUF metadata dump",
        "Reads a model's GGUF header in-process (no process, no GPU): attention.key_length, head_count_kv, context length, expert counts, RoPE settings.",
        vec![param(
            "model",
            None,
            "a GGUF path, e.g. {backend.planner-moe.model_path} or {models_dir}/x.gguf",
        )],
        vec![g],
    );

    let smi_q = Step::exec(
        "per-GPU memory and utilization",
        "nvidia-smi",
        &[
            "--query-gpu=index,name,uuid,driver_version,memory.used,memory.total,utilization.gpu,temperature.gpu,power.draw,clocks.sm,pstate",
            "--format=csv",
        ],
    );
    let smi_apps = Step::exec(
        "compute processes",
        "nvidia-smi",
        &[
            "--query-compute-apps=gpu_uuid,pid,process_name,used_memory",
            "--format=csv",
        ],
    );
    let mut smi_full = Step::exec("full report", "nvidia-smi", &[]);
    smi_full.expect = Expect::MayFail;
    let smi = recipe(
        "nvidia-smi-snapshot",
        "nvidia-smi snapshot",
        "Point-in-time GPU state: memory, utilization, clocks, temperatures and the processes holding VRAM.",
        vec![],
        vec![smi_q, smi_apps, smi_full],
    );

    let mut server = Step::exec(
        "scratch llama-server (backend's exact flags)",
        "{llamacpp_bin}/llama-server",
        &["{backend.server_args}", "--port", "{scratch_port}"],
    );
    server.background = true;
    server.ready_url = Some("http://127.0.0.1:{scratch_port}/health".into());
    server.ready_timeout_secs = Some(1200);
    server.parser = Some(ParserKind::ServerTimings);
    server.expect = Expect::MayCrash;
    server.description = "The backend's real launch command on a scratch port, so the crash is captured with its exit code.".into();
    server.env.insert(
        "CUDA_VISIBLE_DEVICES".into(),
        "{backend.cuda_visible_devices}".into(),
    );
    let mut prompt = Step::exec("long synthetic prompt", "", &[]);
    prompt.kind = StepKind::Http;
    prompt.expect = Expect::MayFail;
    prompt.timeout_secs = Some(1800);
    prompt.http = Some(HttpSpec {
        url: "http://127.0.0.1:{scratch_port}/completion".into(),
        method: "POST".into(),
        body: Some(json!({
            "prompt": "{synthetic_prompt}",
            "n_predict": 16,
            "cache_prompt": false,
            "temperature": 0
        })),
        synthetic_prompt_tokens: Some("{prompt_tokens}".into()),
    });
    prompt.description = "Sends a prompt of about {prompt_tokens} tokens; a crash during prompt processing shows up as a failed request and the server's exit code.".into();
    let mut repro = recipe(
        "server-crash-repro",
        "Reproduce a backend crash with a long prompt",
        "Starts the backend's exact llama-server command line on a scratch port (the real backend is drained and stopped first, then restored), sends one long synthetic prompt, and captures the server's stderr, exit code and any CUDA/ggml error block.",
        vec![
            param("backend", None, "backend id to reproduce"),
            param("prompt_tokens", Some("8192"), "approximate prompt length in tokens"),
            param("scratch_port", Some("18099"), "loopback port for the scratch server"),
        ],
        vec![server, prompt],
    );
    repro.requires.stop_backends = vec!["{backend}".into()];
    repro.env.insert("CUDA_LAUNCH_BLOCKING".into(), "1".into());

    vec![depth, gguf, smi, repro]
}

/// Look up a shipped recipe.
pub fn builtin(name: &str) -> Option<Recipe> {
    builtins().into_iter().find(|r| r.name == name)
}

/// Translate llama-server flags into their llama-bench spelling:
/// cache types, flash attention, tensor overrides, expert offload,
/// batch sizes, threads and mmap. Server-only flags (`--jinja`,
/// `--parallel`, `--moe-cache-mib`, `-c`, ...) have no llama-bench
/// equivalent and are returned as `dropped`.
pub fn server_flags_to_bench(raw: &[String]) -> (Vec<String>, Vec<String>) {
    let mut out = Vec::new();
    let mut dropped = Vec::new();
    for it in crate::argv::group(raw) {
        let crate::argv::ArgItem::Flag { name, value, .. } = it else {
            if let crate::argv::ArgItem::Positional(p) = it {
                dropped.push(p);
            }
            continue;
        };
        let v = value.clone().unwrap_or_default();
        let mapped: Option<(&str, String)> = match name.as_str() {
            "-ctk" | "--cache-type-k" => Some(("-ctk", v.clone())),
            "-ctv" | "--cache-type-v" => Some(("-ctv", v.clone())),
            "-fa" | "--flash-attn" => Some((
                "-fa",
                match v.to_ascii_lowercase().as_str() {
                    "off" | "0" | "false" => "0".into(),
                    // `auto` resolves to on for CUDA backends that
                    // support it, which is what is under test.
                    _ => "1".into(),
                },
            )),
            "-ot" | "--override-tensor" => Some(("-ot", v.clone())),
            "--n-cpu-moe" | "-ncmoe" => Some(("-ncmoe", v.clone())),
            "-ub" | "--ubatch-size" => Some(("-ub", v.clone())),
            "-b" | "--batch-size" => Some(("-b", v.clone())),
            "-t" | "--threads" => Some(("-t", v.clone())),
            "-sm" | "--split-mode" => Some(("-sm", v.clone())),
            "-mg" | "--main-gpu" => Some(("-mg", v.clone())),
            "-ts" | "--tensor-split" => Some(("-ts", v.replace(',', "/"))),
            "--no-mmap" => Some(("-mmp", "0".into())),
            "-nkvo" | "--no-kv-offload" => Some(("-nkvo", "1".into())),
            _ => None,
        };
        match mapped {
            Some((f, val)) if !val.is_empty() => {
                out.push(f.to_string());
                out.push(val);
            }
            _ => {
                dropped.push(match value {
                    Some(val) => format!("{name} {val}"),
                    None => name,
                });
            }
        }
    }
    (out, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_validate_and_are_unique() {
        let all = builtins();
        assert_eq!(all.len(), 4);
        for r in &all {
            r.validate().unwrap_or_else(|e| panic!("{}: {e}", r.name));
            assert!(r.builtin);
        }
        assert!(builtin("server-crash-repro").is_some());
    }

    #[test]
    fn translates_the_planner_moe_flags() {
        let raw: Vec<String> = "--cache-type-k q8_0 --cache-type-v q8_0 --flash-attn auto -ot exps=CPU --moe-cache-mib 2048 --jinja"
            .split(' ')
            .map(str::to_string)
            .collect();
        let (b, dropped) = server_flags_to_bench(&raw);
        assert_eq!(
            b,
            "-ctk q8_0 -ctv q8_0 -fa 1 -ot exps=CPU"
                .split(' ')
                .collect::<Vec<_>>()
        );
        assert_eq!(dropped, vec!["--moe-cache-mib 2048", "--jinja"]);
        let (ts, _) = server_flags_to_bench(&["--tensor-split".into(), "1,0".into()]);
        assert_eq!(ts, vec!["-ts", "1/0"]);
    }
}
