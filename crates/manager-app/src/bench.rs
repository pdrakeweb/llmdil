//! Bench (Bench design, approved Oct 23, 2026): server-driven
//! configuration sweeps against temporary `bench-<n>` backends,
//! a prompt library with shipped defaults, planner-generated
//! prompts through the consultation transport, and a standalone
//! results store. The pure logic (sweep types, expansion and
//! validation pruning, comparability keys, spec log parsing,
//! presets, winner computation) lives in the `manager-bench`
//! crate; this module is the app glue: ops, stores, the runner,
//! the GPU interlock, and per-arm preflight.
//!
//! Standing rules from the design that this module enforces:
//! - Bench backends are supervisor-registered temporaries on the
//!   experiment-runner pattern: never in `config.json`, never in
//!   gateway groups, loopback only, guaranteed cleanup, and a
//!   startup sweep for crash leftovers (§6.4, D5).
//! - A sweep starts only when no backend sharing a GPU (or the
//!   CPU-offload resource) with the target is running; stopping
//!   a conflicting backend takes an explicit operator-confirmed
//!   list, and Bench never restarts what it stopped (D6, §6.1).
//! - The 20-minute sweep figure is a TARGET (D14 as reconciled
//!   Oct 23, 2026): the projection warns, nothing refuses and
//!   nothing stops a sweep for exceeding it. Durations are
//!   recorded and reported. The operator's stop is cancellation
//!   (D13), which is real: a scoped handle owned by the sweep,
//!   not the task bookkeeping.
//! - Results live in their own store and never feed Optimize
//!   (D10): nothing here writes the measurement store or any
//!   Optimize state.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use manager_bench as mb;
use manager_config::{AppConfig, BackendConfig};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{ApiError, AppState, TaskStatus};

// ---------------------------------------------------------------------------
// Shared job state (AppState fields)
// ---------------------------------------------------------------------------

/// Live progress of the running sweep, surfaced through
/// `bench_results_list` (the results payload carries it on the
/// running sweep's record).
#[derive(Debug, Clone, Serialize)]
pub struct BenchProgress {
    pub arm_index: usize,
    pub arm_count: usize,
    pub phase: String,
    pub run: usize,
    pub started_unix_ms: i64,
}

/// The one running sweep (Bench runs one at a time, §6.4).
#[derive(Debug, Clone)]
pub struct BenchJob {
    pub sweep_id: String,
    pub progress: BenchProgress,
}

// ---------------------------------------------------------------------------
// Paths and stores
// ---------------------------------------------------------------------------

fn prompts_path(state: &AppState) -> PathBuf {
    state.data_dir.join("bench-prompts.json")
}

fn results_path(state: &AppState) -> PathBuf {
    state.data_dir.join("bench-results.json")
}

fn journal_path(state: &AppState) -> PathBuf {
    state.data_dir.join("bench-journal.json")
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Load the prompt library, materializing the shipped defaults
/// (first read creates the file; unedited shipped entries
/// refresh when the app ships newer versions, §4.1).
fn load_prompts(state: &AppState) -> mb::PromptLibrary {
    let mut lib = mb::PromptLibrary::load(&prompts_path(state)).unwrap_or_default();
    if lib.materialize_shipped() {
        if let Err(e) = lib.save(&prompts_path(state)) {
            eprintln!("bench: prompt library save failed: {e}");
        }
    }
    lib
}

fn load_results(state: &AppState) -> mb::BenchResults {
    mb::BenchResults::load(&results_path(state)).unwrap_or_default()
}

fn save_results(state: &AppState, results: &mb::BenchResults) {
    if let Err(e) = results.save(&results_path(state)) {
        eprintln!("bench: results save failed: {e}");
    }
}

fn prompts_payload(lib: &mb::PromptLibrary) -> Value {
    json!({ "prompts": lib.entries })
}

fn results_payload(state: &AppState, results: &mb::BenchResults) -> Value {
    let job = state
        .bench_job
        .try_lock()
        .ok()
        .and_then(|guard| (*guard).clone());
    let sweeps: Vec<Value> = results
        .sweeps
        .iter()
        .map(|s| {
            let mut v = serde_json::to_value(s).unwrap_or(Value::Null);
            if let (Some(job), Some(obj)) = (&job, v.as_object_mut()) {
                if obj.get("id").and_then(|i| i.as_str()) == Some(job.sweep_id.as_str())
                    && obj.get("status").and_then(|s| s.as_str()) == Some("running")
                {
                    obj.insert(
                        "progress".to_string(),
                        serde_json::to_value(&job.progress).unwrap_or(Value::Null),
                    );
                }
            }
            v
        })
        .collect();
    json!({ "sweeps": sweeps })
}

// ---------------------------------------------------------------------------
// Journal (<data_dir>/bench-journal.json) — crash-leftover record
// for the startup sweep, on the experiment journal's pattern.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BenchJournal {
    #[serde(default)]
    active: Vec<ActiveBench>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ActiveBench {
    temp_id: String,
    #[serde(default)]
    pid: Option<u32>,
    port: u16,
    sweep_id: String,
    started_at: String,
}

impl BenchJournal {
    fn load(path: &PathBuf) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self, path: &PathBuf) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| e.to_string())
    }

    fn add_active(&mut self, rec: ActiveBench) {
        self.active.retain(|a| a.temp_id != rec.temp_id);
        self.active.push(rec);
    }

    fn set_active_pid(&mut self, temp_id: &str, pid: u32) {
        if let Some(a) = self.active.iter_mut().find(|a| a.temp_id == temp_id) {
            a.pid = Some(pid);
        }
    }

    fn remove_active(&mut self, temp_id: &str) -> bool {
        let before = self.active.len();
        self.active.retain(|a| a.temp_id != temp_id);
        self.active.len() != before
    }
}

fn load_journal(state: &AppState) -> BenchJournal {
    BenchJournal::load(&journal_path(state))
}

fn save_journal(state: &AppState, journal: &BenchJournal) {
    if let Err(e) = journal.save(&journal_path(state)) {
        eprintln!("bench: journal save failed: {e}");
    }
}

/// Is this supervisor/config id one of Bench's temporaries?
fn is_bench_temp_id(id: &str) -> bool {
    id.starts_with("bench-")
}

// ---------------------------------------------------------------------------
// Startup sweep: remove leftovers of sweeps interrupted by an
// app crash (design §6.4, experiment §A.3 step 5 pattern).
// ---------------------------------------------------------------------------

pub async fn startup_sweep(state: &AppState) {
    // Defensive config scrub: bench temporaries are never
    // written to the config, but a leftover entry must never be
    // started or routed as a real backend.
    {
        let cfg = state.config.read().await.clone();
        if cfg.backends.iter().any(|b| is_bench_temp_id(&b.id)) {
            let mut new_cfg = cfg.clone();
            let removed: Vec<String> = new_cfg
                .backends
                .iter()
                .filter(|b| is_bench_temp_id(&b.id))
                .map(|b| b.id.clone())
                .collect();
            new_cfg.backends.retain(|b| !is_bench_temp_id(&b.id));
            for g in new_cfg.gateway.groups.iter_mut() {
                g.members.retain(|m| !removed.contains(m));
            }
            for m in new_cfg.models.iter_mut() {
                m.assigned_backends.retain(|b| !removed.contains(b));
            }
            if manager_config::save(&new_cfg, &state.config_path).is_ok() {
                *state.config.write().await = new_cfg;
                eprintln!(
                    "bench: startup sweep removed leftover temp backends from config: {}",
                    removed.join(", ")
                );
            }
        }
    }
    let mut journal = load_journal(state);
    let active = std::mem::take(&mut journal.active);
    for record in &active {
        if let Some(pid) = record.pid {
            let alive = manager_supervisor::pid_alive(pid);
            let image = manager_supervisor::process_image_is(pid, "llama-server.exe")
                || manager_supervisor::process_image_is(pid, "llama-server");
            if alive && image {
                match manager_wizard::ops::kill_process(pid) {
                    Ok(()) => eprintln!(
                        "bench: startup sweep terminated leftover {} (pid {pid})",
                        record.temp_id
                    ),
                    Err(e) => eprintln!(
                        "bench: startup sweep could not terminate {} (pid {pid}): {e}",
                        record.temp_id
                    ),
                }
            }
        }
        let mut sup = state.supervisor().await.lock().await;
        sup.reap_exited();
        if sup.spec(&record.temp_id).is_some() {
            let _ = sup.stop(&record.temp_id, false).await;
            sup.unregister_spec(&record.temp_id);
        }
    }
    if !active.is_empty() {
        save_journal(state, &journal);
    }
    // A sweep record left "running" by the crash is partial by
    // definition; its measured arms are kept (D13's shape).
    let mut results = load_results(state);
    let mut changed = false;
    for sweep in results.sweeps.iter_mut() {
        if sweep.status == "running" {
            sweep.status = "partial".to_string();
            sweep.partial = true;
            sweep.ended_ms = Some(now_ms());
            for arm in sweep.arms.iter_mut() {
                if arm.status == mb::ArmStatus::Running || arm.status == mb::ArmStatus::Pending {
                    arm.status = mb::ArmStatus::Cancelled;
                    arm.detail = Some(
                        "sweep interrupted by an app restart; this arm never completed"
                            .to_string(),
                    );
                }
            }
            changed = true;
        }
    }
    if changed {
        save_results(state, &results);
    }
}

// ---------------------------------------------------------------------------
// Prompt library ops (§7)
// ---------------------------------------------------------------------------

pub async fn op_bench_prompts_list(
    state: &Arc<AppState>,
    _sink: &Arc<dyn crate::EventSink>,
) -> crate::ApiResult {
    let lib = load_prompts(state);
    Ok(prompts_payload(&lib))
}

pub async fn op_bench_prompt_save(
    state: &Arc<AppState>,
    _sink: &Arc<dyn crate::EventSink>,
    entry: Value,
) -> crate::ApiResult {
    let mut entry: mb::PromptEntry = serde_json::from_value(entry)
        .map_err(|e| ApiError::InvalidInput(format!("invalid prompt entry: {e}")))?;
    if entry.name.trim().is_empty() {
        return Err(ApiError::InvalidInput("a prompt needs a name".to_string()));
    }
    if entry.body.trim().is_empty() {
        return Err(ApiError::InvalidInput("a prompt needs a body".to_string()));
    }
    let mut lib = load_prompts(state);
    if entry.id.trim().is_empty() {
        entry.id = unique_prompt_id(&lib, &entry.name);
    }
    // A shipped entry whose text changed is the operator's from
    // here on (§4.1), whatever the client claimed: the shipped
    // original stays restorable from the compiled-in defaults.
    if let Some(existing) = lib.entries.iter().find(|e| e.id == entry.id) {
        if existing.origin == mb::PromptOrigin::Shipped
            && existing.body != entry.body
            && entry.origin == mb::PromptOrigin::Shipped
        {
            entry.origin = mb::PromptOrigin::User;
        }
    }
    entry.sha256 = mb::prompt_sha256(&entry.body);
    entry.updated_ms = now_ms();
    lib.upsert(entry);
    lib.save(&prompts_path(state))
        .map_err(|e| ApiError::InvalidInput(format!("prompt save failed: {e}")))?;
    Ok(prompts_payload(&lib))
}

pub async fn op_bench_prompt_delete(
    state: &Arc<AppState>,
    _sink: &Arc<dyn crate::EventSink>,
    id: String,
) -> crate::ApiResult {
    let mut lib = load_prompts(state);
    if !lib.remove(&id) {
        return Err(ApiError::NotFound(format!("no prompt '{id}'")));
    }
    // Deleting a shipped entry restores its shipped text on the
    // next materialization (§4.1) — that IS the restore path.
    lib.materialize_shipped();
    lib.save(&prompts_path(state))
        .map_err(|e| ApiError::InvalidInput(format!("prompt save failed: {e}")))?;
    Ok(prompts_payload(&lib))
}

fn unique_prompt_id(lib: &mb::PromptLibrary, name: &str) -> String {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let base = if slug.is_empty() { "prompt".to_string() } else { slug };
    if lib.entries.iter().all(|e| e.id != base) {
        return base;
    }
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if lib.entries.iter().all(|e| e.id != candidate) {
            return candidate;
        }
    }
    unreachable!()
}

// ---------------------------------------------------------------------------
// Prompt generation (§4.3): the consultation transport aimed at
// the best running backend; the draft lands UNSAVED in the
// editor. Never starts a backend; never auto-promotes.
// ---------------------------------------------------------------------------

const GENERATE_TEMPERATURE: f32 = 0.7;
const GENERATE_TIMEOUT: Duration = Duration::from_secs(900);

pub async fn op_bench_prompt_generate(
    state: &Arc<AppState>,
    _sink: &Arc<dyn crate::EventSink>,
    purpose: String,
    description: String,
    size: String,
    backend_id: Option<String>,
) -> crate::ApiResult {
    let (target_tokens, max_tokens) = match size.as_str() {
        "short" => (500u32, 1_200u32),
        "standard" => (4_000u32, 7_000u32),
        other => {
            return Err(ApiError::InvalidInput(format!(
                "unknown prompt size '{other}' (short | standard)"
            )))
        }
    };
    if description.trim().is_empty() {
        return Err(ApiError::InvalidInput(
            "describe the prompt to generate".to_string(),
        ));
    }
    let cfg = state.config.read().await.clone();
    // Running set from the supervisor, exactly as the interlock
    // reads it.
    let running: HashSet<String> = {
        let mut sup = state.supervisor().await.lock().await;
        sup.reap_exited();
        sup.status()
            .into_iter()
            .filter(|s| s.pid.is_some())
            .map(|s| s.id)
            .collect()
    };
    let backend = match &backend_id {
        Some(id) => {
            let b = cfg
                .backends
                .iter()
                .find(|b| &b.id == id)
                .ok_or_else(|| ApiError::NotFound(format!("no backend '{id}'")))?;
            if !running.contains(id) {
                return Err(ApiError::InvalidInput(format!(
                    "backend '{id}' is not running; generation never starts a backend"
                )));
            }
            b.clone()
        }
        None => {
            // Best running backend by rated context (the
            // consultation's pick, §4.3).
            let mut candidates: Vec<&BackendConfig> = cfg
                .backends
                .iter()
                .filter(|b| running.contains(&b.id))
                .collect();
            candidates.sort_by_key(|b| std::cmp::Reverse(b.flags.n_ctx));
            candidates
                .first()
                .copied()
                .cloned()
                .ok_or_else(|| {
                    ApiError::InvalidInput(
                        "no backend is running; generation never starts a backend".to_string(),
                    )
                })?
        }
    };
    let task_id = state
        .record_task(
            "bench-prompt-generate",
            json!({ "backend": backend.id, "purpose": purpose, "size": size }),
        )
        .await;
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    {
        let mut slot = state.bench_generate_abort.lock().await;
        if slot.is_some() {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Failed,
                    json!({ "error": "a prompt generation is already running" }),
                )
                .await;
            return Err(ApiError::InvalidInput(
                "a prompt generation is already running".to_string(),
            ));
        }
        *slot = Some(tx);
    }
    let model_name = crate::model_for_backend(&cfg, &backend)
        .map(|m| m.id.clone())
        .unwrap_or_else(|| backend.id.clone());
    let system_prompt = format!(
        "You write benchmark prompts for a local LLM benchmarking tool. \
         Output ONLY the prompt text itself — no commentary, no preamble, no markdown fences. \
         The prompt must be fully self-contained: every piece of material the model needs \
         (code, documents, data) is inside the prompt itself. \
         Purpose tag: {purpose}. Target length: about {target_tokens} tokens. \
         For a coder prompt, include real, coherent code with consistent identifiers and an \
         edit-or-continue task that deliberately reuses those identifiers. \
         For a planner prompt, state a constrained problem with explicit constraints and a \
         concrete deliverable. \
         For a tool-runner prompt, describe a scenario that calls for structured, tool-style \
         steps over concrete data included in the prompt. \
         For a general prompt, include substantive source text and a question over it."
    );
    let messages = vec![
        manager_supervisor::ChatMessage {
            role: "system".to_string(),
            content: system_prompt,
        },
        manager_supervisor::ChatMessage {
            role: "user".to_string(),
            content: description.trim().to_string(),
        },
    ];
    let base_url = format!("http://127.0.0.1:{}", backend.port);
    let completion = manager_supervisor::chat_completion(
        &base_url,
        &model_name,
        &messages,
        GENERATE_TEMPERATURE,
        max_tokens,
        GENERATE_TIMEOUT,
    );
    let outcome = tokio::select! {
        res = completion => Some(res),
        _ = rx => None,
    };
    {
        let mut slot = state.bench_generate_abort.lock().await;
        *slot = None;
    }
    match outcome {
        None => {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Aborted,
                    json!({ "backend": backend.id }),
                )
                .await;
            Err(ApiError::InvalidInput(
                "prompt generation cancelled".to_string(),
            ))
        }
        Some(Err(e)) => {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Failed,
                    json!({ "error": e.to_string() }),
                )
                .await;
            Err(ApiError::InvalidInput(format!(
                "prompt generation failed: {e}"
            )))
        }
        Some(Ok(result)) => {
            if result.content.trim().is_empty() {
                state
                    .finish_task(
                        &task_id,
                        TaskStatus::Failed,
                        json!({ "error": "the backend returned an empty draft" }),
                    )
                    .await;
                return Err(ApiError::InvalidInput(format!(
                    "backend '{}' returned an empty draft ({} transport bytes, {} reasoning bytes)",
                    backend.id, result.response_bytes, result.reasoning_bytes
                )));
            }
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Done,
                    json!({ "backend": backend.id, "chars": result.content.len() }),
                )
                .await;
            Ok(json!({
                "text": result.content,
                "backend_id": backend.id,
                "model": model_name,
            }))
        }
    }
}

pub async fn op_bench_prompt_generate_cancel(
    state: &Arc<AppState>,
    _sink: &Arc<dyn crate::EventSink>,
) -> crate::ApiResult {
    let sender = {
        let mut slot = state.bench_generate_abort.lock().await;
        slot.take()
    };
    match sender {
        Some(tx) => {
            let _ = tx.send(());
            Ok(json!({ "cancelled": true }))
        }
        None => Ok(json!({ "cancelled": false })),
    }
}

// ---------------------------------------------------------------------------
// GGUF header facts for validation pruning (§3.2): the model's
// trained context length and the tokenizer's vocabulary size,
// read from the file's metadata. The app's layout parser does
// not carry these two keys, so this is a minimal metadata walk
// (integers and the tokens array's element count only).
// ---------------------------------------------------------------------------

fn gguf_header_facts(path: &PathBuf) -> (Option<u32>, Option<u32>) {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return (None, None),
    };
    // Headers with a full token array run to a few MB; cap the
    // walk so a hostile file cannot make it read unboundedly.
    let mut buf = Vec::new();
    let _ = file
        .by_ref()
        .take(64 * 1024 * 1024)
        .read_to_end(&mut buf);
    parse_gguf_facts(&buf)
}

fn parse_gguf_facts(buf: &[u8]) -> (Option<u32>, Option<u32>) {
    walk_gguf_facts(buf).unwrap_or((None, None))
}

fn walk_gguf_facts(buf: &[u8]) -> Option<(Option<u32>, Option<u32>)> {
    struct R<'a> {
        b: &'a [u8],
        p: usize,
    }
    impl<'a> R<'a> {
        fn take(&mut self, n: usize) -> Option<&'a [u8]> {
            let end = self.p.checked_add(n)?;
            let s = self.b.get(self.p..end)?;
            self.p = end;
            Some(s)
        }
        fn u32(&mut self) -> Option<u32> {
            Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
        }
        fn u64(&mut self) -> Option<u64> {
            Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
        }
        fn string(&mut self) -> Option<String> {
            let n = self.u64()? as usize;
            Some(String::from_utf8_lossy(self.take(n)?).into_owned())
        }
        /// Skip one metadata value of the given GGUF type,
        /// reporting the tokens-array element count when the
        /// value is an array (the caller wants exactly one).
        fn skip_value(&mut self, ty: u32) -> Option<Option<u64>> {
            match ty {
                0 | 1 | 7 => {
                    self.take(1)?;
                    Some(None)
                }
                2 | 3 => {
                    self.take(2)?;
                    Some(None)
                }
                4..=6 => {
                    self.take(4)?;
                    Some(None)
                }
                8 => {
                    let n = self.u64()? as usize;
                    self.take(n)?;
                    Some(None)
                }
                9 => {
                    let elem_ty = self.u32()?;
                    let count = self.u64()?;
                    for _ in 0..count {
                        self.skip_value(elem_ty)?;
                    }
                    Some(Some(count))
                }
                10..=12 => {
                    self.take(8)?;
                    Some(None)
                }
                _ => None,
            }
        }
        fn int_value(&mut self, ty: u32) -> Option<u64> {
            match ty {
                0 => Some(self.take(1)?[0] as u64),
                2 => Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?) as u64),
                4 => Some(self.u32()? as u64),
                5 => Some(self.take(4).map(|b| {
                    i32::from_le_bytes(b.try_into().unwrap()) as u64
                })?),
                10 => self.u64(),
                11 => Some(self.u64()?),
                _ => {
                    self.skip_value(ty)?;
                    None
                }
            }
        }
    }
    let mut r = R { b: buf, p: 0 };
    if r.take(4)? != b"GGUF" {
        return None;
    }
    let _version = r.u32();
    let _tensor_count = r.u64()?;
    let kv_count = r.u64()?;
    let mut arch: Option<String> = None;
    let mut ctx_len: Option<u32> = None;
    let mut vocab: Option<u32> = None;
    for _ in 0..kv_count {
        let key = match r.string() {
            Some(k) => k,
            None => break,
        };
        let ty = match r.u32() {
            Some(t) => t,
            None => break,
        };
        if key == "general.architecture" && ty == 8 {
            arch = r.string();
            let _ = &arch;
            continue;
        }
        if key == "tokenizer.ggml.tokens" && ty == 9 {
            // Read the array header, record the count, then
            // skip the elements.
            let elem_ty = match r.u32() {
                Some(t) => t,
                None => break,
            };
            let count = match r.u64() {
                Some(c) => c,
                None => break,
            };
            vocab = Some(count as u32);
            let mut ok = true;
            for _ in 0..count {
                if r.skip_value(elem_ty).is_none() {
                    ok = false;
                    break;
                }
            }
            if !ok {
                break;
            }
            continue;
        }
        if ty != 9 && key.ends_with(".context_length") {
            if let Some(v) = r.int_value(ty) {
                ctx_len = Some(v as u32);
            }
            continue;
        }
        if r.skip_value(ty).is_none() {
            break;
        }
    }
    let _ = arch;
    Some((ctx_len, vocab))
}

// ---------------------------------------------------------------------------
// Sweep request, interlock, preflight (design §3, §6.1–§6.3)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SweepRequest {
    backend_id: String,
    prompt_id: String,
    #[serde(default)]
    temperature: f32,
    #[serde(default)]
    confirm_at_06: bool,
    #[serde(default)]
    correctness_control: bool,
    #[serde(default)]
    stop_backend_ids: Vec<String>,
    #[serde(default)]
    preset: Option<String>,
    #[serde(default)]
    dimensions: Option<Vec<Vec<mb::ArmVariant>>>,
    #[serde(default)]
    draft_model_id: Option<String>,
    #[serde(default)]
    baseline_arm_index: usize,
}

/// The target backend's current axes, read from its saved
/// configuration: the base every sweep variant merges over.
fn base_arm_spec(target: &BackendConfig) -> mb::ArmSpec {
    let raw = target.raw_flags.as_deref().unwrap_or("");
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    let mut cache_k = mb::CacheType::F16;
    let mut cache_v = mb::CacheType::F16;
    let mut flash = mb::FlashAttn::Auto;
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        let (flag, inline_val) = match t.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (t, None),
        };
        let val = |i: &mut usize| -> Option<String> {
            if let Some(v) = &inline_val {
                return Some(v.clone());
            }
            *i += 1;
            tokens.get(*i).map(|s| s.to_string())
        };
        match flag {
            "--cache-type-k" | "-ctk" => {
                if let Some(v) = val(&mut i) {
                    cache_k = parse_cache_type(&v).unwrap_or(cache_k);
                }
            }
            "--cache-type-v" | "-ctv" => {
                if let Some(v) = val(&mut i) {
                    cache_v = parse_cache_type(&v).unwrap_or(cache_v);
                }
            }
            "--flash-attn" => {
                if let Some(v) = val(&mut i) {
                    flash = match v.as_str() {
                        "on" => mb::FlashAttn::On,
                        "off" => mb::FlashAttn::Off,
                        _ => mb::FlashAttn::Auto,
                    };
                }
            }
            _ => {}
        }
        i += 1;
    }
    // The target's effective expert offload, decoded from its
    // raw flags and re-spelled in the canonical form the app
    // itself writes: an MoE backend's arms record the position
    // they run at (the arm label names it), and the offload
    // dimension sweeps positions from this base. The launch
    // rewrite strips the base's expert tokens and appends this
    // expression, so the launched position is the same one —
    // only its spelling is canonical. `None` when the backend
    // offloads nothing: its arms carry no offload expression
    // and the base tokens pass through untouched.
    let offload_expr = manager_models::offload_flag_string(manager_models::expert_offload(
        target.raw_flags.as_deref(),
    ));
    mb::ArmSpec {
        cache_k,
        cache_v,
        flash_attn: flash,
        depth: 0,
        n_ctx: target.flags.n_ctx,
        spec: mb::SpecKind::None,
        ngram_n_min: None,
        draft_model_id: None,
        draft_n_max: None,
        draft_device: mb::DraftDevice::TargetGpu,
        draft_cache_k: mb::CacheType::Q8_0,
        draft_cache_v: mb::CacheType::Q8_0,
        offload_expr,
    }
}

fn parse_cache_type(s: &str) -> Option<mb::CacheType> {
    match s {
        "f16" => Some(mb::CacheType::F16),
        "q8_0" => Some(mb::CacheType::Q8_0),
        "q4_0" => Some(mb::CacheType::Q4_0),
        _ => None,
    }
}

/// Does this backend run layers on the CPU (experts or all
/// layers)? The CPU-offload contention class of §6.2.
fn cpu_offload(b: &BackendConfig) -> bool {
    !matches!(
        manager_models::expert_offload(b.raw_flags.as_deref()),
        manager_models::ExpertOffload::None
    ) || matches!(crate::backend_gpu_visibility(b), crate::GpuVisibility::CpuOnly)
}

/// A backend's GPU placement as the interlock judges it (§6.1).
///
/// This is deliberately NOT `crate::backend_gpu_visibility`:
/// that function reports `Undetermined` for any backend whose
/// device selection lives in raw flags and is not exactly
/// {GPU 0}, because the LAUNCH path will not remap a raw
/// selection it does not model (code-review 4.4) and launches it
/// unrestricted instead. For conflict purposes a parsed raw
/// selection is determinate evidence of where the backend runs:
/// a backend raw-pinned to GPU 1 (`--tensor-split 0,1`,
/// `--device CUDA1`, `--main-gpu 1`) shares no GPU with a GPU-0
/// target, and judging it Undetermined made it conflict with
/// every sweep on the machine. Only a selection that does not
/// parse stays Undetermined here, and an Undetermined placement
/// still conflicts with everything (conservative: a sweep that
/// cannot prove isolation does not run).
#[derive(Debug, Clone, PartialEq)]
enum BenchPlacement {
    CpuOnly,
    Gpus(std::collections::BTreeSet<u32>),
    Undetermined,
}

fn bench_placement(b: &BackendConfig) -> BenchPlacement {
    if crate::raw_flags_pin_cpu_only(b.raw_flags.as_deref()) {
        return BenchPlacement::CpuOnly;
    }
    if !b.flags.tensor_split.is_empty() {
        // A structured split remains the source of truth and a
        // raw selection yields to it, as in the launch path.
        let placed: std::collections::BTreeSet<u32> = b
            .flags
            .tensor_split
            .iter()
            .enumerate()
            .filter(|(_, f)| **f > 0.0)
            .map(|(i, _)| i as u32)
            .collect();
        return if placed.is_empty() {
            BenchPlacement::Undetermined
        } else {
            BenchPlacement::Gpus(placed)
        };
    }
    match crate::raw_device_selection(b.raw_flags.as_deref()) {
        crate::RawDeviceSelection::Set(set) => BenchPlacement::Gpus(set),
        // Unsplit with no raw selection: llama.cpp's main GPU.
        crate::RawDeviceSelection::None => {
            BenchPlacement::Gpus(std::collections::BTreeSet::from([0]))
        }
        crate::RawDeviceSelection::Unparseable => BenchPlacement::Undetermined,
    }
}

/// The running backends that conflict with a sweep against
/// `target` (§6.1/§6.2): any backend sharing a GPU with the
/// target's placement, the target itself, and — when the TARGET
/// runs layers on the CPU — any other CPU-offload backend (the
/// CPU is shared by every MoE backend's experts, §3.4/§6.2; the
/// rule binds on the target's offload, not on any running
/// backend's). An undetermined placement conflicts with
/// everything (conservative: a sweep that cannot prove isolation
/// does not run).
fn interlock_conflicts(
    cfg: &AppConfig,
    running: &HashSet<String>,
    target: &BackendConfig,
) -> Vec<String> {
    let target_place = bench_placement(target);
    let target_cpu = cpu_offload(target);
    let mut out: Vec<String> = Vec::new();
    for b in &cfg.backends {
        if !running.contains(&b.id) {
            continue;
        }
        if b.id == target.id {
            out.push(b.id.clone());
            continue;
        }
        let place = bench_placement(b);
        let gpu_conflict = match (&target_place, &place) {
            (BenchPlacement::Gpus(a), BenchPlacement::Gpus(bset)) => {
                a.iter().any(|g| bset.contains(g))
            }
            (BenchPlacement::CpuOnly, _) | (_, BenchPlacement::CpuOnly) => false,
            _ => true, // Undetermined on either side
        };
        let cpu_conflict = target_cpu && cpu_offload(b);
        if gpu_conflict || cpu_conflict {
            out.push(b.id.clone());
        }
    }
    out
}

/// Guaranteed cleanup for one bench temp backend, on the
/// experiment pattern: stop it when live, unregister its spec,
/// remove its journal record. Runs outside the arm future, so
/// neither cancellation nor failure can skip it.
async fn cleanup_bench_temp(state: &AppState, temp_id: &str) {
    {
        let mut sup = state.supervisor().await.lock().await;
        sup.reap_exited();
        let live = sup
            .status()
            .iter()
            .any(|s| s.id == temp_id && s.pid.is_some());
        if live {
            let _ = sup.stop(temp_id, false).await;
        }
        sup.unregister_spec(temp_id);
    }
    let mut journal = load_journal(state);
    if journal.remove_active(temp_id) {
        save_journal(state, &journal);
    }
}

/// POST /tokenize against a bench backend: the measured token
/// count of a text. None when the endpoint does not answer
/// (older builds) — callers fall back to a character estimate.
async fn tokenize_count(base_url: &str, text: &str) -> Option<u32> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .ok()?;
    let resp = client
        .post(format!("{base_url}/tokenize"))
        .header("content-type", "application/json")
        .body(serde_json::to_string(&json!({ "content": text })).ok()?)
        .send()
        .await
        .ok()?;
    let text_body = resp.text().await.ok()?;
    let body: Value = serde_json::from_str(&text_body).ok()?;
    body.get("tokens")
        .and_then(|t| t.as_array())
        .map(|a| a.len() as u32)
}

/// Word-set overlap of a candidate output against the baseline
/// output (the correctness control's divergence measure, §2.1):
/// the share of the baseline's distinct words that also appear
/// in the candidate. Identical greedy outputs score 1.0.
fn word_overlap(baseline: &str, candidate: &str) -> f64 {
    let base: HashSet<&str> = baseline.split_whitespace().collect();
    if base.is_empty() {
        return 1.0;
    }
    let cand: HashSet<&str> = candidate.split_whitespace().collect();
    let shared = base.intersection(&cand).count();
    shared as f64 / base.len() as f64
}

// ---------------------------------------------------------------------------
// One arm's run (§2.1 protocol): temp backend, preflight,
// tokenized depth padding, warm-up + measured runs, spec log
// collection, VRAM peak sampling.
// ---------------------------------------------------------------------------

const REQUEST_TIMEOUT: Duration = Duration::from_secs(900);
const FIXED_SEED: u32 = 42;

struct ArmOutcomeData {
    status: mb::ArmStatus,
    detail: Option<String>,
    runs: Vec<mb::RunRow>,
    peak_vram_mib: Option<u64>,
    /// First measured run's output text (correctness control).
    first_output: Option<String>,
    /// The sweep's llama build, learned from this arm's server.
    llama_build: Option<String>,
    /// The library prompt's tokenized count, learned per sweep.
    prompt_tokens: Option<u32>,
}

/// The log lines a request added to the temp backend's ring,
/// reduced to the newest window after `before` (the ring's
/// newest record before the request). Records come back
/// newest-first; matching the pre-request newest record inside
/// the post-request page bounds the new window.
fn new_log_lines(
    sup: &manager_supervisor::Supervisor,
    temp_id: &str,
    before_newest: Option<&manager_supervisor::LogRecord>,
) -> Vec<String> {
    let result = sup.logs_query(temp_id, None, None, 4000);
    let mut lines: Vec<String> = Vec::new();
    for rec in &result.records {
        if let Some(b) = before_newest {
            if rec.ts == b.ts && rec.line == b.line {
                break;
            }
        }
        lines.push(rec.line.clone());
    }
    lines.reverse(); // chronological
    lines
}

fn newest_log_record(
    sup: &manager_supervisor::Supervisor,
    temp_id: &str,
) -> Option<manager_supervisor::LogRecord> {
    sup.logs_query(temp_id, None, None, 1).records.into_iter().next()
}

/// Parse one request window's lines into a run row.
fn run_row_from(
    sample: &manager_supervisor::BenchmarkSample,
    lines: &[String],
    warmup: bool,
    prompt_tokens: Option<u32>,
) -> mb::RunRow {
    let mut row = mb::RunRow {
        warmup,
        prompt_tps: sample.prompt_per_second,
        gen_tps: sample.predicted_per_second,
        ttft_ms: sample.ttft_ms,
        prompt_tokens,
        generated_tokens: None,
        acceptance_rate: None,
        accepted: None,
        generated: None,
        mean_accepted_len: None,
        drafters: Vec::new(),
    };
    // The aggregate line is the request's own verdict; the
    // statistics lines attribute it per drafter (§1.5).
    for line in lines {
        if let Some(acc) = mb::parse_acceptance_line(line) {
            row.acceptance_rate = Some(acc.rate);
            row.accepted = Some(acc.accepted);
            row.generated = Some(acc.generated);
            row.mean_accepted_len = Some(acc.mean_len);
            row.generated_tokens = Some(acc.generated as u32);
        }
        if let Some(stats) = mb::parse_statistics_line(line) {
            if let Some(existing) = row
                .drafters
                .iter_mut()
                .find(|d: &&mut mb::DrafterAgg| d.kind == stats.kind)
            {
                existing.gen_tokens += stats.gen_tokens;
                existing.acc_tokens += stats.acc_tokens;
            } else {
                row.drafters.push(mb::DrafterAgg {
                    kind: stats.kind.clone(),
                    gen_tokens: stats.gen_tokens,
                    acc_tokens: stats.acc_tokens,
                });
            }
        }
    }
    row
}

/// Immutable per-sweep context the arm runner needs.
struct SweepCtx {
    cfg: AppConfig,
    target: BackendConfig,
    model_id: String,
    temperature: f32,
    draft_path: Option<String>,
    draft_device_arg: Option<String>,
}

async fn set_progress(state: &AppState, arm_index: usize, arm_count: usize, phase: &str, run: usize) {
    if let Ok(mut slot) = state.bench_job.try_lock() {
        if let Some(job) = slot.as_mut() {
            job.progress.arm_index = arm_index;
            job.progress.arm_count = arm_count;
            job.progress.phase = phase.to_string();
            job.progress.run = run;
        }
    }
}

/// Save one sweep record into the results store (replace by id
/// or append).
fn save_sweep(state: &AppState, record: &mb::SweepRecord) {
    let mut results = load_results(state);
    if let Some(existing) = results.sweeps.iter_mut().find(|s| s.id == record.id) {
        *existing = record.clone();
    } else {
        results.sweeps.push(record.clone());
    }
    save_results(state, &results);
}

/// Run one arm end to end. Every failure is data (an
/// ArmOutcomeData with a non-Measured status), never an Err:
/// the sweep records it and moves on. Cleanup is the CALLER's
/// job — it lives outside this future so cancellation cannot
/// skip it.
#[allow(clippy::too_many_arguments)]
async fn run_arm(
    state: &AppState,
    ctx: &SweepCtx,
    sweep_id: &str,
    arm_index: usize,
    arm_count: usize,
    spec: &mb::ArmSpec,
    raw_flags: &str,
    port: u16,
    prompt_body: &str,
) -> ArmOutcomeData {
    let mut out = ArmOutcomeData {
        status: mb::ArmStatus::Failed,
        detail: None,
        runs: Vec::new(),
        peak_vram_mib: None,
        first_output: None,
        llama_build: None,
        prompt_tokens: None,
    };
    let temp_id = format!("bench-{}", arm_index + 1);
    let mut temp = ctx.target.clone();
    temp.id = temp_id.clone();
    temp.port = port;
    temp.flags.n_ctx = spec.n_ctx;
    // The bench backend runs at the most verbose log level the
    // app exposes: the per-drafter TRACE statistics (§1.5) are
    // the combined arms' attribution source.
    temp.raw_flags = Some(format!("{raw_flags} --verbose"));
    let mut temp_cfg = ctx.cfg.clone();
    temp_cfg.backends.push(temp.clone());

    // -- Start preflight (§6.3): the same gate every app-mediated
    // start passes, evaluated against the cloned config carrying
    // the temp backend. Its verdicts carry the numbers.
    if let Err(e) = crate::backend_start_preflight_for(state, &temp_cfg, &temp, false).await {
        out.status = mb::ArmStatus::PreflightFailed;
        out.detail = Some(format!("start preflight refused this arm: {e}"));
        return out;
    }

    // -- Journal before the spawn, so a crash from here on
    // leaves a sweep record.
    {
        let mut journal = load_journal(state);
        journal.add_active(ActiveBench {
            temp_id: temp_id.clone(),
            pid: None,
            port,
            sweep_id: sweep_id.to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
        });
        save_journal(state, &journal);
    }

    // -- Register the spec directly and spawn.
    let pid: Option<u32> = {
        let store = state.telemetry().await;
        let gpus = crate::lock_telemetry(&store).inventory();
        let spec_proc = crate::backend_spec(&ctx.cfg, &temp, &gpus);
        let mut sup = state.supervisor().await.lock().await;
        sup.register_spec(spec_proc);
        if let Err(e) = sup.spawn_backend(&temp_id).await {
            out.detail = Some(format!("temp backend {temp_id} failed to start: {e:?}"));
            return out;
        }
        sup.status()
            .into_iter()
            .find(|s| s.id == temp_id)
            .and_then(|s| s.pid)
    };
    if let Some(pid) = pid {
        let mut journal = load_journal(state);
        journal.set_active_pid(&temp_id, pid);
        save_journal(state, &journal);
    }

    // -- Wait healthy on the existing probe.
    if let Err(e) = crate::optimize_experiments::wait_healthy(state, &temp_id, port).await {
        out.detail = Some(e);
        return out;
    }
    let base_url = format!("http://127.0.0.1:{port}");
    out.llama_build = manager_supervisor::server_build_info(&base_url).await;

    // -- The measured prompt: the library prompt padded to the
    // arm's depth with DISTINCT shipped library text (§2.1 —
    // never repeated filler, which would falsify n-gram
    // acceptance). Depth 0, or a prompt already at depth, runs
    // unpadded.
    let library_tokens = tokenize_count(&base_url, prompt_body).await;
    out.prompt_tokens = library_tokens;
    let measured_prompt = match library_tokens {
        Some(have) if spec.depth > have => {
            let need = spec.depth - have;
            let mut pad = String::new();
            let mut est: u32 = 0;
            let blocks = mb::padding_blocks();
            let mut bi = 0usize;
            while est < need && !blocks.is_empty() {
                let block = blocks[bi % blocks.len()];
                // The padding estimate is a character heuristic;
                // the run rows carry the backend's own prompt
                // rate, and the depth is the target the padding
                // aims at, not a claim about the exact fill.
                est += (block.len() as u32) / 4;
                pad.push_str(block);
                pad.push_str("\n\n");
                bi += 1;
            }
            format!("{pad}{prompt_body}")
        }
        _ => prompt_body.to_string(),
    };
    let measured_tokens = tokenize_count(&base_url, &measured_prompt).await;

    // -- VRAM peak sampling for the arm's duration (§5.1).
    let peak = Arc::new(std::sync::Mutex::new(Option::<u64>::None));
    let sampler_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler = if let Some(pid) = pid {
        let store = state.telemetry().await;
        let peak = Arc::clone(&peak);
        let stop = Arc::clone(&sampler_stop);
        Some(tokio::spawn(async move {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                {
                    let guard = crate::lock_telemetry(&store);
                    let total: u64 = guard
                        .last_processes()
                        .iter()
                        .filter(|p| p.pid == pid)
                        .filter_map(|p| p.vram_mib)
                        .sum();
                    if total > 0 {
                        let mut slot = peak.lock().unwrap();
                        *slot = Some(slot.map(|p| p.max(total)).unwrap_or(total));
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }))
    } else {
        None
    };

    // -- Warm-up (discarded from the medians; its row is kept,
    // flagged), then the measured runs. The spec-log window for
    // each request is the ring's growth across that request.
    let mut first_output: Option<String> = None;
    let mut failure: Option<String> = None;
    let phases: Vec<(bool, u32, &str)> = std::iter::once((true, mb::WARMUP_MAX_TOKENS, "warming"))
        .chain((0..mb::MEASURED_RUNS).map(|_| (false, mb::MEASURED_MAX_TOKENS, "measuring")))
        .collect();
    for (step, (warmup, cap, phase)) in phases.iter().enumerate() {
        set_progress(state, arm_index, arm_count, phase, if *warmup { 0 } else { step }).await;
        let before = {
            let sup = state.supervisor().await.lock().await;
            newest_log_record(&sup, &temp_id)
        };
        let messages = vec![manager_supervisor::ChatMessage {
            role: "user".to_string(),
            content: measured_prompt.clone(),
        }];
        match manager_supervisor::benchmark_completion_seeded(
            &base_url,
            &ctx.model_id,
            &messages,
            ctx.temperature,
            *cap,
            Some(FIXED_SEED),
            REQUEST_TIMEOUT,
        )
        .await
        {
            Ok(sample) => {
                let lines = {
                    let sup = state.supervisor().await.lock().await;
                    new_log_lines(&sup, &temp_id, before.as_ref())
                };
                let row = run_row_from(&sample, &lines, *warmup, measured_tokens);
                if !*warmup && first_output.is_none() {
                    first_output = Some(sample.content.clone());
                }
                out.runs.push(row);
            }
            Err(e) => {
                failure = Some(format!(
                    "{} failed: {e}",
                    if *warmup { "warm-up" } else { "benchmark run" }
                ));
                break;
            }
        }
    }
    sampler_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(handle) = sampler {
        let _ = handle.await;
    }
    out.peak_vram_mib = *peak.lock().unwrap();
    out.first_output = first_output;
    match failure {
        Some(f) => {
            out.status = mb::ArmStatus::Failed;
            out.detail = Some(f);
        }
        None => {
            out.status = mb::ArmStatus::Measured;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The sweep executor: arms in order, cancellation between and
// within arms (cleanup always outside the arm future), the
// correctness control, and the record kept current on disk.
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn execute_sweep(
    state: &Arc<AppState>,
    ctx: &SweepCtx,
    record: &mut mb::SweepRecord,
    outcomes: Vec<mb::ArmOutcome>,
    prompt_body: &str,
    correctness_control: bool,
    rx: &mut tokio::sync::oneshot::Receiver<()>,
) -> bool {
    let arm_count = outcomes.len();
    let mut used_ports: HashSet<u16> = ctx.cfg.backends.iter().map(|b| b.port).collect();
    let mut cancelled = false;
    let mut baseline_output: Option<String> = None;
    let mut stop_note: Option<String> = None;
    for (index, outcome) in outcomes.iter().enumerate() {
        if cancelled || stop_note.is_some() {
            let arm_rec = &mut record.arms[index];
            if arm_rec.status == mb::ArmStatus::Pending {
                arm_rec.status = mb::ArmStatus::Cancelled;
                arm_rec.detail = Some(
                    stop_note
                        .clone()
                        .unwrap_or_else(|| "sweep cancelled by the operator".to_string()),
                );
            }
            continue;
        }
        match outcome {
            mb::ArmOutcome::Pruned { reason, .. } => {
                let arm_rec = &mut record.arms[index];
                arm_rec.status = mb::ArmStatus::Pruned;
                arm_rec.detail = Some(reason.clone());
                save_sweep(state, record);
            }
            mb::ArmOutcome::Ready(spec) => {
                let raw_flags = mb::arm_raw_flags(
                    ctx.target.raw_flags.as_deref(),
                    spec,
                    ctx.draft_path.as_deref(),
                    ctx.draft_device_arg.as_deref(),
                );
                {
                    let arm_rec = &mut record.arms[index];
                    arm_rec.raw_flags = raw_flags.clone();
                    arm_rec.status = mb::ArmStatus::Running;
                    arm_rec.started_ms = Some(now_ms());
                }
                save_sweep(state, record);
                set_progress(state, index, arm_count, "launching", 0).await;
                let port = manager_optimize::lowest_free_port(
                    &used_ports,
                    ctx.cfg.gateway.port,
                    ctx.cfg.postgres.port,
                );
                used_ports.insert(port);
                let temp_id = format!("bench-{}", index + 1);
                let arm_future = run_arm(
                    state,
                    ctx,
                    &record.id,
                    index,
                    arm_count,
                    spec,
                    &raw_flags,
                    port,
                    prompt_body,
                );
                let data = tokio::select! {
                    _ = &mut *rx => {
                        cancelled = true;
                        None
                    }
                    data = arm_future => Some(data),
                };
                // Cleanup is outside the arm future: it runs on
                // completion, failure, and cancellation alike.
                cleanup_bench_temp(state, &temp_id).await;
                let arm_rec = &mut record.arms[index];
                arm_rec.ended_ms = Some(now_ms());
                match data {
                    None => {
                        arm_rec.status = mb::ArmStatus::Cancelled;
                        arm_rec.detail =
                            Some("sweep cancelled by the operator".to_string());
                        record.partial = true;
                    }
                    Some(data) => {
                        arm_rec.status = data.status;
                        arm_rec.detail = data.detail;
                        arm_rec.runs = data.runs;
                        arm_rec.peak_vram_mib = data.peak_vram_mib;
                        if !arm_rec.runs.is_empty() {
                            arm_rec.summary = Some(mb::summarize(&arm_rec.runs));
                        }
                        if record.llama_build.is_empty() {
                            if let Some(build) = &data.llama_build {
                                record.llama_build = build.clone();
                            }
                        }
                        if record.prompt_tokens.is_none() {
                            record.prompt_tokens = data.prompt_tokens;
                        }
                        if index == record.baseline_arm_index {
                            baseline_output = data.first_output.clone();
                        }
                        // Correctness control (§2.1): a spec
                        // arm whose temperature-0 output
                        // diverges from the baseline's stops
                        // the sweep; the divergence is stated
                        // on the record, never hidden.
                        if correctness_control
                            && ctx.temperature == 0.0
                            && spec.spec != mb::SpecKind::None
                            && index != record.baseline_arm_index
                            && arm_rec.status == mb::ArmStatus::Measured
                        {
                            if let (Some(base), Some(out_text)) =
                                (&baseline_output, &data.first_output)
                            {
                                let overlap = word_overlap(base, out_text);
                                if overlap < 0.5 {
                                    let note = format!(
                                        "correctness control: arm {} output diverged from the baseline (word overlap {:.0}% < 50%); sweep stopped",
                                        index + 1,
                                        overlap * 100.0
                                    );
                                    arm_rec.detail = Some(match arm_rec.detail.take() {
                                        Some(d) => format!("{d} {note}"),
                                        None => note.clone(),
                                    });
                                    stop_note = Some(note);
                                    record.partial = true;
                                }
                            }
                        }
                    }
                }
                save_sweep(state, record);
            }
        }
    }
    // Comparability keys (§3.3): one key per sweep — the
    // held-constant conditions (model identity, runner, prompt,
    // temperature, llama build) are sweep-level facts. Set once
    // the build is known from a launched arm; when no arm ever
    // launched, the key stays empty and the record says so by
    // absence rather than by a fabricated build string.
    if !record.llama_build.is_empty() {
        let key = mb::comparability_key(
            &record.model,
            &record.prompt_sha256,
            record.temperature,
            &record.llama_build,
        );
        for arm in record.arms.iter_mut() {
            arm.comparability_key = key.clone();
        }
    }
    if cancelled {
        record.partial = true;
    }
    record.status = if record.partial {
        "partial".to_string()
    } else {
        "complete".to_string()
    };
    record.ended_ms = Some(now_ms());
    save_sweep(state, record);
    cancelled
}

// ---------------------------------------------------------------------------
// Sweep ops (§7)
// ---------------------------------------------------------------------------

pub async fn op_bench_run_start(
    state: &Arc<AppState>,
    _sink: &Arc<dyn crate::EventSink>,
    request: Value,
) -> crate::ApiResult {
    let req: SweepRequest = serde_json::from_value(request)
        .map_err(|e| ApiError::InvalidInput(format!("invalid sweep request: {e}")))?;
    // One sweep at a time (§6.4), and never alongside a
    // measurement/experiment job (they contend for the same
    // GPUs the interlock reasons about).
    if state.bench_job.lock().await.is_some() {
        return Err(ApiError::InvalidInput(
            "a bench sweep is already running".to_string(),
        ));
    }
    if state.optimize_job_abort.lock().await.is_some() {
        return Err(ApiError::InvalidInput(
            "a measurement or experiment job is already running".to_string(),
        ));
    }
    let cfg = state.config.read().await.clone();
    let target = cfg
        .backends
        .iter()
        .find(|b| b.id == req.backend_id)
        .cloned()
        .ok_or_else(|| ApiError::NotFound(format!("no backend '{}'", req.backend_id)))?;
    let lib = load_prompts(state);
    let prompt = lib
        .entries
        .iter()
        .find(|p| p.id == req.prompt_id)
        .cloned()
        .ok_or_else(|| ApiError::NotFound(format!("no prompt '{}'", req.prompt_id)))?;

    // Dimensions: a shipped preset (D16) or the operator's own.
    let preset = match req.preset.as_deref() {
        Some("kv") => Some(mb::preset_kv()),
        Some("spec") => Some(mb::preset_spec()),
        Some("combined") => Some(mb::preset_combined()),
        Some(other) => {
            return Err(ApiError::InvalidInput(format!(
                "unknown preset '{other}' (kv | spec | combined)"
            )))
        }
        None => None,
    };
    let dimensions: Vec<Vec<mb::ArmVariant>> = match (&preset, &req.dimensions) {
        (Some(p), _) => p.dimensions.clone(),
        (None, Some(d)) if !d.is_empty() => d.clone(),
        _ => {
            return Err(ApiError::InvalidInput(
                "a sweep needs a preset or custom dimensions".to_string(),
            ))
        }
    };
    let mut base = base_arm_spec(&target);
    if let Some(d) = &req.draft_model_id {
        base.draft_model_id = Some(d.clone());
    }
    let arms = mb::expand(&base, &dimensions);

    // GGUF facts for pruning (§3.2): trained context + vocab
    // shapes for the target and (when a draft arm exists) the
    // draft model.
    let model_entry = crate::model_for_backend(&cfg, &target).cloned();
    let model_abs = model_entry
        .as_ref()
        .map(|m| crate::resolve_under_data_dir(&cfg.data_dir, &m.gguf_path))
        .unwrap_or_else(|| crate::resolve_under_data_dir(&cfg.data_dir, &target.model_file));
    let (model_ctx_len, target_vocab) = gguf_header_facts(&model_abs);
    let draft_model_id = arms
        .iter()
        .find_map(|a| a.draft_model_id.clone())
        .or_else(|| base.draft_model_id.clone());
    let (draft_path, draft_vocab) = match &draft_model_id {
        Some(id) => {
            let entry = cfg.models.iter().find(|m| &m.id == id).ok_or_else(|| {
                ApiError::InvalidInput(format!(
                    "draft model '{id}' is not a registered model"
                ))
            })?;
            let abs = crate::resolve_under_data_dir(&cfg.data_dir, &entry.gguf_path);
            let (_, vocab) = gguf_header_facts(&abs);
            (Some(abs.to_string_lossy().into_owned()), vocab)
        }
        None => (None, None),
    };
    // Offload guardrail facts for pruning (§3.2 rule 5): when
    // the target's model carries catalog offload restrictions,
    // the positions the guardrail blocks on the target's
    // placement are computed with the same evaluation the start
    // preflight uses, so blocked positions prune here with the
    // reason instead of failing one temp-backend start per arm.
    // A journaled override the target may carry does NOT extend
    // to bench temporaries — the preflight evaluates the temp's
    // own id, which no override names — so positions are judged
    // unoverridden, exactly as the preflight will judge them.
    // Nothing is pruned when no restriction governs the
    // placement (the allowed set then covers every position).
    let gpu_inventory = {
        let store = state.telemetry().await;
        let gpus = crate::lock_telemetry(&store).inventory();
        gpus
    };
    let offload_guardrail = crate::backend_offload_info(&cfg, &target).and_then(|info| {
        if info.restrictions.is_empty() {
            return None;
        }
        let (placed_ccs, cpu_only) = crate::placed_offload_placement(&target, &gpu_inventory);
        let allowed_layers: Vec<u32> = (0..=info.moe_layers)
            .filter(|&layers| {
                manager_models::offload_status(
                    &info.restrictions,
                    layers,
                    info.moe_layers,
                    &placed_ccs,
                    cpu_only,
                ) != manager_models::OffloadStatus::Blocked
            })
            .collect();
        if allowed_layers.len() as u32 == info.moe_layers + 1 {
            return None;
        }
        let compute_capability = placed_ccs
            .iter()
            .copied()
            .find(|cc| {
                info.restrictions
                    .iter()
                    .any(|r| r.compute_capability == *cc)
            })
            .or_else(|| info.restrictions.first().map(|r| r.compute_capability))?;
        Some(mb::OffloadGuardrail {
            moe_layers: info.moe_layers,
            allowed_layers,
            compute_capability,
            inherited_layers: Some(info.layers),
        })
    });
    let prune_ctx = mb::PruneContext {
        model_ctx_len,
        target_vocab,
        draft_vocab,
        offload_guardrail,
    };
    let mut outcomes = mb::prune_arms(arms, &prune_ctx);
    // Glue rule (§2.1): an arm whose depth plus the measured
    // output cap cannot fit its context cannot run as defined.
    for outcome in outcomes.iter_mut() {
        if let mb::ArmOutcome::Ready(spec) = outcome {
            if spec.depth > 0 && spec.depth + mb::MEASURED_MAX_TOKENS > spec.n_ctx {
                *outcome = mb::ArmOutcome::Pruned {
                    spec: spec.clone(),
                    reason: format!(
                        "depth {} + {} output tokens exceeds n_ctx {}",
                        spec.depth,
                        mb::MEASURED_MAX_TOKENS,
                        spec.n_ctx
                    ),
                };
            }
        }
    }

    // Interlock (§6.1): conflicts must be stopped before the
    // sweep starts — either already stopped, or named in the
    // operator's confirmed stop list. Bench never stops a
    // backend on its own say-so, and never restarts one.
    let running: HashSet<String> = {
        let mut sup = state.supervisor().await.lock().await;
        sup.reap_exited();
        sup.status()
            .into_iter()
            .filter(|s| s.pid.is_some())
            .map(|s| s.id)
            .collect()
    };
    let conflicts = interlock_conflicts(&cfg, &running, &target);
    let uncovered: Vec<&String> = conflicts
        .iter()
        .filter(|id| !req.stop_backend_ids.contains(id))
        .collect();
    if !uncovered.is_empty() {
        let names = uncovered
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ApiError::InvalidInput(format!(
            "running backends conflict with this sweep and must be stopped first: {names} [conflicts: {names}]"
        )));
    }
    let mut stopped_names: Vec<String> = Vec::new();
    for id in &conflicts {
        let mut sup = state.supervisor().await.lock().await;
        match sup.stop(id, false).await {
            Ok(()) => stopped_names.push(id.clone()),
            Err(e) => {
                return Err(ApiError::InvalidInput(format!(
                    "stopping backend '{id}' for the sweep failed: {e:?}"
                )))
            }
        }
    }

    // Draft device argument, resolved against the launch's
    // visibility restriction: the target's placed GPUs are the
    // only visible devices, so the target GPU is CUDA0 inside
    // the bench backend; a CPU placement runs the draft on CPU.
    let draft_device_arg = match crate::backend_gpu_visibility(&target) {
        crate::GpuVisibility::CpuOnly => Some("none".to_string()),
        _ => Some("CUDA0".to_string()),
    };

    // The sweep record, with every arm's snapshot (§3.1).
    let sweep_id = format!("bench-{}", now_ms());
    let ready_specs: Vec<mb::ArmSpec> = outcomes
        .iter()
        .filter_map(|o| match o {
            mb::ArmOutcome::Ready(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    let projected = mb::project_minutes(&ready_specs);
    let baseline_arm_index = if req.baseline_arm_index < outcomes.len() {
        req.baseline_arm_index
    } else {
        0
    };
    let machine_gpus: Vec<String> = gpu_inventory
        .iter()
        .map(|g| format!("{} (GPU {})", g.name, g.index))
        .collect();
    let model_identity = mb::ModelIdentity {
        path: model_abs.to_string_lossy().into_owned(),
        size_bytes: std::fs::metadata(&model_abs).map(|m| m.len()).unwrap_or(0),
        sha256: model_entry
            .as_ref()
            .and_then(|m| m.verified_sha256.clone()),
    };
    let mut record = mb::SweepRecord {
        id: sweep_id.clone(),
        status: "running".to_string(),
        started_ms: now_ms(),
        ended_ms: None,
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        llama_build: String::new(),
        machine_gpus,
        backend_id: target.id.clone(),
        backend_name: target.id.clone(),
        model: model_identity,
        prompt_id: prompt.id.clone(),
        prompt_name: prompt.name.clone(),
        prompt_sha256: mb::prompt_sha256(&prompt.body),
        prompt_tokens: None,
        temperature: req.temperature,
        correctness_control: req.correctness_control,
        interlock: mb::InterlockRecord {
            running_before: conflicts.clone(),
            stopped_confirmed: stopped_names.clone(),
        },
        baseline_arm_index,
        projected_minutes: projected,
        partial: false,
        arms: outcomes
            .iter()
            .enumerate()
            .map(|(index, o)| {
                let spec = match o {
                    mb::ArmOutcome::Ready(s) => s.clone(),
                    mb::ArmOutcome::Pruned { spec, .. } => spec.clone(),
                };
                mb::ArmRecord {
                    index,
                    label: spec.label(),
                    spec: spec.clone(),
                    raw_flags: String::new(),
                    comparability_key: String::new(),
                    status: mb::ArmStatus::Pending,
                    detail: None,
                    runs: Vec::new(),
                    summary: None,
                    peak_vram_mib: None,
                    started_ms: None,
                    ended_ms: None,
                }
            })
            .collect(),
    };
    save_sweep(state, &record);

    let task_id = state
        .record_task(
            "bench-run",
            json!({
                "sweep_id": sweep_id,
                "backend": target.id,
                "prompt": prompt.id,
                "arms": outcomes.len(),
            }),
        )
        .await;
    let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
    {
        let mut slot = state.bench_abort.lock().await;
        *slot = Some(tx);
    }
    {
        let mut slot = state.bench_job.lock().await;
        *slot = Some(BenchJob {
            sweep_id: sweep_id.clone(),
            progress: BenchProgress {
                arm_index: 0,
                arm_count: outcomes.len(),
                phase: "launching".to_string(),
                run: 0,
                started_unix_ms: now_ms(),
            },
        });
    }
    let ctx = SweepCtx {
        cfg: cfg.clone(),
        target: target.clone(),
        model_id: model_entry
            .as_ref()
            .map(|m| m.id.clone())
            .unwrap_or_else(|| target.id.clone()),
        temperature: req.temperature,
        draft_path: draft_path.clone(),
        draft_device_arg: draft_device_arg.clone(),
    };
    let mut cancelled = execute_sweep(
        state,
        &ctx,
        &mut record,
        outcomes.clone(),
        &prompt.body,
        req.correctness_control,
        &mut rx,
    )
    .await;

    // Optional confirmation pass at temperature 0.6 (§2.1): a
    // separate record — the temperature differs, so the
    // comparability key differs — over the same arms.
    if req.confirm_at_06 && req.temperature == 0.0 && !cancelled {
        let mut confirm_record = record.clone();
        confirm_record.id = format!("{sweep_id}-confirm");
        confirm_record.temperature = mb::CONFIRM_TEMPERATURE;
        confirm_record.status = "running".to_string();
        confirm_record.started_ms = now_ms();
        confirm_record.ended_ms = None;
        confirm_record.partial = false;
        for arm in confirm_record.arms.iter_mut() {
            arm.status = mb::ArmStatus::Pending;
            arm.detail = None;
            arm.runs = Vec::new();
            arm.summary = None;
            arm.peak_vram_mib = None;
            arm.started_ms = None;
            arm.ended_ms = None;
            arm.raw_flags = String::new();
        }
        save_sweep(state, &confirm_record);
        if let Ok(mut slot) = state.bench_job.try_lock() {
            if let Some(job) = slot.as_mut() {
                job.sweep_id = confirm_record.id.clone();
            }
        }
        let confirm_ctx = SweepCtx {
            cfg: cfg.clone(),
            target: target.clone(),
            model_id: ctx.model_id.clone(),
            temperature: mb::CONFIRM_TEMPERATURE,
            draft_path: draft_path.clone(),
            draft_device_arg: draft_device_arg.clone(),
        };
        let confirm_cancelled = execute_sweep(
            state,
            &confirm_ctx,
            &mut confirm_record,
            outcomes,
            &prompt.body,
            false,
            &mut rx,
        )
        .await;
        cancelled = cancelled || confirm_cancelled;
    }

    {
        let mut slot = state.bench_abort.lock().await;
        *slot = None;
    }
    {
        let mut slot = state.bench_job.lock().await;
        *slot = None;
    }
    if cancelled {
        state
            .finish_task(
                &task_id,
                TaskStatus::Aborted,
                json!({ "sweep_id": sweep_id, "cancelled": true }),
            )
            .await;
        return Err(ApiError::InvalidInput(
            "bench sweep cancelled".to_string(),
        ));
    }
    state
        .finish_task(
            &task_id,
            TaskStatus::Done,
            json!({ "sweep_id": sweep_id }),
        )
        .await;
    let results = load_results(state);
    Ok(results_payload(state, &results))
}

pub async fn op_bench_run_cancel(
    state: &Arc<AppState>,
    _sink: &Arc<dyn crate::EventSink>,
    sweep_id: String,
) -> crate::ApiResult {
    let matches = {
        let slot = state.bench_job.lock().await;
        slot.as_ref().map(|j| j.sweep_id.clone())
    };
    let current = match matches {
        Some(id) => id,
        None => return Ok(json!({ "cancelled": false })),
    };
    // The confirmation record runs under "<id>-confirm" in the
    // same job; cancelling either id cancels the job.
    if sweep_id != current && sweep_id != current.trim_end_matches("-confirm") {
        return Ok(json!({ "cancelled": false }));
    }
    let sender = {
        let mut slot = state.bench_abort.lock().await;
        slot.take()
    };
    match sender {
        Some(tx) => {
            let _ = tx.send(());
            Ok(json!({ "cancelled": true }))
        }
        None => Ok(json!({ "cancelled": false })),
    }
}

pub async fn op_bench_results_list(
    state: &Arc<AppState>,
    _sink: &Arc<dyn crate::EventSink>,
) -> crate::ApiResult {
    let results = load_results(state);
    Ok(results_payload(state, &results))
}

pub async fn op_bench_result_delete(
    state: &Arc<AppState>,
    _sink: &Arc<dyn crate::EventSink>,
    sweep_id: String,
) -> crate::ApiResult {
    if let Some(job) = state.bench_job.lock().await.as_ref() {
        if job.sweep_id == sweep_id {
            return Err(ApiError::InvalidInput(
                "that sweep is running; cancel it before deleting".to_string(),
            ));
        }
    }
    let mut results = load_results(state);
    let before = results.sweeps.len();
    results.sweeps.retain(|s| s.id != sweep_id);
    if results.sweeps.len() == before {
        return Err(ApiError::NotFound(format!("no sweep '{sweep_id}'")));
    }
    save_results(state, &results);
    Ok(results_payload(state, &results))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(raw_flags: Option<&str>) -> BackendConfig {
        BackendConfig {
            id: "bench-target".to_string(),
            enabled: true,
            model_file: PathBuf::from("models/target.gguf"),
            port: 8090,
            flags: manager_config::ServerFlags {
                n_ctx: 65536,
                n_batch: 512,
                tensor_split: Vec::new(),
                split_mode: manager_config::SplitMode::None,
            },
            raw_flags: raw_flags.map(|s| s.to_string()),
            flags_advanced: false,
            restart_policy: manager_config::RestartPolicy {
                max_retries: 5,
                backoff_base_secs: 2,
            },
            overrides_global: false,
            tool_call_probe: None,
            purpose: String::new(),
        }
    }

    #[test]
    fn base_arm_captures_the_targets_offload_position() {
        // The coder pattern: full expert offload is recorded on
        // the base arm, so every arm snapshot names the position
        // it runs at.
        let spec = base_arm_spec(&backend(Some("-ot exps=CPU --parallel 1")));
        assert_eq!(spec.offload_expr.as_deref(), Some("-ot exps=CPU"));
        assert_eq!(spec.n_ctx, 65536);
        // The partial pattern records its layer count.
        let spec = base_arm_spec(&backend(Some("--n-cpu-moe 24")));
        assert_eq!(spec.offload_expr.as_deref(), Some("--n-cpu-moe 24"));
    }

    #[test]
    fn base_arm_canonicalizes_offload_spellings() {
        // Every spelling the decoder recognizes lands on the one
        // canonical expression for its position.
        let spec = base_arm_spec(&backend(Some("--cpu-moe")));
        assert_eq!(spec.offload_expr.as_deref(), Some("-ot exps=CPU"));
        let spec = base_arm_spec(&backend(Some("-ncmoe=46")));
        assert_eq!(spec.offload_expr.as_deref(), Some("--n-cpu-moe 46"));
    }

    #[test]
    fn base_arm_without_offload_carries_no_expression() {
        let spec = base_arm_spec(&backend(Some("--parallel 1")));
        assert_eq!(spec.offload_expr, None);
        let spec = base_arm_spec(&backend(None));
        assert_eq!(spec.offload_expr, None);
        // A non-expert `-ot` assignment is not expert offload.
        let spec = base_arm_spec(&backend(Some("-ot blk.0.attn=CPU")));
        assert_eq!(spec.offload_expr, None);
    }

    fn placed_backend(id: &str, tensor_split: Vec<f32>, raw_flags: Option<&str>) -> BackendConfig {
        let mut b = backend(raw_flags);
        b.id = id.to_string();
        b.flags.tensor_split = tensor_split;
        b
    }

    fn cfg_with(backends: Vec<BackendConfig>) -> AppConfig {
        let mut cfg = AppConfig::default_config();
        cfg.backends = backends;
        cfg
    }

    fn running(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn interlock_cross_gpu_raw_placed_backend_does_not_conflict() {
        // Pete's case (Oct 23, 2026): the target is resident
        // entirely on GPU 0; coder runs on GPU 1 only, its
        // placement stated in raw flags, and it CPU-offloads its
        // experts. Neither rule binds: the GPUs are disjoint, and
        // the CPU-offload rule binds only when the TARGET
        // CPU-offloads (§6.2). Before the bench-local placement
        // reading, backend_gpu_visibility reported coder
        // Undetermined and the interlock flagged it against every
        // sweep.
        let target = placed_backend("planner", vec![1.0], Some("--parallel 1"));
        let coder = placed_backend(
            "coder",
            Vec::new(),
            Some("--tensor-split 0,1 -ot exps=CPU"),
        );
        let cfg = cfg_with(vec![target.clone(), coder]);
        let conflicts = interlock_conflicts(&cfg, &running(&["coder"]), &target);
        assert!(conflicts.is_empty(), "conflicts: {conflicts:?}");
    }

    #[test]
    fn interlock_shared_gpu_and_running_target_conflict() {
        // An unsplit backend sits on GPU 0 by default and shares
        // it with a GPU-0 target; the running target itself
        // always conflicts (§6.1).
        let target = placed_backend("planner", Vec::new(), None);
        let other = placed_backend("chat", Vec::new(), None);
        let cfg = cfg_with(vec![target.clone(), other]);
        let conflicts = interlock_conflicts(&cfg, &running(&["chat", "planner"]), &target);
        assert_eq!(conflicts, vec!["planner".to_string(), "chat".to_string()]);
    }

    #[test]
    fn interlock_cpu_offload_target_conflicts_with_cpu_offload_backend() {
        // Disjoint GPUs, but the target CPU-offloads, so a
        // running CPU-offload backend shares the CPU expert
        // resource and conflicts (§3.4/§6.2) — while a running
        // dense backend on the other GPU does not.
        let target = placed_backend(
            "coder-target",
            vec![1.0, 0.0],
            Some("-ot exps=CPU"),
        );
        let moe = placed_backend("planner-moe", vec![0.0, 1.0], Some("--n-cpu-moe 24"));
        let dense = placed_backend("planner", vec![0.0, 1.0], Some("--parallel 1"));
        let cfg = cfg_with(vec![target.clone(), moe, dense]);
        let conflicts =
            interlock_conflicts(&cfg, &running(&["planner-moe", "planner"]), &target);
        assert_eq!(conflicts, vec!["planner-moe".to_string()]);
    }

    #[test]
    fn interlock_unparseable_placement_still_conflicts() {
        // A device selection that does not parse stays
        // Undetermined, and Undetermined conflicts with
        // everything — the conservative reading is kept for the
        // genuinely unknown case.
        let target = placed_backend("planner", vec![1.0], None);
        let odd = placed_backend("odd", Vec::new(), Some("--device banana"));
        let cfg = cfg_with(vec![target.clone(), odd]);
        let conflicts = interlock_conflicts(&cfg, &running(&["odd"]), &target);
        assert_eq!(conflicts, vec!["odd".to_string()]);
    }
}
