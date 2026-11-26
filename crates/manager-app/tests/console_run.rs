//! Debug console end to end, through the registry dispatch (the same
//! path the REST API uses), against `diag-fake-tool` copied into a
//! scratch data dir as `llamacpp/llama-bench(.exe)` and
//! `llamacpp/llama-server(.exe)`:
//!
//! - the motivating paste becomes a 3-variant recipe, refuses to run
//!   until the GPU-sharing backend is confirmed, stops it (drained),
//!   runs the variants, and restores it after the crash;
//! - a cancelled run still restores the backend;
//! - run artifacts are complete; the audit log carries the resolved
//!   command lines; backend stderr persists with a `.prev` rotation;
//! - non-loopback API callers and short roles are refused.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use manager_app::registry::{dispatch, Op};
use manager_app::{AppState, EventSink, NoopSink};
use serde_json::{json, Value};

const FAKE: &str = env!("CARGO_BIN_EXE_diag-fake-tool");

fn exe_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

struct Rig {
    state: Arc<AppState>,
    sink: Arc<dyn EventSink>,
    dir: PathBuf,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn rig(tag: &str) -> Rig {
    let dir = std::env::temp_dir().join(format!("llmdil-console-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("llamacpp")).unwrap();
    std::fs::create_dir_all(dir.join("models")).unwrap();
    for stem in ["llama-bench", "llama-server"] {
        std::fs::copy(FAKE, dir.join("llamacpp").join(exe_name(stem))).unwrap();
    }
    std::fs::write(
        dir.join("models").join("fake.gguf"),
        manager_diag::gguf::build_header(&[
            ("general.architecture", json!("qwen3moe")),
            ("qwen3moe.attention.key_length", json!(128)),
            ("qwen3moe.attention.head_count_kv", json!(4)),
        ]),
    )
    .unwrap();
    let mut cfg = manager_config::AppConfig::default_config();
    cfg.data_dir = dir.clone();
    cfg.gateway.groups.clear();
    cfg.models.clear();
    let mut b = cfg.backends[0].clone();
    b.id = "victim".into();
    b.enabled = true;
    b.model_file = PathBuf::from("models").join("fake.gguf");
    b.port = free_port();
    b.flags.tensor_split = vec![];
    b.raw_flags =
        Some("--cache-type-k q8_0 --cache-type-v q8_0 --flash-attn auto --parallel 4".into());
    b.flags_advanced = true;
    cfg.backends = vec![b];
    let state = Arc::new(AppState::new(cfg, dir.join("config.json"), dir.clone()));
    Rig {
        state,
        sink: Arc::new(NoopSink),
        dir,
    }
}

async fn call(r: &Rig, op: Op, args: Value) -> Result<Value, manager_app::ApiError> {
    dispatch(&r.state, &r.sink, op, args).await
}

async fn wait_live(r: &Rig, id: &str, live: bool, within: Duration) {
    let deadline = Instant::now() + within;
    loop {
        let l = manager_app::lifecycle::backend_liveness(&r.state).await;
        let is_live = l.get(id).is_some_and(|b| b.state.is_live());
        if is_live == live {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "backend {id} live={live} not reached: {:?}",
            l.get(id).map(|b| b.state)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Follow a run's output until it ends; returns every line.
async fn follow(r: &Rig, run_id: &str) -> Vec<Value> {
    let mut since = 0;
    let mut lines = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let o = call(
            r,
            Op::ConsoleRunOutput,
            json!({ "run_id": run_id, "since": since, "wait_ms": 2000 }),
        )
        .await
        .unwrap();
        for l in o["lines"].as_array().unwrap() {
            lines.push(l.clone());
        }
        since = o["next"].as_u64().unwrap();
        if o["done"] == true && o["more"] != true {
            return lines;
        }
        assert!(
            Instant::now() < deadline,
            "run did not finish; last output: {:?}",
            lines.iter().rev().take(10).collect::<Vec<_>>()
        );
    }
}

fn text_of(lines: &[Value]) -> String {
    lines
        .iter()
        .map(|l| l["line"].as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn paste(models: &Path) -> String {
    format!(
        "$env:CUDA_LAUNCH_BLOCKING=1\n$m=\"{}\\fake.gguf\"\n.\\llama-bench.exe -m $m -ngl 99 -ot exps=CPU -p 512 -n 0 -d 0,4096,8192 -fa 1 -ctk q8_0 -ctv q8_0   # expect crash at d=8192\n.\\llama-bench.exe -m $m -ngl 99 -ot exps=CPU -p 512 -n 0 -d 8192 -fa 1 -ctk f16 -ctv f16\n.\\llama-bench.exe -m $m -ngl 99 -ot exps=CPU -p 512 -n 0 -d 8192 -fa 0 -ctk q8_0 -ctv f16\n",
        models.display()
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paste_to_results_with_backend_stop_and_restore_after_a_crash() {
    let r = rig("crash");
    call(&r, Op::StartBackend, json!({ "id": "victim" }))
        .await
        .unwrap();
    wait_live(&r, "victim", true, Duration::from_secs(30)).await;

    // Import: 3 variants, $m resolved, env on every step.
    let imp = call(
        &r,
        Op::ConsoleImport,
        json!({ "text": paste(&r.dir.join("models")), "name": "fa kv depth ab" }),
    )
    .await
    .unwrap();
    let recipe = imp["recipe"].clone();
    assert_eq!(recipe["steps"].as_array().unwrap().len(), 1);
    assert_eq!(recipe["steps"][0]["variants"].as_array().unwrap().len(), 3);
    assert_eq!(recipe["env"]["CUDA_LAUNCH_BLOCKING"], "1");
    assert_eq!(recipe["params"][0]["default"], "{models_dir}/fake.gguf");
    let plan = &imp["plan"];
    assert_eq!(plan["errors"], json!([]), "{plan:#}");
    let steps = plan["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 3);
    for s in steps {
        assert_eq!(s["env"]["CUDA_LAUNCH_BLOCKING"], "1");
        assert!(
            s["command_line"].as_str().unwrap().contains("fake.gguf"),
            "$m resolved: {}",
            s["command_line"]
        );
        assert!(
            s["command_line"].as_str().unwrap().ends_with("-o json"),
            "llama-bench gets -o json"
        );
    }
    assert!(plan["gpus"].as_array().unwrap().contains(&json!(0)));
    assert_eq!(plan["stop_backends"][0]["id"], "victim");

    // Save it, then try to run: refused until the stop is confirmed.
    call(&r, Op::ConsoleRecipeCreate, json!({ "recipe": recipe }))
        .await
        .unwrap();
    let first = call(
        &r,
        Op::ConsoleRunStart,
        json!({ "recipe": "fa-kv-depth-ab" }),
    )
    .await
    .unwrap();
    assert_eq!(first["started"], false);
    assert_eq!(first["needs_confirmation"], true);
    assert_eq!(first["confirm_stop"], json!(["victim"]));

    let started = call(
        &r,
        Op::ConsoleRunStart,
        json!({ "recipe": "fa-kv-depth-ab", "confirm_stop": ["victim"] }),
    )
    .await
    .unwrap();
    assert_eq!(started["started"], true, "{started:#}");
    let run_id = started["run_id"].as_str().unwrap().to_string();
    let lines = follow(&r, &run_id).await;
    let text = text_of(&lines);
    assert!(
        text.contains("CUDA_LAUNCH_BLOCKING=1"),
        "env applied:\n{text}"
    );
    assert!(
        text.contains("draining and stopping backend victim"),
        "{text}"
    );
    assert!(
        text.contains("CUDA error: invalid argument"),
        "live stderr:\n{text}"
    );
    assert!(
        text.contains("| variant | exit | decoded |"),
        "results table streamed:\n{text}"
    );

    let res = call(&r, Op::ConsoleRunResult, json!({ "run_id": run_id }))
        .await
        .unwrap();
    assert_eq!(res["status"], "completed", "{res:#}");
    let s = res["steps"].as_array().unwrap();
    assert_eq!(s.len(), 3);
    assert_eq!(s[0]["verdict"], "expected_failure");
    assert_eq!(s[1]["verdict"], "ok");
    assert_eq!(s[2]["verdict"], "ok");
    if cfg!(windows) {
        assert_eq!(s[0]["exit"]["display"], "0xc0000409");
        assert!(s[0]["exit"]["meaning"]
            .as_str()
            .unwrap()
            .starts_with("abort() — GGML_ABORT/fast-fail, not memory corruption"));
    } else {
        assert_eq!(s[0]["exit"]["name"], "SIGABRT");
    }
    assert_eq!(s[0]["analysis"]["failing_depth"], 8192);
    assert_eq!(s[0]["analysis"]["pp_ts_per_depth"]["4096"], 795.2);
    assert_eq!(
        s[0]["analysis"]["cuda_errors"][0]["function"],
        "ggml_cuda_kernel_launch"
    );
    assert!(s[0]["analysis"]["stderr_excerpt"]
        .as_str()
        .unwrap()
        .contains("cudaGetLastError()"));
    assert!(s[0]["telemetry"]["before"].is_array());
    assert!(
        s[0]["telemetry"].get("samples").is_none(),
        "compact result drops samples"
    );
    let table = res["summary"]["table"].as_str().unwrap();
    assert!(table.contains("| 8192 |"), "{table}");
    assert_eq!(res["backends"]["stopped"][0]["id"], "victim");
    assert_eq!(res["backends"]["stopped"][0]["ok"], true);
    assert_eq!(
        res["backends"]["restored"][0]["ok"], true,
        "{:#}",
        res["backends"]
    );
    wait_live(&r, "victim", true, Duration::from_secs(30)).await;

    // Artifacts.
    let run_dir = r.dir.join("diagnostics").join("runs").join(&run_id);
    for f in [
        "run.json",
        "output.jsonl",
        "steps/00.stdout.log",
        "steps/00.stderr.log",
        "steps/02.stdout.log",
    ] {
        assert!(run_dir.join(f).is_file(), "missing {f}");
    }
    let full = call(
        &r,
        Op::ConsoleRunResult,
        json!({ "run_id": run_id, "full": true }),
    )
    .await
    .unwrap();
    assert!(full["steps"][0]["stderr"]
        .as_str()
        .unwrap()
        .contains("ggml-cuda.cu:109"));
    assert!(full["steps"][0]["telemetry"]["samples"].is_array());
    let export = call(&r, Op::ConsoleRunExport, json!({ "run_id": run_id }))
        .await
        .unwrap();
    assert!(Path::new(export["path"].as_str().unwrap()).is_file());
    let history = call(&r, Op::ConsoleRunsList, json!({})).await.unwrap();
    assert_eq!(history["runs"][0]["id"], json!(run_id));

    // Lifecycle markers and the audit trail.
    let lifecycle =
        std::fs::read_to_string(r.dir.join("logs").join("backend-victim.lifecycle.log")).unwrap();
    assert!(
        lifecycle.contains("console: draining and stopping"),
        "{lifecycle}"
    );
    assert!(
        lifecycle.contains("console: restarting after"),
        "{lifecycle}"
    );
    let audit = std::fs::read_to_string(r.dir.join("logs").join("admin-api-audit.jsonl")).unwrap();
    assert!(audit.contains("\"action\":\"console.import\""));
    let start = audit
        .lines()
        .find(|l| {
            l.contains("\"action\":\"console.run.start\"") && l.contains("\"result\":\"started\"")
        })
        .expect("run start audited");
    assert!(
        start.contains("fake.gguf") && start.contains("desktop-ui"),
        "{start}"
    );

    // Backend stderr persisted, with the previous run in .prev after
    // the restore restarted it; the console reads and greps both.
    let logs = call(
        &r,
        Op::ConsoleBackendLogs,
        json!({ "id": "victim", "prev": true, "grep": "listening" }),
    )
    .await
    .unwrap();
    assert!(logs["matching"].as_u64().unwrap() >= 1, "{logs:#}");
    // The restored backend writes its first lines a moment after the
    // start returns.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let cur = call(
            &r,
            Op::ConsoleExec,
            json!({ "line": "logs victim --grep listening" }),
        )
        .await
        .unwrap();
        assert_eq!(cur["ok"], true);
        if cur["text"].as_str().unwrap().contains("listening on") {
            break;
        }
        assert!(Instant::now() < deadline, "{cur:#}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Follow-up built-ins: gguf metadata and show.
    let g = call(
        &r,
        Op::ConsoleExec,
        json!({ "line": "gguf victim attention.key_length head_count_kv" }),
    )
    .await
    .unwrap();
    assert_eq!(
        g["data"]["keys"]["qwen3moe.attention.key_length"], 128,
        "{g:#}"
    );
    let show = call(
        &r,
        Op::ConsoleExec,
        json!({ "line": format!("show {run_id}") }),
    )
    .await
    .unwrap();
    assert!(show["text"]
        .as_str()
        .unwrap()
        .contains("CUDA error: invalid argument"));

    let _ = call(
        &r,
        Op::StopBackend,
        json!({ "id": "victim", "graceful": false }),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_run_still_restores_the_backend() {
    let r = rig("cancel");
    call(&r, Op::StartBackend, json!({ "id": "victim" }))
        .await
        .unwrap();
    wait_live(&r, "victim", true, Duration::from_secs(30)).await;
    let mut env = BTreeMap::new();
    env.insert("FAKE_SLEEP_MS".to_string(), "120000".to_string());
    let recipe = json!({
        "name": "slow",
        "steps": [
            { "name": "slow bench", "exe": "llama-bench", "args": ["-m", "{backend.victim.model_path}", "-ngl", "99", "-d", "0"], "env": env },
            { "name": "never runs", "exe": "llama-bench", "args": ["-d", "0"] }
        ]
    });
    let started = call(
        &r,
        Op::ConsoleRunStart,
        json!({ "recipe_doc": recipe, "confirm_all": true }),
    )
    .await
    .unwrap();
    assert_eq!(started["started"], true, "{started:#}");
    let run_id = started["run_id"].as_str().unwrap().to_string();
    // Wait for the step to be running, then cancel.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let o = call(
            &r,
            Op::ConsoleRunOutput,
            json!({ "run_id": run_id, "since": 0, "wait_ms": 1000, "limit": 5000 }),
        )
        .await
        .unwrap();
        if text_of(o["lines"].as_array().unwrap()).contains("sleeping 120000") {
            break;
        }
        assert!(Instant::now() < deadline, "step never started");
    }
    // A second run is refused while this one is active.
    let busy = call(
        &r,
        Op::ConsoleRunStart,
        json!({ "recipe_doc": recipe, "confirm_all": true }),
    )
    .await;
    assert!(
        matches!(busy, Err(manager_app::ApiError::Busy(_))),
        "{busy:?}"
    );
    let c = call(&r, Op::ConsoleExec, json!({ "line": "cancel" }))
        .await
        .unwrap();
    assert_eq!(c["ok"], true, "{c:#}");
    let lines = follow(&r, &run_id).await;
    let res = call(&r, Op::ConsoleRunResult, json!({ "run_id": run_id }))
        .await
        .unwrap();
    assert_eq!(res["status"], "cancelled", "{}", text_of(&lines));
    assert_eq!(res["steps"][0]["verdict"], "cancelled");
    assert_eq!(res["steps"][1]["verdict"], "skipped");
    assert_eq!(res["backends"]["restored"][0]["id"], "victim");
    assert_eq!(res["backends"]["restored"][0]["ok"], true);
    wait_live(&r, "victim", true, Duration::from_secs(30)).await;
    let _ = call(
        &r,
        Op::StopBackend,
        json!({ "id": "victim", "graceful": false }),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_crash_repro_captures_the_scratch_servers_exit() {
    let r = rig("repro");
    let port = free_port();
    let started = call(
        &r,
        Op::ConsoleRunStart,
        json!({ "recipe": "server-crash-repro", "params": { "backend": "victim", "prompt_tokens": "2048", "scratch_port": port.to_string() }, "set": { "--parallel": "1" } }),
    )
    .await
    .unwrap();
    assert_eq!(started["started"], true, "{started:#}");
    let server_cmd = started["plan"]["steps"][0]["command_line"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        server_cmd.contains("--cache-type-k q8_0") && server_cmd.contains("--parallel 1"),
        "{server_cmd}"
    );
    assert!(
        !server_cmd.contains("--parallel 4"),
        "--set replaces the backend's own flag: {server_cmd}"
    );
    let run_id = started["run_id"].as_str().unwrap().to_string();
    let lines = follow(&r, &run_id).await;
    let res = call(&r, Op::ConsoleRunResult, json!({ "run_id": run_id }))
        .await
        .unwrap();
    let s = res["steps"].as_array().unwrap();
    assert_eq!(s[1]["verdict"], "expected_failure", "{}", text_of(&lines));
    assert_eq!(s[0]["verdict"], "expected_failure", "{:#}", s[0]);
    assert_eq!(
        s[0]["analysis"]["cuda_errors"][0]["error"],
        "invalid argument"
    );
    assert_eq!(
        s[0]["analysis"]["timings"]["last_progress"],
        json!([7680, 0.93])
    );
}

#[tokio::test]
async fn remote_and_underprivileged_callers_are_refused() {
    let r = rig("security");
    let remote = json!({ "kind": "api", "actor": "claude", "role": "admin", "source_ip": "10.0.0.5", "loopback": false });
    let e = call(
        &r,
        Op::ConsoleRunStart,
        json!({ "recipe": "nvidia-smi-snapshot", "caller": remote }),
    )
    .await;
    assert!(
        matches!(e, Err(manager_app::ApiError::Forbidden(_))),
        "{e:?}"
    );
    let e = call(
        &r,
        Op::ConsoleExec,
        json!({ "line": "stop victim", "caller": remote }),
    )
    .await;
    assert!(
        matches!(e, Err(manager_app::ApiError::Forbidden(_))),
        "{e:?}"
    );
    // Reads are fine from anywhere.
    let ok = call(
        &r,
        Op::ConsoleExec,
        json!({ "line": "recipes", "caller": remote }),
    )
    .await
    .unwrap();
    assert_eq!(ok["ok"], true);
    // Role short of the command's class.
    let viewer = json!({ "kind": "api", "actor": "v", "role": "viewer", "source_ip": "127.0.0.1", "loopback": true });
    let e = call(
        &r,
        Op::ConsoleExec,
        json!({ "line": "run nvidia-smi-snapshot", "caller": viewer }),
    )
    .await;
    assert!(
        matches!(e, Err(manager_app::ApiError::Forbidden(_))),
        "{e:?}"
    );
    // The raw shell is off for the API by default.
    let local_admin = json!({ "kind": "api", "actor": "a", "role": "admin", "source_ip": "127.0.0.1", "loopback": true });
    let e = call(
        &r,
        Op::ConsoleShellOpen,
        json!({ "session_id": "s1", "caller": local_admin }),
    )
    .await;
    assert!(
        matches!(e, Err(manager_app::ApiError::Forbidden(_))),
        "{e:?}"
    );
    // Settings cannot be changed over the API.
    let e = call(
        &r,
        Op::ConsoleSettingsSet,
        json!({ "allow_remote_exec": true, "caller": local_admin }),
    )
    .await;
    assert!(
        matches!(e, Err(manager_app::ApiError::Forbidden(_))),
        "{e:?}"
    );
    // A non-allowlisted executable is a plan error; nothing starts.
    let other = std::env::temp_dir().join(format!("llmdil-console-other-{}", std::process::id()));
    std::fs::create_dir_all(&other).unwrap();
    let rogue = other.join(exe_name("rogue"));
    std::fs::copy(FAKE, &rogue).unwrap();
    let res = call(
        &r,
        Op::ConsoleRunStart,
        json!({ "recipe_doc": { "name": "rogue", "steps": [{ "name": "x", "exe": rogue.to_string_lossy() }] } }),
    )
    .await
    .unwrap();
    assert_eq!(res["started"], false);
    assert!(
        res["errors"][0]
            .as_str()
            .unwrap()
            .contains("not on the executable allowlist"),
        "{res:#}"
    );
    let _ = std::fs::remove_dir_all(&other);
    let audit = std::fs::read_to_string(r.dir.join("logs").join("admin-api-audit.jsonl")).unwrap();
    assert!(audit.contains("refused: non-loopback"), "{audit}");
}
