//! Planning: a recipe resolved against this machine — templates
//! filled from LLMdil state, executables resolved (managed llama.cpp
//! install first, then PATH) and checked against the allowlist, the
//! GPUs the run needs inferred, and the running backends that must be
//! stopped for it worked out. Nothing here starts anything.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use manager_diag::gpus::{self, GpuRef};
use manager_diag::recipe::{Expect, ParserKind, Recipe, StepKind, DEFAULT_STEP_TIMEOUT_SECS};
use manager_diag::TemplateContext;
use manager_telemetry::GpuInfo;
use serde::Serialize;
use serde_json::{json, Value};

use crate::{ApiError, AppState};

/// What the machine looks like right now.
pub(crate) struct Machine {
    pub data_dir: PathBuf,
    pub llamacpp_dir: PathBuf,
    pub models_dir: PathBuf,
    pub gpus: Vec<GpuInfo>,
    pub cfg: manager_config::AppConfig,
    pub env: BTreeMap<String, String>,
}

pub(crate) async fn machine(state: &AppState) -> Machine {
    let cfg = state.config.read().await.clone();
    let gpus = {
        let store = state.telemetry().await;
        let inv = crate::lock_telemetry(&store).inventory();
        inv
    };
    Machine {
        data_dir: cfg.data_dir.clone(),
        llamacpp_dir: cfg.data_dir.join("llamacpp"),
        models_dir: cfg.data_dir.join("models"),
        gpus,
        env: std::env::vars().collect(),
        cfg,
    }
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// The template context: install paths, every backend's model path,
/// flags, port, exact server argv and GPU visibility, GPU names and
/// UUIDs, and the process environment.
pub(crate) fn context(m: &Machine) -> TemplateContext {
    let mut c = TemplateContext::new();
    c.set("llamacpp_bin", path_str(&m.llamacpp_dir));
    c.set("data_dir", path_str(&m.data_dir));
    c.set("models_dir", path_str(&m.models_dir));
    c.set(
        "diagnostics_dir",
        path_str(&super::store::diag_dir(&m.data_dir)),
    );
    for g in &m.gpus {
        c.set(format!("gpu.{}.name", g.index), g.name.clone());
        c.set(format!("gpu.{}.uuid", g.index), g.uuid.clone());
    }
    for b in &m.cfg.backends {
        let id = &b.id;
        let spec = crate::backend_spec(&m.cfg, b, &m.gpus);
        let model = if b.model_file.is_absolute() {
            b.model_file.clone()
        } else {
            m.data_dir.join(&b.model_file)
        };
        c.set(format!("backend.{id}.id"), id.clone());
        c.set(format!("backend.{id}.model_path"), path_str(&model));
        c.set(format!("backend.{id}.port"), b.port.to_string());
        c.set(format!("backend.{id}.n_ctx"), b.flags.n_ctx.to_string());
        c.set(
            format!("backend.{id}.url"),
            format!("http://127.0.0.1:{}", b.port),
        );
        let raw: Vec<String> = b
            .raw_flags
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .map(str::to_string)
            .collect();
        c.set_list(format!("backend.{id}.raw_flags"), raw);
        // The exact llama-server argv the supervisor launches, minus
        // `--port N` (a recipe picks its own port).
        let mut server_args = Vec::new();
        let mut it = spec.args.iter().peekable();
        while let Some(a) = it.next() {
            if a == "--port" {
                it.next();
                continue;
            }
            server_args.push(a.clone());
        }
        // llama-bench spelling of the same flags (minus -m: recipes
        // pass the model themselves).
        let mut for_bench = Vec::new();
        let mut it = server_args.iter();
        while let Some(a) = it.next() {
            if a == "-m" || a == "--model" {
                it.next();
                continue;
            }
            for_bench.push(a.clone());
        }
        let (bench_flags, _dropped) = manager_diag::builtins::server_flags_to_bench(&for_bench);
        c.set_list(format!("backend.{id}.server_args"), server_args);
        c.set_list(format!("backend.{id}.bench_flags"), bench_flags);
        let cvd = spec
            .env
            .iter()
            .find(|(k, _)| k == "CUDA_VISIBLE_DEVICES")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        c.set(format!("backend.{id}.cuda_visible_devices"), cvd);
    }
    c.env = m.env.clone();
    c
}

/// What the paste importer needs to know about this machine.
pub(crate) async fn import_context(state: &AppState) -> manager_diag::paste::ImportContext {
    let m = machine(state).await;
    let managed_tools: Vec<String> = std::fs::read_dir(&m.llamacpp_dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| {
                    let l = n.to_ascii_lowercase();
                    l.ends_with(".exe") || !l.contains('.')
                })
                .collect()
        })
        .unwrap_or_default();
    let mut path_tools = Vec::new();
    for t in ["nvidia-smi", "pwsh", "python", "curl"] {
        if which(t, &m.env).is_some() {
            path_tools.push(t.to_string());
            path_tools.push(format!("{t}.exe"));
        }
    }
    manager_diag::paste::ImportContext {
        data_dir: path_str(&m.data_dir),
        models_dir: path_str(&m.models_dir),
        llamacpp_dir: path_str(&m.llamacpp_dir),
        managed_tools,
        env: m.env.clone(),
        path_tools,
    }
}

// ---------------------------------------------------------------------------
// Executables
// ---------------------------------------------------------------------------

/// Script and shell extensions recipe steps never run: they execute
/// through an interpreter, i.e. a shell string.
const REFUSED_EXTENSIONS: &[&str] = &[
    "bat", "cmd", "ps1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "msc", "sh",
];

fn env_get<'a>(env: &'a BTreeMap<String, String>, k: &str) -> Option<&'a String> {
    env.iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(k))
        .map(|(_, v)| v)
}

/// Resolve a program name through PATH (`.exe` appended on Windows
/// when the name has no extension).
pub(crate) fn which(name: &str, env: &BTreeMap<String, String>) -> Option<PathBuf> {
    let path = env_get(env, "PATH")?;
    let sep = if cfg!(windows) { ';' } else { ':' };
    for dir in path.split(sep).filter(|d| !d.trim().is_empty()) {
        let dir = PathBuf::from(dir.trim().trim_matches('"'));
        for cand in candidates(&dir.join(name)) {
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

fn candidates(p: &Path) -> Vec<PathBuf> {
    if cfg!(windows) && p.extension().is_none() {
        let mut exe = p.as_os_str().to_owned();
        exe.push(".exe");
        vec![PathBuf::from(exe), p.to_path_buf()]
    } else {
        vec![p.to_path_buf()]
    }
}

fn normalize_seps(s: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(s.replace('/', "\\"))
    } else {
        PathBuf::from(s)
    }
}

/// Resolve a step executable: an absolute path as given; a relative
/// path against the step's working dir (else the llama.cpp dir); a
/// bare name in the managed llama.cpp install first, then PATH.
pub(crate) fn resolve_exe(
    raw: &str,
    m: &Machine,
    workdir: Option<&Path>,
) -> Result<PathBuf, String> {
    let p = normalize_seps(raw.trim());
    if p.as_os_str().is_empty() {
        return Err("empty executable".into());
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!("{raw}: `..` is not allowed in an executable path"));
    }
    let found = if p.is_absolute() {
        candidates(&p).into_iter().find(|c| c.is_file())
    } else if p.components().count() > 1 {
        let base = workdir
            .map(Path::to_path_buf)
            .unwrap_or_else(|| m.llamacpp_dir.clone());
        candidates(&base.join(&p)).into_iter().find(|c| c.is_file())
    } else {
        candidates(&m.llamacpp_dir.join(&p))
            .into_iter()
            .find(|c| c.is_file())
            .or_else(|| which(raw.trim(), &m.env))
    };
    let Some(exe) = found else {
        return Err(format!(
            "{raw}: not found (looked in the managed llama.cpp install {} and on PATH)",
            m.llamacpp_dir.display()
        ));
    };
    if let Some(ext) = exe.extension().and_then(|e| e.to_str()) {
        if REFUSED_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()) {
            return Err(format!(
                "{}: .{ext} files run through a script interpreter; recipe steps never run through a shell",
                exe.display()
            ));
        }
    }
    Ok(exe)
}

fn norm_cmp(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/').to_string();
    if cfg!(windows) {
        s.to_ascii_lowercase()
    } else {
        s
    }
}

fn glob_match(pattern: &str, name: &str) -> bool {
    let (p, n) = if cfg!(windows) {
        (pattern.to_ascii_lowercase(), name.to_ascii_lowercase())
    } else {
        (pattern.to_string(), name.to_string())
    };
    match p.split_once('*') {
        None => p == n,
        Some((pre, post)) => {
            n.len() >= pre.len() + post.len() && n.starts_with(pre) && n.ends_with(post)
        }
    }
}

/// Is `exe` on the allowlist? An entry with a directory part matches
/// files in that directory by name (a `*` glob allowed); a bare name
/// matches exactly what PATH resolves it to.
pub(crate) fn allowlisted(
    exe: &Path,
    entries: &[String],
    ctx: &TemplateContext,
    env: &BTreeMap<String, String>,
) -> Option<String> {
    let exe_dir = exe.parent().map(norm_cmp).unwrap_or_default();
    let exe_name = exe
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    for e in entries {
        let Ok(resolved) = ctx.resolve_str(e) else {
            continue;
        };
        if resolved.contains('/') || resolved.contains('\\') {
            let rp = normalize_seps(&resolved);
            let (dir, pat) = match (rp.parent(), rp.file_name()) {
                (Some(d), Some(f)) => (norm_cmp(d), f.to_string_lossy().into_owned()),
                _ => continue,
            };
            if dir == exe_dir && glob_match(&pat, &exe_name) {
                return Some(e.clone());
            }
        } else if let Some(w) = which(&resolved, env) {
            if norm_cmp(&w) == norm_cmp(exe) {
                return Some(e.clone());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The plan
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ResolvedHttp {
    pub url: String,
    pub method: String,
    pub body: Option<Value>,
    pub synthetic_prompt_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ResolvedGguf {
    pub path: String,
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ResolvedStep {
    /// Position in the run (0-based).
    pub index: usize,
    pub step_index: usize,
    pub variant_index: Option<usize>,
    pub label: String,
    pub variant_name: Option<String>,
    pub description: String,
    pub kind: StepKind,
    pub exe: Option<String>,
    pub args: Vec<String>,
    /// Environment the step sets (recipe + step + variant), resolved.
    /// A variable that resolves to empty is not set (an empty
    /// `CUDA_VISIBLE_DEVICES` would hide every GPU).
    pub env: BTreeMap<String, String>,
    pub workdir: Option<String>,
    pub timeout_secs: u64,
    pub parser: Option<ParserKind>,
    pub expect: Expect,
    pub background: bool,
    pub ready_url: Option<String>,
    pub ready_timeout_secs: u64,
    pub http: Option<ResolvedHttp>,
    pub gguf: Option<ResolvedGguf>,
    /// Display form of the command (argv quoted for reading; it is
    /// never executed as a string).
    pub command_line: String,
    pub gpus: Vec<u32>,
    pub gpu_reasons: Vec<String>,
    pub allowlist_entry: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StopCandidate {
    pub id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub recipe: Recipe,
    pub variant: Option<usize>,
    pub params: BTreeMap<String, String>,
    pub set: BTreeMap<String, String>,
    pub(crate) steps: Vec<ResolvedStep>,
    pub gpus: Vec<u32>,
    pub gpu_reasons: Vec<String>,
    pub declared_gpus: Option<Vec<u32>>,
    pub(crate) stop: Vec<StopCandidate>,
    pub keep_running: Vec<String>,
    pub restore: bool,
    pub warnings: Vec<String>,
    /// Blocking problems (unresolved templates, missing or
    /// non-allowlisted executables): a run refuses while any exist.
    pub errors: Vec<String>,
}

impl Plan {
    /// The review document (UI and API).
    pub fn to_review_json(&self) -> Value {
        let gpu_names: Vec<String> = self.gpus.iter().map(|g| format!("GPU {g}")).collect();
        json!({
            "recipe": self.recipe,
            "variant": self.variant,
            "params": self.params,
            "set": self.set,
            "steps": self.steps,
            "gpus": self.gpus,
            "gpu_reasons": self.gpu_reasons,
            "declared_gpus": self.declared_gpus,
            "stop_backends": self.stop,
            "keep_running": self.keep_running,
            "restore": self.restore,
            "warnings": self.warnings,
            "errors": self.errors,
            "runnable": self.errors.is_empty(),
            "summary": format!(
                "{} execution(s); needs {}; stops {}",
                self.steps.len(),
                if gpu_names.is_empty() { "no GPU".to_string() } else { gpu_names.join(", ") },
                if self.stop.is_empty() {
                    "nothing".to_string()
                } else {
                    self.stop.iter().map(|s| s.id.clone()).collect::<Vec<_>>().join(", ")
                }
            ),
        })
    }
}

/// Quote an argv for display only.
pub(crate) fn display_command(exe: &str, args: &[String]) -> String {
    let q = |s: &str| {
        if s.is_empty() || s.contains(char::is_whitespace) || s.contains('"') {
            format!("\"{}\"", s.replace('"', "\\\""))
        } else {
            s.to_string()
        }
    };
    std::iter::once(q(exe))
        .chain(args.iter().map(|a| q(a)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn resolve_env(
    ctx: &TemplateContext,
    env: &BTreeMap<String, String>,
    errors: &mut Vec<String>,
    at: &str,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (k, v) in env {
        match ctx.resolve_str(v) {
            Ok(r) if r.is_empty() => {}
            Ok(r) => {
                out.insert(k.clone(), r);
            }
            Err(e) => errors.push(format!("{at}: env {k}: {e}")),
        }
    }
    out
}

/// Resolve `recipe` against this machine.
pub(crate) async fn build_plan(
    state: &AppState,
    recipe: &Recipe,
    variant: Option<usize>,
    params: &BTreeMap<String, String>,
    set: &BTreeMap<String, String>,
) -> Result<Plan, ApiError> {
    recipe
        .validate()
        .map_err(|e| ApiError::InvalidInput(e.to_string()))?;
    for k in set.keys() {
        if !manager_diag::argv::is_flag(k) {
            return Err(ApiError::InvalidInput(format!(
                "--set {k:?}: overrides name flags (e.g. --set --parallel=1)"
            )));
        }
    }
    let m = machine(state).await;
    let base = context(&m);
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let ctx = match base.with_params(&recipe.params, params) {
        Ok(c) => c,
        Err(e) => {
            errors.push(e.to_string());
            base.clone()
        }
    };
    let resolved_params: BTreeMap<String, String> = recipe
        .params
        .iter()
        .filter_map(|p| ctx.vars.get(&p.name).map(|v| (p.name.clone(), v.clone())))
        .chain(params.iter().map(|(k, v)| {
            (
                k.clone(),
                ctx.vars.get(k).cloned().unwrap_or_else(|| v.clone()),
            )
        }))
        .collect();
    let planned = recipe
        .expand(variant)
        .map_err(|e| ApiError::InvalidInput(e.to_string()))?;
    let inventory: Vec<GpuRef> = m
        .gpus
        .iter()
        .map(|g| GpuRef {
            index: g.index,
            uuid: g.uuid.clone(),
        })
        .collect();
    let allowlist = m.cfg.diagnostics.exec_allowlist.clone();
    let mut steps = Vec::new();
    let mut all_gpus: BTreeSet<u32> = BTreeSet::new();
    let mut gpu_reasons = Vec::new();
    for (i, p) in planned.iter().enumerate() {
        let s = &recipe.steps[p.step_index];
        let at = format!("step {} ({})", i + 1, p.label);
        let mut args = match ctx.resolve_args(&p.args) {
            Ok(a) => a,
            Err(e) => {
                errors.push(format!("{at}: {e}"));
                Vec::new()
            }
        };
        // Run-level `--set` overrides apply to the resolved argv, so
        // they replace a flag that came in through a list template
        // (`{backend.server_args}`) instead of duplicating it.
        if s.kind == StepKind::Exec && !set.is_empty() {
            let mut resolved_set = BTreeMap::new();
            for (k, v) in set {
                match ctx.resolve_str(v) {
                    Ok(r) => {
                        resolved_set.insert(k.clone(), r);
                    }
                    Err(e) => errors.push(format!("{at}: --set {k}: {e}")),
                }
            }
            args = manager_diag::argv::apply_overrides(&args, &resolved_set, &[]);
        }
        let env = resolve_env(&ctx, &p.env, &mut errors, &at);
        let workdir = match s.workdir.as_deref().map(|w| ctx.resolve_str(w)) {
            Some(Ok(w)) => Some(w),
            Some(Err(e)) => {
                errors.push(format!("{at}: workdir: {e}"));
                None
            }
            None => None,
        };
        let res = |s: &str, errors: &mut Vec<String>| match ctx.resolve_str(s) {
            Ok(v) => v,
            Err(e) => {
                errors.push(format!("{at}: {e}"));
                String::new()
            }
        };
        let mut exe = None;
        let mut allowlist_entry = None;
        let mut need = manager_diag::gpus::GpuNeed::default();
        let mut http = None;
        let mut gguf = None;
        let command_line;
        match s.kind {
            StepKind::Exec => {
                let raw = res(&s.exe, &mut errors);
                match resolve_exe(&raw, &m, workdir.as_deref().map(Path::new)) {
                    Ok(path) => {
                        allowlist_entry = allowlisted(&path, &allowlist, &ctx, &m.env);
                        if allowlist_entry.is_none() {
                            errors.push(format!(
                                "{at}: {} is not on the executable allowlist (Settings → Debug console; entries: {})",
                                path.display(),
                                allowlist.join(", ")
                            ));
                        }
                        // llama-bench: structured output when allowed.
                        let fname = path
                            .file_name()
                            .map(|f| f.to_string_lossy().to_ascii_lowercase())
                            .unwrap_or_default();
                        let parser = s.parser.or_else(|| {
                            fname
                                .contains("llama-bench")
                                .then_some(ParserKind::LlamaBench)
                        });
                        if parser == Some(ParserKind::LlamaBench)
                            && s.bench_json
                            && fname.contains("llama-bench")
                            && !manager_diag::argv::has_flag(&args, &["-o", "--output"])
                        {
                            args.push("-o".into());
                            args.push("json".into());
                        }
                        need = gpus::infer(&path_str(&path), &args, &env, &inventory);
                        exe = Some(path_str(&path));
                    }
                    Err(e) => errors.push(format!("{at}: {e}")),
                }
                command_line = display_command(exe.as_deref().unwrap_or(&raw), &args);
            }
            StepKind::Http => {
                let h = s.http.as_ref().expect("validated");
                let url = res(&h.url, &mut errors);
                if !url.is_empty() && !is_loopback_url(&url) {
                    errors.push(format!(
                        "{at}: http steps may only address loopback servers, not {url}"
                    ));
                }
                // `{synthetic_prompt}` stays a placeholder until the
                // runner generates the text.
                let mut body_ctx = ctx.clone();
                body_ctx.set("synthetic_prompt", super::runner::SYNTHETIC_SENTINEL);
                let body = match h.body.as_ref().map(|b| body_ctx.resolve_json(b)) {
                    Some(Ok(b)) => Some(b),
                    Some(Err(e)) => {
                        errors.push(format!("{at}: body: {e}"));
                        None
                    }
                    None => None,
                };
                let tokens = h
                    .synthetic_prompt_tokens
                    .as_deref()
                    .map(|t| res(t, &mut errors))
                    .and_then(|t| match t.trim().parse::<u32>() {
                        Ok(n) if n <= 262_144 => Some(n),
                        _ => {
                            errors.push(format!(
                                "{at}: synthetic_prompt_tokens {t:?} is not a number up to 262144"
                            ));
                            None
                        }
                    });
                command_line = format!("{} {url}", h.method.to_ascii_uppercase());
                http = Some(ResolvedHttp {
                    url,
                    method: h.method.to_ascii_uppercase(),
                    body,
                    synthetic_prompt_tokens: tokens,
                });
            }
            StepKind::Gguf => {
                let g = s.gguf.as_ref().expect("validated");
                let path = res(&g.path, &mut errors);
                command_line = format!("gguf-metadata {path}");
                gguf = Some(ResolvedGguf {
                    path,
                    keys: g.keys.clone(),
                });
            }
        }
        for r in &need.reasons {
            gpu_reasons.push(format!("{}: {r}", p.label));
        }
        all_gpus.extend(need.gpus.iter().copied());
        let description = ctx
            .resolve_str(&p.description)
            .unwrap_or_else(|_| p.description.clone());
        steps.push(ResolvedStep {
            index: i,
            step_index: p.step_index,
            variant_index: p.variant_index,
            label: p.label.clone(),
            variant_name: p.variant_name.clone(),
            description,
            kind: s.kind,
            exe,
            args,
            env,
            workdir,
            timeout_secs: s.timeout_secs.unwrap_or(DEFAULT_STEP_TIMEOUT_SECS),
            parser: s.parser.or_else(|| {
                (s.kind == StepKind::Exec && s.exe.to_ascii_lowercase().contains("llama-bench"))
                    .then_some(ParserKind::LlamaBench)
            }),
            expect: p.expect,
            background: s.background,
            ready_url: match s.ready_url.as_deref().map(|u| ctx.resolve_str(u)) {
                Some(Ok(u)) => {
                    if !is_loopback_url(&u) {
                        errors.push(format!("{at}: ready_url must be a loopback URL"));
                    }
                    Some(u)
                }
                Some(Err(e)) => {
                    errors.push(format!("{at}: ready_url: {e}"));
                    None
                }
                None => None,
            },
            ready_timeout_secs: s.ready_timeout_secs.unwrap_or(600),
            http,
            gguf,
            command_line,
            gpus: need.gpus.iter().copied().collect(),
            gpu_reasons: need.reasons.clone(),
            allowlist_entry,
        });
    }
    let declared = recipe.requires.gpus.clone();
    if let Some(d) = &declared {
        let inferred: Vec<u32> = all_gpus.iter().copied().collect();
        if &inferred != d && !inferred.is_empty() {
            warnings.push(format!(
                "declared GPUs {d:?} differ from the inferred {inferred:?}; using the declared set"
            ));
        }
        all_gpus = d.iter().copied().collect();
    }
    // Backends: explicit stops, plus every live backend sharing a GPU
    // with the run (the bench interlock's placement judgment).
    let keep_running: Vec<String> = recipe
        .requires
        .keep_running
        .iter()
        .filter_map(|k| ctx.resolve_str(k).ok())
        .collect();
    let live: BTreeMap<String, bool> = crate::lifecycle::backend_liveness(state)
        .await
        .into_values()
        .map(|l| (l.id.clone(), l.state.is_live()))
        .collect();
    let mut stop: Vec<StopCandidate> = Vec::new();
    for raw in &recipe.requires.stop_backends {
        match ctx.resolve_str(raw) {
            Ok(id) => {
                if !m.cfg.backends.iter().any(|b| b.id == id) {
                    errors.push(format!("requires.stop_backends: no backend {id:?}"));
                } else if live.get(&id).copied().unwrap_or(false)
                    && !stop.iter().any(|s| s.id == id)
                {
                    stop.push(StopCandidate {
                        id,
                        reason: "named by the recipe".into(),
                    });
                }
            }
            Err(e) => errors.push(format!("requires.stop_backends: {e}")),
        }
    }
    for b in &m.cfg.backends {
        if !live.get(&b.id).copied().unwrap_or(false)
            || stop.iter().any(|s| s.id == b.id)
            || keep_running.contains(&b.id)
            || all_gpus.is_empty()
        {
            continue;
        }
        let reason = match crate::bench::bench_placement(b) {
            crate::bench::BenchPlacement::Gpus(set) => {
                let shared: Vec<u32> = set.intersection(&all_gpus).copied().collect();
                if shared.is_empty() {
                    continue;
                }
                format!(
                    "running on {}",
                    shared
                        .iter()
                        .map(|g| format!("GPU {g}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            crate::bench::BenchPlacement::CpuOnly => continue,
            crate::bench::BenchPlacement::Undetermined => {
                "placement undetermined (raw device flags); assumed to share the GPUs".into()
            }
        };
        stop.push(StopCandidate {
            id: b.id.clone(),
            reason,
        });
    }
    if m.gpus.is_empty() && !all_gpus.is_empty() {
        warnings.push("no GPU inventory from telemetry yet; GPU indices are taken from the command lines alone".into());
    }
    Ok(Plan {
        recipe: recipe.clone(),
        variant,
        params: resolved_params,
        set: set.clone(),
        steps,
        gpus: all_gpus.into_iter().collect(),
        gpu_reasons,
        declared_gpus: declared,
        stop,
        keep_running,
        restore: recipe.requires.restore,
        warnings,
        errors,
    })
}

/// `http://127.0.0.1:…`, `http://localhost:…`, `http://[::1]:…`.
pub(crate) fn is_loopback_url(u: &str) -> bool {
    let rest = u
        .strip_prefix("http://")
        .or_else(|| u.strip_prefix("https://"));
    let Some(rest) = rest else { return false };
    let host = rest.split(['/', '?']).next().unwrap_or("");
    let host = if let Some(h) = host.strip_prefix('[') {
        h.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host)
    };
    host == "127.0.0.1" || host.eq_ignore_ascii_case("localhost") || host == "::1"
}
