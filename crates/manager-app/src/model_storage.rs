//! Tiered model storage orchestration (docs/tiered-model-storage.md):
//! the inventory of tierable files, pins, in-use detection, the single
//! background mover, the reconcile loop, and the operations.
//!
//! The pure pieces live elsewhere: the resolver and the configuration in
//! `manager_config::storage`, the verified copy / state file / policy in
//! `manager_models::tier`. Promotion copies cold → hot and leaves the
//! cold copy as an archive; demotion deletes the hot copy when the cold
//! archive is intact, and otherwise copies hot → cold (verified) first.
//! At most one move runs at a time, in a background-priority thread
//! that parks while any backend is loading.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use manager_config::{
    path_key, resolve_model_path, tier_file, tier_relative, tier_root, AppConfig,
    ModelStorageConfig, Residency, Tier,
};
use manager_models::tier::{
    copy_verified, free_bytes, plan, same_volume, sweep_partials, total_bytes, ArchiveRecord,
    CopyControl, CopyOptions, FileFacts, MoveKind, MoveOutcome, MoveRecord, PinReason, PlanInputs,
    PlannedMove, SkippedMove, StorageState, TierError, STATE_FILE,
};

use crate::{
    ensure_backend_specs, is_real_pin, mutate_save_install, ApiError, ApiResult, AppState,
    DownloadOutcome, EventSink, TaskStatus,
};

/// Reconcile-loop cadence; the policy is evaluated every
/// [`POLICY_EVERY_TICKS`] ticks (and after manual requests).
pub const TICK_SECS: u64 = 5;
const POLICY_EVERY_TICKS: u64 = 12;
/// Ticks to wait after app start before the first policy pass, so
/// startup (adoption, autostarts) never competes with a copy.
const POLICY_START_DELAY_TICKS: u64 = 12;

/// The runtime half of the feature, owned by [`AppState`].
#[derive(Default)]
pub(crate) struct StorageRuntime {
    state: std::sync::Mutex<Option<StorageState>>,
    active: std::sync::Mutex<Option<ActiveMove>>,
    queue: std::sync::Mutex<VecDeque<MoveRequest>>,
    last_plan: std::sync::Mutex<(Vec<PlannedMove>, Vec<SkippedMove>)>,
    /// Skipped-move alerts already raised (logical|kind|reason).
    alerted: std::sync::Mutex<HashSet<String>>,
    /// Paths each spawned process opened (`-m`, draft), by process id.
    opened: std::sync::Mutex<HashMap<String, Vec<PathBuf>>>,
    ticks: AtomicU64,
    policy_dirty: AtomicBool,
    seq: AtomicU64,
}

#[derive(Clone)]
struct ActiveMove {
    id: String,
    task_id: String,
    logical: PathBuf,
    kind: MoveKind,
    to: Tier,
    ctl: Arc<CopyControl>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MoveRequest {
    logical: PathBuf,
    kind: MoveKind,
    reason: String,
}

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join(STATE_FILE)
}

// ---------------------------------------------------------------------------
// State file access
// ---------------------------------------------------------------------------

/// Run `f` against the loaded state (loading it on first use, which
/// also performs crash recovery: running moves become interrupted).
fn with_state<R>(state: &AppState, f: impl FnOnce(&mut StorageState) -> R) -> R {
    let mut slot = lock(&state.model_storage.state);
    if slot.is_none() {
        let path = state_path(&state.data_dir);
        let (mut loaded, err) = StorageState::load(&path);
        if let Some(e) = err {
            tracing::warn!(target: "manager_app::model_storage", error = %e, "model-storage.json unreadable; starting empty");
        }
        let n = loaded.mark_interrupted(Utc::now());
        if n > 0 {
            tracing::warn!(target: "manager_app::model_storage", count = n, "moves interrupted by the previous app exit");
            let _ = loaded.save(&path);
        }
        *slot = Some(loaded);
    }
    f(slot.as_mut().expect("loaded above"))
}

/// [`with_state`] then persist.
fn mutate_state<R>(state: &AppState, f: impl FnOnce(&mut StorageState) -> R) -> R {
    let path = state_path(&state.data_dir);
    with_state(state, |s| {
        let r = f(s);
        if let Err(e) = s.save(&path) {
            tracing::warn!(target: "manager_app::model_storage", error = %e, "could not save model-storage.json");
        }
        r
    })
}

// ---------------------------------------------------------------------------
// Startup, start recording
// ---------------------------------------------------------------------------

/// App start: load the state file (crash recovery) and sweep leftover
/// `.partial` copies from every reachable tier.
pub async fn startup(state: &AppState) {
    let cfg = state.config.read().await.clone();
    with_state(state, |_| ());
    for t in manager_config::all_tiers(&cfg.model_storage) {
        if let Some(root) = tier_root(&cfg.data_dir, &cfg.model_storage, t) {
            for p in sweep_partials(&root) {
                tracing::info!(target: "manager_app::model_storage", path = %p.display(), "removed an interrupted tier copy");
            }
        }
    }
}

/// The logical path of a file inside some tier root (`models\X.gguf`).
pub(crate) fn logical_for_path(cfg: &AppConfig, abs: &Path) -> Option<PathBuf> {
    let key = path_key(abs);
    for t in manager_config::all_tiers(&cfg.model_storage) {
        let root = tier_root(&cfg.data_dir, &cfg.model_storage, t)?;
        let rk = format!("{}\\", path_key(&root).trim_end_matches('\\'));
        if let Some(rest) = key.strip_prefix(&rk) {
            // Rebuild from the original path to keep its casing.
            let tail = abs.to_string_lossy();
            let tail = &tail[tail.len() - rest.len()..];
            return Some(manager_config::logical_from_relative(Path::new(tail)));
        }
    }
    None
}

/// Model paths a process spec opens: `-m`/`--model` and the draft
/// model flags, in `flag value` or `flag=value` form.
fn spec_model_paths(args: &[String]) -> Vec<PathBuf> {
    const FLAGS: [&str; 5] = [
        "-m",
        "--model",
        "-md",
        "--model-draft",
        "--spec-draft-model",
    ];
    let mut out = Vec::new();
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        if FLAGS.contains(&a.as_str()) {
            if let Some(v) = it.peek() {
                out.push(PathBuf::from(v.as_str()));
            }
        } else if let Some((f, v)) = a.split_once('=') {
            if FLAGS.contains(&f) {
                out.push(PathBuf::from(v));
            }
        }
    }
    out
}

/// Record a successful spawn of supervised process `id`: what it opened
/// (for in-use detection) and a start of its model (for LRU and the
/// N-starts promotion rule). Called by every app-mediated spawn path.
pub(crate) async fn note_process_start(state: &AppState, id: &str) {
    let cfg = state.config.read().await.clone();
    let args = {
        let sup = state.supervisor().await.lock().await;
        sup.spec(id).map(|s| s.args.clone()).unwrap_or_default()
    };
    let paths = spec_model_paths(&args);
    let main = args
        .iter()
        .position(|a| a == "-m" || a == "--model")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    lock(&state.model_storage.opened).insert(id.to_string(), paths);
    if let Some(m) = main {
        if let Some(logical) = logical_for_path(&cfg, &m) {
            let tier = if path_key(&m)
                .starts_with(&path_key(&cfg.data_dir.join(manager_config::MODELS_DIR)))
            {
                "hot"
            } else {
                "cold"
            };
            tracing::info!(target: "manager_app::model_storage", process = id, model = %m.display(), tier, "model start recorded");
            let window = cfg.model_storage.promote_window_days;
            mutate_state(state, |s| s.record_start(&logical, Utc::now(), window));
            state
                .model_storage
                .policy_dirty
                .store(true, Ordering::SeqCst);
        }
    }
}

// ---------------------------------------------------------------------------
// Inventory
// ---------------------------------------------------------------------------

/// One tierable file as the storage view sees it.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) logical: PathBuf,
    pub(crate) rel: PathBuf,
    pub(crate) model_id: Option<String>,
    pub(crate) pin_sha: Option<String>,
    pub(crate) residency: Residency,
    pub(crate) present_in: Vec<Tier>,
    /// Bytes per tier copy.
    pub(crate) sizes: HashMap<Tier, u64>,
    pub(crate) archived: bool,
    pub(crate) pin: Option<PinReason>,
}

impl Entry {
    fn size(&self) -> u64 {
        self.sizes.values().copied().max().unwrap_or(0)
    }
    fn hot(&self) -> bool {
        self.present_in.contains(&Tier::Hot)
    }
}

fn cold_available(cfg: &AppConfig) -> bool {
    cfg.model_storage
        .cold_tiers
        .first()
        .map(|c| c.path.is_dir())
        .unwrap_or(false)
}

/// Why `logical` must stay hot, if it must.
pub(crate) fn pin_for(
    cfg: &AppConfig,
    model_id: Option<&str>,
    logical: &Path,
) -> Option<PinReason> {
    let key = path_key(logical);
    let rel_key = tier_relative(logical).map(|r| path_key(&r));
    for b in cfg.backends.iter().filter(|b| b.enabled) {
        let same = path_key(&b.model_file) == key
            || (tier_relative(&b.model_file).map(|r| path_key(&r)) == rel_key && rel_key.is_some());
        if same {
            return Some(PinReason::Backend(b.id.clone()));
        }
    }
    let hot_abs = path_key(&manager_config::resolve_under(&cfg.data_dir, logical));
    for b in cfg.backends.iter().filter(|b| b.enabled) {
        if let Some(raw) = &b.raw_flags {
            let raw_key = raw.replace('/', "\\").to_lowercase();
            if raw_key.contains(&hot_abs) {
                return Some(PinReason::RawFlags(b.id.clone()));
            }
        }
    }
    if manager_config::keep_hot_matches(&cfg.model_storage, model_id, logical) {
        return Some(PinReason::KeepHot);
    }
    None
}

/// Every tierable file: registered models with a tierable path, plus
/// any `*.gguf` directly in a reachable tier root (unregistered files).
pub(crate) fn inventory(cfg: &AppConfig, st: &StorageState) -> Vec<Entry> {
    let storage = &cfg.model_storage;
    let mut seen: HashSet<String> = HashSet::new();
    let mut candidates: Vec<(PathBuf, Option<&manager_config::ModelConfig>)> = Vec::new();
    for m in &cfg.models {
        if tier_relative(&m.gguf_path).is_some() && seen.insert(path_key(&m.gguf_path)) {
            candidates.push((m.gguf_path.clone(), Some(m)));
        }
    }
    for t in manager_config::all_tiers(storage) {
        let Some(root) = tier_root(&cfg.data_dir, storage, t) else {
            continue;
        };
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut names: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension()
                        .map(|x| x.eq_ignore_ascii_case("gguf"))
                        .unwrap_or(false)
            })
            .filter_map(|p| p.file_name().map(PathBuf::from))
            .collect();
        names.sort();
        for n in names {
            let logical = manager_config::logical_from_relative(&n);
            if seen.insert(path_key(&logical)) {
                candidates.push((logical, None));
            }
        }
    }
    candidates
        .into_iter()
        .filter_map(|(logical, model)| {
            let rel = tier_relative(&logical)?;
            let r = resolve_model_path(&cfg.data_dir, storage, &logical);
            let mut sizes = HashMap::new();
            for t in &r.present_in {
                if let Some(p) = tier_file(&cfg.data_dir, storage, *t, &rel) {
                    if let Ok(md) = std::fs::metadata(&p) {
                        sizes.insert(*t, md.len());
                    }
                }
            }
            let pin_sha = model.map(|m| m.sha256.clone()).filter(|s| is_real_pin(s));
            let archived = r
                .present_in
                .iter()
                .filter_map(|t| match t {
                    Tier::Cold(_) => tier_file(&cfg.data_dir, storage, *t, &rel),
                    Tier::Hot => None,
                })
                .any(|p| st.valid_archive(&p, pin_sha.as_deref()).is_some());
            let model_id = model.map(|m| m.id.clone());
            let pin = pin_for(cfg, model_id.as_deref(), &logical);
            Some(Entry {
                logical,
                rel,
                model_id,
                pin_sha,
                residency: r.residency,
                present_in: r.present_in,
                sizes,
                archived,
                pin,
            })
        })
        .collect()
}

/// Path keys of every model file a live process holds open: recorded
/// spawn paths plus the current spec of every live process (adopted
/// processes included). Conservative by design; the delete-time
/// sharing violation is the backstop.
async fn in_use_keys(state: &AppState) -> HashSet<String> {
    let mut keys = HashSet::new();
    let mut sup = state.supervisor().await.lock().await;
    sup.reap_exited();
    let live: Vec<String> = sup
        .status()
        .into_iter()
        .filter(|s| s.pid.is_some())
        .map(|s| s.id)
        .collect();
    let opened = lock(&state.model_storage.opened);
    for id in &live {
        if let Some(spec) = sup.spec(id) {
            for p in spec_model_paths(&spec.args) {
                keys.insert(path_key(&p));
            }
        }
        for p in opened.get(id).into_iter().flatten() {
            keys.insert(path_key(p));
        }
    }
    keys
}

/// True while any backend is loading: a lifecycle transition in
/// flight, a supervised process in Starting, or a backend whose live
/// pid has not answered `/health` yet (bounded to its first 10 min so
/// a wedged backend cannot park copies forever).
async fn any_backend_loading(state: &AppState) -> bool {
    let records = state.lifecycle.records();
    if records
        .iter()
        .any(|(k, r)| crate::lifecycle::backend_id_of(k).is_some() && r.transition.is_some())
    {
        return true;
    }
    let sup = state.supervisor().await.lock().await;
    sup.status().into_iter().any(|s| {
        if s.state == manager_supervisor::ProcessState::Starting {
            return true;
        }
        let young = s
            .uptime
            .map(|u| u < std::time::Duration::from_secs(600))
            .unwrap_or(false);
        let key = crate::lifecycle::backend_key(&s.id);
        match (s.pid, records.get(&key)) {
            (Some(pid), Some(r)) => young && r.ready_pid != Some(pid),
            _ => false,
        }
    })
}

/// Model ids with a download or verification in flight.
async fn busy_model_ids(state: &AppState) -> HashSet<String> {
    let mut ids: HashSet<String> = state
        .downloads
        .handles
        .lock()
        .await
        .keys()
        .cloned()
        .collect();
    ids.extend(lock(&state.downloads.verifying).iter().cloned());
    ids
}

fn active_logical_key(state: &AppState) -> Option<String> {
    lock(&state.model_storage.active)
        .as_ref()
        .map(|a| path_key(&a.logical))
}

/// Whether a move is running or queued for `logical` (downloads and
/// verification refuse while one is).
pub(crate) fn move_pending_for(state: &AppState, logical: &Path) -> bool {
    let key = path_key(logical);
    active_logical_key(state).as_deref() == Some(key.as_str())
        || lock(&state.model_storage.queue)
            .iter()
            .any(|q| path_key(&q.logical) == key)
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

async fn compute_plan(state: &AppState, cfg: &AppConfig) -> (Vec<PlannedMove>, Vec<SkippedMove>) {
    let now = Utc::now();
    let in_use = in_use_keys(state).await;
    let busy = busy_model_ids(state).await;
    let active = active_logical_key(state);
    let (entries, facts) = with_state(state, |st| {
        let entries = inventory(cfg, st);
        // Idle demotion's clock starts the first time a hot file is seen.
        for e in entries.iter().filter(|e| e.hot()) {
            st.touch_first_seen(&e.logical, now);
        }
        let facts: Vec<FileFacts> = entries
            .iter()
            .map(|e| {
                let paths: Vec<PathBuf> = e
                    .present_in
                    .iter()
                    .filter_map(|t| tier_file(&cfg.data_dir, &cfg.model_storage, *t, &e.rel))
                    .collect();
                let usage = st.usage(&e.logical);
                FileFacts {
                    logical: e.logical.clone(),
                    model_id: e.model_id.clone(),
                    size: e.size(),
                    hot: e.hot(),
                    cold: e.present_in.contains(&Tier::Cold(0)),
                    archived: e.archived,
                    pin: e.pin.clone(),
                    in_use: paths.iter().any(|p| in_use.contains(&path_key(p))),
                    busy: e
                        .model_id
                        .as_ref()
                        .map(|id| busy.contains(id))
                        .unwrap_or(false)
                        || active.as_deref() == Some(path_key(&e.logical).as_str()),
                    last_used: usage.and_then(|u| u.last_used()),
                    recent_starts: st.recent_starts(
                        &e.logical,
                        now,
                        cfg.model_storage.promote_window_days,
                    ),
                }
            })
            .collect();
        (entries, facts)
    });
    let _ = with_state(state, |st| st.save(&state_path(&state.data_dir)));
    drop(entries);
    let hot_root = cfg.data_dir.join(manager_config::MODELS_DIR);
    let cold_root = cfg.model_storage.cold_tiers.first().map(|c| c.path.clone());
    plan(&PlanInputs {
        files: &facts,
        storage: &cfg.model_storage,
        now,
        hot_free_bytes: free_bytes(&hot_root).or_else(|| free_bytes(&cfg.data_dir)),
        cold_free_bytes: cold_root.as_deref().and_then(free_bytes),
        cold_available: cold_available(cfg),
    })
}

/// One reconcile tick: keep the active copy's pause flag current, start
/// the next queued manual move, or (on policy ticks) the policy's next
/// move. Called every [`TICK_SECS`] by the app's background loop.
pub async fn tick(state: &Arc<AppState>, sink: &Arc<dyn EventSink>) {
    let n = state.model_storage.ticks.fetch_add(1, Ordering::SeqCst);
    let loading = any_backend_loading(state).await;
    if let Some(a) = lock(&state.model_storage.active).as_ref() {
        let was = a.ctl.pause.swap(loading, Ordering::SeqCst);
        if was != loading {
            tracing::info!(target: "manager_app::model_storage", move_id = %a.id, paused = loading, "copy {}", if loading { "paused: a backend is loading" } else { "resumed" });
        }
        return;
    }
    let next = lock(&state.model_storage.queue).pop_front();
    if let Some(req) = next {
        if let Err(e) = begin_move(state, sink, req.clone()).await {
            tracing::warn!(target: "manager_app::model_storage", logical = %req.logical.display(), error = %e, "queued move refused");
        }
        return;
    }
    let due = n >= POLICY_START_DELAY_TICKS
        && (n.is_multiple_of(POLICY_EVERY_TICKS)
            || state
                .model_storage
                .policy_dirty
                .swap(false, Ordering::SeqCst));
    if !due {
        return;
    }
    let cfg = state.config.read().await.clone();
    let (moves, skipped) = compute_plan(state, &cfg).await;
    raise_skipped_alerts(state, &skipped);
    *lock(&state.model_storage.last_plan) = (moves.clone(), skipped);
    if loading {
        return;
    }
    if let Some(m) = moves.into_iter().next() {
        let req = MoveRequest {
            logical: m.logical,
            kind: m.kind,
            reason: m.reason,
        };
        if let Err(e) = begin_move(state, sink, req.clone()).await {
            tracing::warn!(target: "manager_app::model_storage", logical = %req.logical.display(), error = %e, "policy move refused");
        }
    }
}

fn raise_skipped_alerts(state: &AppState, skipped: &[SkippedMove]) {
    let mut alerted = lock(&state.model_storage.alerted);
    let current: HashSet<String> = skipped
        .iter()
        .map(|s| format!("{}|{}|{}", path_key(&s.logical), s.kind.as_str(), s.reason))
        .collect();
    for s in skipped {
        let key = format!("{}|{}|{}", path_key(&s.logical), s.kind.as_str(), s.reason);
        if alerted.insert(key) {
            let title = match s.reason.as_str() {
                "hot tier full" => "Hot model tier full",
                "cold tier full" => "Cold model tier full",
                _ => "Model move skipped",
            };
            crate::event_bus::publish_alert(
                state,
                "model-storage",
                title,
                &format!("{} {}: {}", s.kind.as_str(), s.logical.display(), s.reason),
            );
        }
    }
    alerted.retain(|k| current.contains(k));
}

// ---------------------------------------------------------------------------
// The mover
// ---------------------------------------------------------------------------

/// Validate and start one move. Instant demotions finish inline; copies
/// run on a dedicated thread and finish in a watcher task.
async fn begin_move(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    req: MoveRequest,
) -> Result<String, ApiError> {
    let _ = sink;
    let cfg = state.config.read().await.clone();
    let storage = &cfg.model_storage;
    if storage.cold_tiers.is_empty() {
        return Err(ApiError::InvalidInput(
            "no cold tier is configured (Settings → Model storage)".to_string(),
        ));
    }
    if !cold_available(&cfg) {
        return Err(ApiError::InvalidInput(format!(
            "cold tier {} is not available",
            storage.cold_tiers[0].path.display()
        )));
    }
    if lock(&state.model_storage.active).is_some() {
        return Err(ApiError::Busy("another model move is running".to_string()));
    }
    let entries = with_state(state, |st| inventory(&cfg, st));
    let key = path_key(&req.logical);
    let e = entries
        .into_iter()
        .find(|e| path_key(&e.logical) == key)
        .ok_or_else(|| ApiError::NotFound(format!("no tierable file {}", req.logical.display())))?;
    if let Some(id) = &e.model_id {
        if busy_model_ids(state).await.contains(id) {
            return Err(ApiError::Busy(format!(
                "a download or verification is in flight for model {id}"
            )));
        }
    }
    let cold = Tier::Cold(0);
    let hot_path = tier_file(&cfg.data_dir, storage, Tier::Hot, &e.rel).expect("hot tier");
    let cold_path = tier_file(&cfg.data_dir, storage, cold, &e.rel).expect("cold tier 0");
    let (src_tier, dst_tier) = match req.kind {
        MoveKind::Promote => {
            if e.hot() {
                return Err(ApiError::InvalidInput(format!(
                    "{} is already in the hot tier",
                    e.logical.display()
                )));
            }
            if !e.present_in.contains(&cold) {
                return Err(ApiError::NotFound(format!(
                    "{} is not in the cold tier",
                    e.logical.display()
                )));
            }
            (cold, Tier::Hot)
        }
        MoveKind::Demote => {
            if !e.hot() {
                return Err(ApiError::InvalidInput(format!(
                    "{} is not in the hot tier",
                    e.logical.display()
                )));
            }
            let in_use = in_use_keys(state).await;
            if in_use.contains(&path_key(&hot_path)) {
                return Err(ApiError::Busy(format!(
                    "{} is open by a running process; stop it before demoting",
                    e.logical.display()
                )));
            }
            (Tier::Hot, cold)
        }
    };
    let src = if src_tier == Tier::Hot {
        &hot_path
    } else {
        &cold_path
    };
    let dst = if dst_tier == Tier::Hot {
        &hot_path
    } else {
        &cold_path
    };
    let size = e.size();
    let instant = req.kind == MoveKind::Demote && e.archived;
    if !instant {
        let dst_root = tier_root(&cfg.data_dir, storage, dst_tier).expect("tier root");
        let _ = std::fs::create_dir_all(&dst_root);
        if let Some(free) = free_bytes(&dst_root) {
            let needed = size + manager_models::tier::COPY_HEADROOM_BYTES;
            if free < needed {
                return Err(ApiError::Models(
                    TierError::NoSpace { needed, free }.to_string(),
                ));
            }
        }
    }

    let id = format!(
        "mv-{}-{}",
        Utc::now().timestamp_millis(),
        state.model_storage.seq.fetch_add(1, Ordering::SeqCst)
    );
    let task_kind = match req.kind {
        MoveKind::Promote => "model-promote",
        MoveKind::Demote => "model-demote",
    };
    let task_id = state
        .record_task(
            task_kind,
            json!({ "file": e.logical, "model_id": e.model_id, "reason": req.reason, "move_id": id }),
        )
        .await;
    let record = MoveRecord {
        id: id.clone(),
        logical: e.logical.clone(),
        kind: req.kind.as_str().to_string(),
        from: src_tier.as_wire(),
        to: dst_tier.as_wire(),
        reason: req.reason.clone(),
        started_at: Utc::now(),
        finished_at: None,
        outcome: MoveOutcome::Running,
        error: None,
        bytes: size,
        instant,
    };
    mutate_state(state, |s| s.push_move(record));
    tracing::info!(target: "manager_app::model_storage", task_id = %task_id, move_id = %id, kind = req.kind.as_str(), file = %e.logical.display(), reason = %req.reason, instant, "model move started");

    let ctl = Arc::new(CopyControl::default());
    ctl.total_bytes.store(size, Ordering::SeqCst);
    *lock(&state.model_storage.active) = Some(ActiveMove {
        id: id.clone(),
        task_id: task_id.clone(),
        logical: e.logical.clone(),
        kind: req.kind,
        to: dst_tier,
        ctl: Arc::clone(&ctl),
    });

    if instant {
        let result = finish_demotion(state, &cfg, &e, &hot_path, None).await;
        settle(state, &e, result).await;
        return Ok(task_id);
    }

    let opts = CopyOptions {
        rate_bytes_per_sec: u64::from(storage.copy_rate_mib_s) * 1024 * 1024,
        background_io: true,
    };
    let (src, dst) = (src.clone(), dst.clone());
    let expected = e.pin_sha.clone();
    let worker_ctl = Arc::clone(&ctl);
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("model-tier-copy".to_string())
        .spawn(move || {
            let r = copy_verified(&src, &dst, expected.as_deref(), opts, &worker_ctl);
            let _ = tx.send(r);
        })
        .map_err(|e| ApiError::Io(format!("could not start the copy thread: {e}")))?;
    let watcher_state = Arc::clone(state);
    tokio::spawn(async move {
        let state = watcher_state;
        let copied = rx
            .await
            .unwrap_or_else(|_| Err(TierError::Io("copy thread exited".to_string())));
        let cfg = state.config.read().await.clone();
        let result = match (req.kind, copied) {
            (_, Err(err)) => Err(err),
            (MoveKind::Promote, Ok(out)) => {
                finish_promotion(&state, &cfg, &e, &cold_path, out).await
            }
            (MoveKind::Demote, Ok(out)) => {
                finish_demotion(&state, &cfg, &e, &hot_path, Some((cold_path.clone(), out))).await
            }
        };
        settle(&state, &e, result).await;
    });
    Ok(task_id)
}

/// Re-stamp `verified_at` for a pinned model whose copy was just
/// verified against the pin.
async fn restamp_verified(state: &AppState, e: &Entry) {
    let (Some(id), Some(pin)) = (&e.model_id, &e.pin_sha) else {
        return;
    };
    let mut cfg = state.config.write().await;
    let r = mutate_save_install(&mut cfg, &state.config_path, |cfg| {
        if let Some(m) = cfg.models.iter_mut().find(|m| &m.id == id) {
            m.verified_at = Some(Utc::now());
            m.verified_sha256 = Some(pin.clone());
        }
    });
    if let Err(err) = r {
        tracing::warn!(target: "manager_app::model_storage", model_id = %id, error = %err, "could not re-stamp verified_at after a verified move");
    }
}

fn archive_record(path: &Path, sha256: &str) -> Option<ArchiveRecord> {
    let md = std::fs::metadata(path).ok()?;
    Some(ArchiveRecord {
        path: path.to_path_buf(),
        size: md.len(),
        mtime_ms: manager_models::tier::file_mtime_ms(&md)?,
        sha256: sha256.to_string(),
        verified_at: Utc::now(),
    })
}

async fn finish_promotion(
    state: &AppState,
    _cfg: &AppConfig,
    e: &Entry,
    cold_path: &Path,
    out: manager_models::tier::CopyOutcome,
) -> Result<(), TierError> {
    // The cold source's stream matched the verified copy: it is an
    // intact archive.
    if let Some(rec) = archive_record(cold_path, &out.sha256) {
        mutate_state(state, |s| s.set_archive(rec));
    }
    restamp_verified(state, e).await;
    Ok(())
}

async fn finish_demotion(
    state: &AppState,
    _cfg: &AppConfig,
    e: &Entry,
    hot_path: &Path,
    copied: Option<(PathBuf, manager_models::tier::CopyOutcome)>,
) -> Result<(), TierError> {
    if let Some((cold_path, out)) = &copied {
        if let Some(rec) = archive_record(cold_path, &out.sha256) {
            mutate_state(state, |s| s.set_archive(rec));
        }
        restamp_verified(state, e).await;
    }
    // Last in-use check right before the delete; the delete itself
    // fails on Windows if a process still maps the file.
    if in_use_keys(state).await.contains(&path_key(hot_path)) {
        return Err(TierError::Io(format!(
            "{} is open by a running process; the cold copy is kept and the next demotion is instant",
            hot_path.display()
        )));
    }
    std::fs::remove_file(hot_path).map_err(|err| {
        TierError::Io(format!(
            "could not delete the hot copy {} ({err}); the cold copy is kept",
            hot_path.display()
        ))
    })
}

/// Record the outcome of the active move, refresh launch specs so the
/// next spawn resolves to the new location, and settle the task.
async fn settle(state: &Arc<AppState>, e: &Entry, result: Result<(), TierError>) {
    let active = lock(&state.model_storage.active).take();
    let Some(active) = active else {
        return;
    };
    let now = Utc::now();
    let (outcome, error) = match &result {
        Ok(()) => (MoveOutcome::Done, None),
        Err(TierError::Cancelled) => (MoveOutcome::Cancelled, None),
        Err(err) => (MoveOutcome::Failed, Some(err.to_string())),
    };
    mutate_state(state, |s| {
        s.update_move(&active.id, |m| {
            m.outcome = outcome;
            m.finished_at = Some(now);
            m.error = error.clone();
        })
    });
    // A source that does not match its pin is a bad model file: gate
    // its start exactly as a failed Verify does.
    if let (Err(TierError::SourceMismatch { .. }), Some(id)) = (&result, &e.model_id) {
        {
            let mut cfg = state.config.write().await;
            let _ = mutate_save_install(&mut cfg, &state.config_path, |cfg| {
                if let Some(m) = cfg.models.iter_mut().find(|m| &m.id == id) {
                    m.verified_at = None;
                    m.verified_sha256 = None;
                }
            });
        }
        state.downloads.outcomes.lock().await.insert(
            id.clone(),
            DownloadOutcome {
                attempt: 0,
                failed: Some(
                    "SHA-256 verification failed during a storage move: the file does not match the pinned hash"
                        .to_string(),
                ),
                verification_failure: true,
            },
        );
    }
    // Launch specs (and service definitions) re-resolve now, so a
    // crash restart or service start uses where the file is now.
    ensure_backend_specs(state).await;
    let affected: Vec<String> = {
        let cfg = state.config.read().await;
        cfg.backends
            .iter()
            .filter(|b| path_key(&b.model_file) == path_key(&e.logical))
            .map(|b| b.id.clone())
            .collect()
    };
    for b in affected {
        state.sync_backend_service_definition(&b).await;
    }
    let status = match outcome {
        MoveOutcome::Done => TaskStatus::Done,
        MoveOutcome::Cancelled => TaskStatus::Aborted,
        _ => TaskStatus::Failed,
    };
    match &result {
        Ok(()) => {
            tracing::info!(target: "manager_app::model_storage", task_id = %active.task_id, move_id = %active.id, kind = active.kind.as_str(), file = %active.logical.display(), to = %active.to.as_wire(), "model move finished")
        }
        Err(err) => {
            tracing::warn!(target: "manager_app::model_storage", task_id = %active.task_id, move_id = %active.id, kind = active.kind.as_str(), file = %active.logical.display(), error = %err, "model move did not complete")
        }
    }
    if let Err(err) = &result {
        if !matches!(err, TierError::Cancelled) {
            crate::event_bus::publish_alert(
                state,
                "model-storage",
                "Model move failed",
                &format!(
                    "{} {}: {err}",
                    active.kind.as_str(),
                    active.logical.display()
                ),
            );
        }
    }
    state
        .finish_task(
            &active.task_id,
            status,
            json!({ "file": active.logical, "move_id": active.id, "error": error }),
        )
        .await;
    state
        .model_storage
        .policy_dirty
        .store(true, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// Read models for list_models / status
// ---------------------------------------------------------------------------

/// The `storage` object `list_models` attaches to a model row.
pub(crate) fn model_row_storage(
    state: &AppState,
    cfg: &AppConfig,
    m: &manager_config::ModelConfig,
) -> Value {
    let Some(rel) = tier_relative(&m.gguf_path) else {
        return Value::Null;
    };
    let r = resolve_model_path(&cfg.data_dir, &cfg.model_storage, &m.gguf_path);
    let pin_sha = Some(m.sha256.as_str()).filter(|s| is_real_pin(s));
    let archived = with_state(state, |st| {
        r.present_in.iter().any(|t| {
            matches!(t, Tier::Cold(_))
                && tier_file(&cfg.data_dir, &cfg.model_storage, *t, &rel)
                    .map(|p| st.valid_archive(&p, pin_sha).is_some())
                    .unwrap_or(false)
        })
    });
    let last_started = with_state(state, |st| {
        st.usage(&m.gguf_path).and_then(|u| u.last_started)
    });
    let pin = pin_for(cfg, Some(&m.id), &m.gguf_path);
    json!({
        "tier": residency_wire(r.residency),
        "path": r.path,
        "present_in": r.present_in.iter().map(|t| t.as_wire()).collect::<Vec<_>>(),
        "archived": archived,
        "cold_unavailable": r.cold_unavailable,
        "pinned": pin.as_ref().map(|p| p.describe()),
        "keep_hot": manager_config::keep_hot_matches(&cfg.model_storage, Some(&m.id), &m.gguf_path),
        "moving": moving_json(state, &m.gguf_path),
        "last_started": last_started,
    })
}

fn residency_wire(r: Residency) -> Value {
    match r {
        Residency::In(t) => json!(t.as_wire()),
        Residency::Missing => json!("missing"),
        Residency::NotTierable => Value::Null,
    }
}

fn moving_json(state: &AppState, logical: &Path) -> Value {
    let active = lock(&state.model_storage.active);
    match active.as_ref() {
        Some(a) if path_key(&a.logical) == path_key(logical) => json!({
            "kind": a.kind.as_str(),
            "to": a.to.as_wire(),
            "fraction": (a.ctl.fraction() * 1000.0).round() / 1000.0,
            "paused": a.ctl.pause.load(Ordering::SeqCst),
            "task_id": a.task_id,
        }),
        _ => {
            let queued = lock(&state.model_storage.queue)
                .iter()
                .find(|q| path_key(&q.logical) == path_key(logical))
                .map(|q| json!({ "kind": q.kind.as_str(), "queued": true }));
            queued.unwrap_or(Value::Null)
        }
    }
}

fn tier_json(cfg: &AppConfig, t: Tier, entries: &[Entry]) -> Value {
    let root = tier_root(&cfg.data_dir, &cfg.model_storage, t).unwrap_or_default();
    let available = root.is_dir();
    let probe = if available {
        root.clone()
    } else {
        cfg.data_dir.clone()
    };
    let files: Vec<Value> = entries
        .iter()
        .filter_map(|e| {
            e.sizes
                .get(&t)
                .map(|b| json!({ "file": e.logical, "model_id": e.model_id, "bytes": b }))
        })
        .collect();
    let used: u64 = entries.iter().filter_map(|e| e.sizes.get(&t)).sum();
    let label = match t {
        Tier::Hot => String::new(),
        Tier::Cold(i) => cfg.model_storage.cold_tiers[i].label.clone(),
    };
    json!({
        "tier": t.as_wire(),
        "path": root,
        "label": label,
        "available": available,
        "free_bytes": if available || t == Tier::Hot { free_bytes(&probe) } else { None },
        "total_bytes": if available || t == Tier::Hot { total_bytes(&probe) } else { None },
        "model_bytes": used,
        "files": files,
    })
}

/// `model_storage_status`: tiers, budget, every tierable file, the
/// active move, the policy's plan, and recent moves.
pub async fn op_model_storage_status(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let cfg = state.config.read().await.clone();
    let (entries, usage, moves) = with_state(state, |st| {
        let entries = inventory(&cfg, st);
        let usage: HashMap<String, Value> = entries
            .iter()
            .map(|e| {
                let u = st.usage(&e.logical);
                (
                    path_key(&e.logical),
                    json!({
                        "last_started": u.and_then(|u| u.last_started),
                        "first_seen": u.and_then(|u| u.first_seen),
                        "recent_starts": st.recent_starts(&e.logical, Utc::now(), cfg.model_storage.promote_window_days),
                    }),
                )
            })
            .collect();
        let moves: Vec<MoveRecord> = st.moves.iter().rev().take(20).cloned().collect();
        (entries, usage, moves)
    });
    let tiers: Vec<Value> = manager_config::all_tiers(&cfg.model_storage)
        .into_iter()
        .map(|t| tier_json(&cfg, t, &entries))
        .collect();
    let files: Vec<Value> = entries
        .iter()
        .map(|e| {
            let mut v = json!({
                "file": e.logical,
                "model_id": e.model_id,
                "registered": e.model_id.is_some(),
                "tier": residency_wire(e.residency),
                "present_in": e.present_in.iter().map(|t| t.as_wire()).collect::<Vec<_>>(),
                "bytes": e.size(),
                "archived": e.archived,
                "pinned": e.pin.as_ref().map(|p| p.describe()),
                "moving": moving_json(state, &e.logical),
            });
            if let Some(u) = usage.get(&path_key(&e.logical)) {
                v["usage"] = u.clone();
            }
            v
        })
        .collect();
    let (planned, skipped) = lock(&state.model_storage.last_plan).clone();
    Ok(json!({
        "config": cfg.model_storage,
        "tiers": tiers,
        "files": files,
        "planned": planned.iter().map(|m| json!({ "file": m.logical, "kind": m.kind.as_str(), "reason": m.reason })).collect::<Vec<_>>(),
        "skipped": skipped.iter().map(|m| json!({ "file": m.logical, "kind": m.kind.as_str(), "reason": m.reason })).collect::<Vec<_>>(),
        "moves": moves,
    }))
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// Map an operation target — a model id or a logical path — to the
/// logical path.
fn target_logical(cfg: &AppConfig, target: &str) -> Result<PathBuf, ApiError> {
    if let Some(m) = cfg.models.iter().find(|m| m.id == target) {
        return tier_relative(&m.gguf_path)
            .map(|_| m.gguf_path.clone())
            .ok_or_else(|| {
                ApiError::InvalidInput(format!(
                    "model {target}'s file {} is outside the tiered models directory",
                    m.gguf_path.display()
                ))
            });
    }
    // Logical paths are written `models\<file>.gguf` (the form the UI
    // and this error show) on every OS: accept either separator. A
    // leading separator stays as written, so an absolute path is still
    // refused by `tier_relative`.
    let t = target.trim();
    let p: PathBuf = if t.starts_with(['\\', '/']) {
        PathBuf::from(t)
    } else {
        t.split(['\\', '/']).filter(|s| !s.is_empty()).collect()
    };
    if tier_relative(&p).is_some() {
        return Ok(p);
    }
    Err(ApiError::NotFound(format!(
        "no model or tierable file {target:?} (use a model id or models\\<file>.gguf)"
    )))
}

async fn request_move(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    target: String,
    kind: MoveKind,
) -> ApiResult {
    let cfg = state.config.read().await.clone();
    let logical = target_logical(&cfg, &target)?;
    if kind == MoveKind::Demote {
        let model_id = cfg
            .models
            .iter()
            .find(|m| path_key(&m.gguf_path) == path_key(&logical))
            .map(|m| m.id.clone());
        if let Some(pin) = pin_for(&cfg, model_id.as_deref(), &logical) {
            return Err(ApiError::InvalidInput(format!(
                "{} must stay hot ({}); {} first",
                logical.display(),
                pin.describe(),
                match pin {
                    PinReason::KeepHot => "remove the keep-hot pin",
                    _ => "disable that backend",
                }
            )));
        }
    }
    if move_pending_for(state, &logical) {
        return Err(ApiError::Busy(format!(
            "a move is already running or queued for {}",
            logical.display()
        )));
    }
    let req = MoveRequest {
        logical: logical.clone(),
        kind,
        reason: "manual".to_string(),
    };
    if lock(&state.model_storage.active).is_some() {
        lock(&state.model_storage.queue).push_back(req);
        return Ok(json!({ "file": logical, "kind": kind.as_str(), "queued": true }));
    }
    let task_id = begin_move(state, sink, req).await?;
    Ok(json!({ "file": logical, "kind": kind.as_str(), "task_id": task_id }))
}

/// `model_storage_promote`: copy a cold file to the hot tier (the cold
/// copy stays as an archive). `target` is a model id or a logical path.
pub async fn op_model_storage_promote(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    target: String,
) -> ApiResult {
    request_move(state, sink, target, MoveKind::Promote).await
}

/// `model_storage_demote`: move a hot file to the cold tier (instant
/// when its cold archive is intact). Refused for pinned files.
pub async fn op_model_storage_demote(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    target: String,
) -> ApiResult {
    request_move(state, sink, target, MoveKind::Demote).await
}

/// `model_storage_cancel`: cancel the running move for `target` (or
/// drop it from the queue). The source is never touched by a copy.
pub async fn op_model_storage_cancel(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    target: String,
) -> ApiResult {
    let cfg = state.config.read().await.clone();
    let logical = target_logical(&cfg, &target)?;
    let key = path_key(&logical);
    let mut queue = lock(&state.model_storage.queue);
    let before = queue.len();
    queue.retain(|q| path_key(&q.logical) != key);
    if queue.len() != before {
        return Ok(json!({ "file": logical, "cancelled": "queued" }));
    }
    drop(queue);
    match lock(&state.model_storage.active).as_ref() {
        Some(a) if path_key(&a.logical) == key => {
            a.ctl.cancel.store(true, Ordering::SeqCst);
            Ok(json!({ "file": logical, "cancelled": "running", "task_id": a.task_id }))
        }
        _ => Err(ApiError::NotFound(format!(
            "no move is running or queued for {}",
            logical.display()
        ))),
    }
}

/// `model_set_keep_hot`: add or remove an operator keep-hot pin.
pub async fn op_model_set_keep_hot(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    target: String,
    keep: bool,
) -> ApiResult {
    let mut cfg = state.config.write().await;
    let logical = target_logical(&cfg, &target)?;
    let entry = cfg
        .models
        .iter()
        .find(|m| m.id == target)
        .map(|m| m.id.clone())
        .unwrap_or_else(|| logical.to_string_lossy().into_owned());
    mutate_save_install(&mut cfg, &state.config_path, |cfg| {
        let model_id = cfg
            .models
            .iter()
            .find(|m| path_key(&m.gguf_path) == path_key(&logical))
            .map(|m| m.id.clone());
        let ks = &mut cfg.model_storage.keep_hot;
        ks.retain(|e| {
            let e = e.trim();
            !(model_id.as_deref() == Some(e) || path_key(Path::new(e)) == path_key(&logical))
        });
        if keep {
            ks.push(entry.clone());
        }
    })?;
    state
        .model_storage
        .policy_dirty
        .store(true, Ordering::SeqCst);
    Ok(json!({ "file": logical, "keep_hot": keep }))
}

/// Save-time checks for a new `model_storage` section beyond the
/// structural validation every save runs: each cold root exists (it is
/// created when missing and its parent exists), is a directory, and is
/// not on the data directory's volume; a removed cold tier must not
/// still hold model files.
pub(crate) fn check_storage_paths(
    current: &AppConfig,
    next: &ModelStorageConfig,
) -> Result<(), ApiError> {
    for (i, c) in next.cold_tiers.iter().enumerate() {
        if !c.path.is_absolute() {
            return Err(ApiError::InvalidInput(format!(
                "cold tier {} must be an absolute path",
                c.path.display()
            )));
        }
        if !c.path.exists() {
            std::fs::create_dir_all(&c.path).map_err(|e| {
                ApiError::InvalidInput(format!(
                    "cold tier {} does not exist and could not be created: {e}",
                    c.path.display()
                ))
            })?;
        }
        if !c.path.is_dir() {
            return Err(ApiError::InvalidInput(format!(
                "cold tier {} is not a directory",
                c.path.display()
            )));
        }
        if same_volume(&c.path, &current.data_dir) == Some(true) {
            return Err(ApiError::InvalidInput(format!(
                "cold tier {} is on the same volume as the data directory; a cold tier must be a different drive",
                c.path.display()
            )));
        }
        let _ = i;
    }
    let kept: HashSet<String> = next.cold_tiers.iter().map(|c| path_key(&c.path)).collect();
    for c in &current.model_storage.cold_tiers {
        if kept.contains(&path_key(&c.path)) {
            continue;
        }
        let holds = std::fs::read_dir(&c.path)
            .map(|rd| {
                rd.flatten().any(|e| {
                    e.path()
                        .extension()
                        .map(|x| x.eq_ignore_ascii_case("gguf"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        if holds {
            return Err(ApiError::InvalidInput(format!(
                "cold tier {} still holds model files; promote or remove them before dropping the tier",
                c.path.display()
            )));
        }
    }
    Ok(())
}

/// `model_storage_set`: replace the `model_storage` section.
pub async fn op_model_storage_set(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    storage: Value,
) -> ApiResult {
    let next: ModelStorageConfig = serde_json::from_value(storage)
        .map_err(|e| ApiError::InvalidInput(format!("invalid model_storage section: {e}")))?;
    let mut cfg = state.config.write().await;
    check_storage_paths(&cfg, &next)?;
    mutate_save_install(&mut cfg, &state.config_path, |cfg| {
        cfg.model_storage = next.clone();
    })?;
    drop(cfg);
    tracing::info!(target: "manager_app::model_storage", cold_tiers = next.cold_tiers.len(), auto = next.auto, "model storage settings saved");
    state
        .model_storage
        .policy_dirty
        .store(true, Ordering::SeqCst);
    Ok(json!({ "model_storage": next }))
}

/// The tier a new download for `logical` lands in: wherever a copy
/// already is, else the configured download tier (cold only when
/// reachable).
pub(crate) fn download_destination(cfg: &AppConfig, logical: &Path) -> PathBuf {
    let r = resolve_model_path(&cfg.data_dir, &cfg.model_storage, logical);
    if let Residency::In(_) = r.residency {
        return r.path;
    }
    if cfg.model_storage.download_tier == manager_config::DownloadTier::Cold && cold_available(cfg)
    {
        if let Some(rel) = tier_relative(logical) {
            if let Some(p) = tier_file(&cfg.data_dir, &cfg.model_storage, Tier::Cold(0), &rel) {
                return p;
            }
        }
    }
    r.hot_path
}

/// Last-known time a file was started (for display); exposed for tests.
#[allow(dead_code)]
pub(crate) fn last_started(state: &AppState, logical: &Path) -> Option<DateTime<Utc>> {
    with_state(state, |st| st.usage(logical).and_then(|u| u.last_started))
}

#[cfg(test)]
mod tests;
