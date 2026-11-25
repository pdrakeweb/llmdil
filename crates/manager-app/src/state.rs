//! `AppState`: the shared service state, its constructor, lazily
//! initialized subsystems, and the config save/install helpers.
//!
//! `AppState` is a container: cohesive groups of state live in
//! per-subsystem objects (the `state/` submodules), each owning its
//! own locks. Re-homing a lock into a subsystem never merged it with
//! another lock or changed its kind (std vs tokio, `Mutex` vs
//! `RwLock`) or its critical sections; the rules below are the ones
//! the code already followed and still does.
//!
//! # Lock order
//!
//! Where two locks are ever held together they are acquired in this
//! order — never the reverse:
//!
//! 1. **wizard → config.** A wizard (`wizard(mode)`) guard is held
//!    while the config write lock is taken to apply a step's returned
//!    config edits (`apply_wizard_config_edits`, PS-4) and across
//!    hydration/restore; the step-payload cache
//!    (`wizard.steps_cache`) is written under the wizard guard too
//!    (wizard → steps cache).
//! 2. **config → downloads (PS-1).** `op_list_models` and the start
//!    preflight read the config and then lock the download handles;
//!    `op_download_model` therefore drops the `downloads.handles`
//!    guard before it waits on `config.write()` (a parked writer on
//!    tokio's `RwLock` queues every later reader, so the inversion
//!    deadlocked the whole app). Within downloads: handles →
//!    outcomes → verifying.
//! 3. **Supervisor liveness BEFORE config.write (PS-11).** Journaled
//!    edits take the config write lock through
//!    `ops::gateway::config_write_for_journal`, which gathers backend
//!    liveness (supervisor + SCM snapshot) against a config clone
//!    with no config guard held, then takes `config.write()` and
//!    hands the snapshot to `record_*_change`; it retries when the
//!    locked config's liveness inputs (backend ids/ports, supervision
//!    mode) changed in between. The activation status readers
//!    (`op_gateway_status`, `activation_unresolved`) read liveness
//!    against a config clone, not a held read guard. No supervisor →
//!    config path exists either; keep it that way.
//! 4. **Never initialize telemetry under the config write lock.** The
//!    first `telemetry()` call reads the config while building the
//!    poll loop; callers fetch telemetry/inventory before (or without)
//!    taking the config write lock.
//! 5. **Telemetry, gateway stats and direct slots are never taken
//!    under the supervisor lock**, and the preflight reads the
//!    telemetry snapshot and the supervisor status without holding
//!    either across the other.
//! 6. **tasks → event bus.** `record_task` publishes the task event
//!    while holding the task-records lock so no settle event can
//!    overtake it (the bus publish is synchronous and takes no
//!    `AppState` lock).
//!
//! Lifecycle (`lifecycle`, `scm_cache`, `lifecycle_monitor`,
//! `backend_spec_ids`) and log-shipping (`logship*`) state keep
//! their own rules, documented in [`crate::lifecycle`] and
//! [`crate::logship`].

use crate::*;

mod activation_slots;
mod bench_slots;
mod downloads;
mod gateway;
mod optimize_slots;
mod task_registry;
mod telemetry;
mod wizard;

pub(crate) use activation_slots::ActivationState;
pub(crate) use bench_slots::BenchState;
pub(crate) use downloads::{DownloadOutcome, DownloadsState};
pub(crate) use gateway::GatewayState;
pub(crate) use optimize_slots::{JobSlot, OptimizeState};
pub(crate) use task_registry::TaskRegistry;
pub(crate) use telemetry::TelemetryState;
pub(crate) use wizard::WizardState;

/// Shared backend state. Subsystems are lazily initialized (`OnceCell`) so
/// a failure surfaces on first use of that subsystem, never at startup.
pub struct AppState {
    pub(crate) config: RwLock<AppConfig>,
    pub(crate) config_path: PathBuf,
    pub(crate) data_dir: PathBuf,
    /// How the startup config load went (finding F2): set once by the
    /// shell right after construction from
    /// `manager_config::load_for_startup`, reported by `get_status` as
    /// `config_load` so the UI can flag a `.bak` fallback or a run on
    /// built-in defaults. `None` for states built without that call
    /// (tests, tools).
    pub(crate) config_load: std::sync::Mutex<Option<Value>>,
    pub(crate) supervisor: OnceCell<Mutex<Supervisor>>,
    /// GPU telemetry and history (see [`telemetry::TelemetryState`]).
    pub(crate) telemetry: TelemetryState,
    /// The log-streaming shipper (log-streaming design) and what it
    /// was started with; started after config load when the feature
    /// is enabled and restarted when its settings are saved. See
    /// [`logship`].
    pub(crate) logship: std::sync::Mutex<logship::LogshipSlot>,
    /// Serializes [`logship::apply_log_streaming`]: one start / stop /
    /// restart of the shipper at a time.
    pub(crate) logship_apply: Mutex<()>,
    /// The shipper's in-process record feed (app events, process-mode
    /// backend output). Exists from startup so sinks can be wrapped
    /// before the shipper starts; sending is a no-op until a shipper
    /// subscribes.
    pub(crate) logship_tap: manager_logship::RecordTap,
    /// The app-level event bus (see [`event_bus`]).
    pub(crate) event_bus: event_bus::AppEventBus,
    /// The desktop shell's own sink (see [`AppState::set_desktop_sink`]).
    pub(crate) desktop_sink: std::sync::OnceLock<Arc<dyn EventSink>>,
    /// The setup wizard (see [`wizard::WizardState`]).
    pub(crate) wizard: WizardState,
    /// The tracked-task registry (see [`task_registry::TaskRegistry`]).
    pub(crate) tasks: TaskRegistry,
    /// The model-download subsystem (see [`downloads::DownloadsState`]).
    pub(crate) downloads: DownloadsState,
    /// Ids of the supervisor specs this layer registered as backends.
    /// The Supervisor is shared with the MCP subsystem (process ids
    /// `mcp-<id>`); pruning and orphan-surfacing in the backend commands
    /// must only ever touch ids in this set.
    pub(crate) backend_spec_ids: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Session-only MCP process modes, overlaid on the specs derived
    /// from `AppConfig.mcp` by [`AppState::mcp_servers`]. Nothing
    /// writes it any more: the persisted registry is `AppConfig.mcp`
    /// (exposure lives there, see [`AppState::mcp_registry`]), and
    /// Local processes are spawned from it by `mcp_local` (review
    /// v0.1.60 PS-5).
    pub(crate) mcp_modes: Mutex<HashMap<String, manager_mcp::McpMode>>,
    /// The gateway observation stores (see [`gateway::GatewayState`]).
    pub(crate) gateway: GatewayState,
    /// WinML secondary-backend registry (stateful single-model rule).
    pub(crate) winml: OnceCell<Mutex<manager_winml::WinMlRegistry>>,
    /// The Optimize job slots (see [`optimize_slots::OptimizeState`]).
    pub(crate) optimize: OptimizeState,
    /// The Bench job slots (see [`bench_slots::BenchState`]).
    pub(crate) bench: BenchState,
    /// Transition-watch state for the toast notification monitor
    /// (see [`notify`]): backend states, gateway liveness
    /// expectation, and activation-plan pending state, so
    /// state-driven toasts fire on transitions only. std Mutex:
    /// synchronous diff/update only.
    pub(crate) notify_watch: std::sync::Mutex<notify::NotifyWatch>,
    /// Serializes app self-update work (check / stage / apply): a
    /// scheduled check, a manual check, and a Settings download can
    /// race on the same state file and staging directory otherwise.
    /// Held across the operation; the network calls involved are
    /// bounded (release list is one small JSON document, the
    /// installer ~8 MB).
    pub(crate) app_update_guard: Mutex<()>,
    /// Latest offload-probe state per backend id (see the probe run
    /// after a start that activates a saved offload change). std
    /// Mutex: synchronous insert/read only.
    pub(crate) offload_probes: std::sync::Mutex<std::collections::HashMap<String, OffloadProbe>>,
    /// Per-service lifecycle registry (see [`lifecycle`]): in-flight
    /// transitions, operator intent, failures and readiness for the
    /// gateway, PostgreSQL, the log receiver and every backend.
    pub(crate) lifecycle: Arc<lifecycle::LifecycleRegistry>,
    /// The cached SCM snapshot ([`lifecycle::scm_snapshot`]): when it
    /// was taken and the status of every managed service. std Mutex:
    /// synchronous read/replace only.
    #[allow(clippy::type_complexity)]
    pub(crate) scm_cache: std::sync::Mutex<
        Option<(
            std::time::Instant,
            Arc<HashMap<String, manager_services::ServiceStatus>>,
        )>,
    >,
    /// Activation plan apply serialization and progress (see
    /// [`activation_slots::ActivationState`]).
    pub(crate) activation: ActivationState,
    /// The lifecycle monitor's counters between ticks (see
    /// [`lifecycle_monitor`]). std Mutex: synchronous updates only.
    pub(crate) lifecycle_monitor: std::sync::Mutex<lifecycle_monitor::MonitorState>,
}

/// One running Estimate/Measure job's descriptor (tracker #69),
/// carried in the Optimize state payload as `job`: the job kind
/// (`"measure"` or `"estimate"`), the backend it runs against,
/// and when it started (Unix ms) — everything a page instance
/// that did not start the job needs to render the true
/// in-progress state with the correct per-profile copy.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OptimizeJobInfo {
    pub kind: String,
    pub backend_id: String,
    pub started_unix_ms: i64,
}

/// The two Optimize operation families that contend for a
/// backend (tracker #70): a consultation generates against the
/// backend while a benchmark (Measure / Estimate, a screening-eval
/// baseline run, or an experiment batch's baseline run against
/// its consulted backend) measures it. Within a family the
/// existing slots already serialize (one consultation at a time;
/// one GPU job at a time); across families the per-backend
/// record in [`AppState`] is the serialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptimizeOpFamily {
    Consultation,
    Benchmark,
}

/// One running Optimize operation's per-backend record
/// (tracker #70), carried in the Optimize state payload as
/// `running_ops`: which family and kind of operation holds the
/// backend, and when it started (Unix ms) — so a refusal can
/// name the running operation and its elapsed time, including
/// for a page instance that did not start it (the naming the
/// tracker #69 job-state payload provides for the same reason).
#[derive(Debug, Clone)]
pub struct OptimizeRunningOp {
    pub family: OptimizeOpFamily,
    /// The operation kind: `consultation`, `measure`,
    /// `estimate`, `eval-baseline`, `experiment`, or
    /// `experiment-estimate`.
    pub kind: String,
    pub backend_id: String,
    pub started_unix_ms: i64,
    /// Unique token of this claim (monotonic per process): a
    /// release clears the record only when the token matches, so
    /// a cancelled operation's late release cannot clear a newer
    /// operation's record on the same backend.
    pub claim_id: u64,
}

/// One backend's offload-probe state: a start that activated a saved
/// offload change runs a long-prompt probe against the new position
/// and records the outcome here for the backends list to surface.
#[derive(Debug, Clone)]
pub struct OffloadProbe {
    /// `probing` | `passed` | `failed`.
    pub(crate) state: String,
    /// Offload slider position (MoE layers on system RAM) probed.
    pub(crate) layers: u32,
    pub(crate) detail: Option<String>,
}

/// On-disk form of the install wizard's progress
/// (`<data_dir>/wizard-state.json`): the wizard's own report,
/// restored at launch so closing the app does not reset the
/// checklist. Written after every state mutation.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct PersistedWizard {
    pub(crate) version: u32,
    pub(crate) report: WizardReport,
}

/// The Credential Manager target names whose credential is
/// PRESENT, across the Gateway-exposure MCP servers of the given
/// configurations — the arming predicate input for every drift
/// and journaling comparison (MCP design §4.2: presence, not
/// values; the accepted gap is documented on
/// [`manager_wizard::armed_gateway_mcp_servers`]). Callers
/// comparing a pre/post pair pass both configurations, so a
/// server removed by the change still resolves its arming state
/// for the pre-change side.
pub(crate) async fn mcp_credential_presence(
    cfgs: &[&AppConfig],
) -> std::collections::HashSet<String> {
    let ops = RealSystemOps::default();
    let mut present: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut targets: Vec<&str> = Vec::new();
    for cfg in cfgs {
        for server in cfg
            .mcp
            .servers
            .iter()
            .filter(|s| s.exposure == manager_config::McpExposure::Gateway)
        {
            if let Some(target) = server.cred_ref.as_deref().filter(|r| !r.is_empty()) {
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
        }
    }
    for target in targets {
        if ops.credential_exists(target).await.unwrap_or(false) {
            present.insert(target.to_string());
        }
    }
    present
}

impl AppState {
    /// The app's data directory (config, logs, and the API's
    /// stores live under it). Exposed for the desktop shell's
    /// server-lifecycle commands.
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    pub fn new(config: AppConfig, config_path: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            config: RwLock::new(config),
            config_path,
            data_dir,
            config_load: std::sync::Mutex::new(None),
            supervisor: OnceCell::new(),
            telemetry: TelemetryState::default(),
            logship: std::sync::Mutex::new(logship::LogshipSlot::default()),
            logship_apply: Mutex::new(()),
            desktop_sink: std::sync::OnceLock::new(),
            logship_tap: manager_logship::RecordTap::new(),
            event_bus: event_bus::AppEventBus::default(),
            wizard: WizardState::default(),
            tasks: TaskRegistry::default(),
            downloads: DownloadsState::default(),
            backend_spec_ids: std::sync::Mutex::new(std::collections::HashSet::new()),
            mcp_modes: Mutex::new(HashMap::new()),
            gateway: GatewayState::default(),
            winml: OnceCell::new(),
            bench: BenchState::default(),
            optimize: OptimizeState::default(),
            notify_watch: std::sync::Mutex::new(notify::NotifyWatch::default()),
            app_update_guard: Mutex::new(()),
            offload_probes: std::sync::Mutex::new(std::collections::HashMap::new()),
            lifecycle: Arc::new(lifecycle::LifecycleRegistry::new()),
            scm_cache: std::sync::Mutex::new(None),
            lifecycle_monitor: std::sync::Mutex::new(lifecycle_monitor::MonitorState::default()),
            activation: ActivationState::default(),
        }
    }

    /// The per-service lifecycle registry.
    pub fn lifecycle(&self) -> &Arc<lifecycle::LifecycleRegistry> {
        &self.lifecycle
    }

    /// Record the startup config-load outcome (finding F2) for
    /// `get_status`. `source` is the `manager_config::ConfigSource`;
    /// `error` the primary load error; `preserved` the
    /// `*.corrupt-<unix-ms>` copies made before anything could save.
    pub fn set_config_load_state(
        &self,
        source: manager_config::ConfigSource,
        error: Option<String>,
        preserved: &[PathBuf],
    ) {
        let v = json!({
            "source": source,
            "error": error,
            "preserved": preserved
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
        });
        *self.config_load.lock().unwrap_or_else(|p| p.into_inner()) = Some(v);
    }

    pub(crate) async fn record_task(&self, kind: &str, detail: Value) -> String {
        let id = self.tasks.next_id();
        let now = now_ms();
        let record = TaskRecord {
            id: id.clone(),
            kind: kind.to_string(),
            status: TaskStatus::Running,
            detail,
            created_ms: now,
            started_ms: now,
            finished_ms: None,
            error: None,
        };
        // The control exists before the record, so an abort that
        // finds the record running always finds its token.
        self.tasks.controls.register(&id);
        let mut tasks = self.tasks.records.lock().await;
        // Published under the lock: a subscriber that GETs the task
        // on this event finds it, and no settle event can overtake it.
        event_bus::publish_task(self, &record);
        tasks.insert(id.clone(), record);
        drop(tasks);
        tasks::link_task(self, &id);
        tasks::task_started(&id, kind);
        id
    }

    /// Settle a task's terminal state (abort-aware; see [`tasks`]).
    pub(crate) async fn finish_task(&self, id: &str, status: TaskStatus, detail: Value) {
        tasks::finish(self, id, status, detail).await;
    }

    pub(crate) async fn wizard_ctx(&self, mode: RunMode) -> WizardContext {
        // The wizard reads the same model registry as every other view;
        // make sure the first-run catalog seeding has happened before
        // the snapshot is taken, whichever view runs first. (Seeding
        // used to live only inside the Models commands, so a wizard run
        // before the first Models visit saw an empty registry and its
        // download step reported "no models configured".)
        ensure_catalog_models(self).await;
        // Live step progress goes straight to wizard.log: the step
        // outcome's own log is only persisted when the step returns,
        // so a multi-minute step (a cold package install, a database
        // wait) otherwise leaves the file silent between "started"
        // and the outcome block and reads as a hang.
        let log_data_dir = self.data_dir.clone();
        let log_sink: manager_wizard::StepLogSink = Arc::new(move |line: String| {
            append_wizard_log(
                &log_data_dir,
                &[format!("[{}] {line}", chrono::Utc::now().to_rfc3339())],
            );
        });
        WizardContext {
            config: self.config.read().await.clone(),
            data_dir: self.data_dir.clone(),
            mode,
            ops: Arc::new(RealSystemOps::default()),
            log_sink: Some(log_sink),
            // Steps return their config edits; the app applies them
            // under the config lock (apply_wizard_config_edits, PS-4).
            config_edit_mode: manager_wizard::ConfigEditMode::ReturnToCaller,
        }
    }

    pub(crate) async fn supervisor(&self) -> &Mutex<Supervisor> {
        self.supervisor
            .get_or_init(|| async {
                Mutex::new(Supervisor::new(BackoffPolicy {
                    base: Duration::from_secs(1),
                    max: Duration::from_secs(60),
                    max_retries: 5,
                }))
            })
            .await
    }

    /// The shared telemetry store. The first call builds the poll loop
    /// (NVML with an nvidia-smi fallback) at the configured cadence and
    /// starts it; the store the loop writes is the store readers see.
    /// Before this wiring the store was never fed and snapshots were
    /// always empty.
    pub(crate) async fn telemetry(&self) -> Arc<std::sync::Mutex<TelemetryStore>> {
        self.telemetry
            .store
            .get_or_init(|| async {
                let interval_ms = self.config.read().await.telemetry.poll_interval_ms;
                let cadence = if interval_ms == 0 {
                    PollCadence::Off
                } else if interval_ms <= 1500 {
                    PollCadence::Every1s
                } else {
                    PollCadence::Every5s
                };
                let source = manager_telemetry::default_telemetry_stack();
                let (poller, store) = Poller::new(source, cadence);
                let handle = poller.start();
                let _ = self.telemetry.poller.set(std::sync::Mutex::new(handle));
                // Static GPU inventory (index, UUID, driver model)
                // for launch construction: CUDA_VISIBLE_DEVICES is
                // expressed in GPU UUIDs (see backend_spec). The
                // query is BOUNDED and retried: it previously ran
                // unbounded inside this OnceCell init, so a hung
                // nvidia-smi (a wedged driver call is exactly when
                // inventory matters) stalled every telemetry caller
                // for the whole session, and a transient failure
                // cached an empty inventory forever — launches then
                // stayed unrestricted with no signal anywhere
                // (code-review 4.2). Spec building treats an empty
                // inventory as "cannot determine" and leaves
                // launches unrestricted, so a late-arriving
                // inventory only improves later launches.
                let smi = NvidiaSmiSource::default();
                let attempt = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    smi.run_query(),
                )
                .await;
                let loaded = match attempt {
                    Ok(Ok(xml)) => match NvidiaSmiSource::parse_inventory(&xml) {
                        Ok(inventory) if !inventory.is_empty() => {
                            lock_telemetry(&store).set_inventory(inventory);
                            true
                        }
                        Ok(_) => {
                            tracing::warn!("gpu inventory: nvidia-smi reported no GPUs");
                            false
                        }
                        Err(e) => {
                            tracing::warn!("gpu inventory: could not parse nvidia-smi output: {e}");
                            false
                        }
                    },
                    Ok(Err(e)) => {
                        tracing::warn!("gpu inventory: nvidia-smi query failed: {e}");
                        false
                    }
                    Err(_) => {
                        tracing::warn!("gpu inventory: nvidia-smi query timed out after 10s");
                        false
                    }
                };
                if !loaded {
                    let store_retry = store.clone();
                    tokio::spawn(async move {
                        for delay_secs in [5u64, 15, 45] {
                            tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
                            let smi = NvidiaSmiSource::default();
                            let attempt = tokio::time::timeout(
                                std::time::Duration::from_secs(10),
                                smi.run_query(),
                            )
                            .await;
                            if let Ok(Ok(xml)) = attempt {
                                if let Ok(inventory) = NvidiaSmiSource::parse_inventory(&xml) {
                                    if !inventory.is_empty() {
                                        lock_telemetry(&store_retry)
                                            .set_inventory(inventory);
                                        tracing::info!(
                                            "gpu inventory: loaded on retry after {delay_secs}s"
                                        );
                                        return;
                                    }
                                }
                            }
                            tracing::warn!(
                                "gpu inventory: retry after {delay_secs}s did not load an inventory"
                            );
                        }
                        tracing::warn!(
                            "gpu inventory: giving up after retries; launches stay unrestricted until the next app start"
                        );
                    });
                }
                store
            })
            .await
            .clone()
    }

    /// The persistent GPU history cell. The first call loads the
    /// history file from the data dir (missing/corrupt = empty);
    /// the recorder appends and the history op reads through the
    /// same cell, so the file's in-memory image is single-owned.
    pub(crate) async fn gpu_history(&self) -> &std::sync::Mutex<gpu_history::GpuHistoryCell> {
        self.telemetry
            .gpu_history
            .get_or_init(|| async {
                std::sync::Mutex::new(gpu_history::GpuHistoryCell {
                    store: gpu_history::load_history(&self.data_dir),
                    last_saved_ms: 0,
                })
            })
            .await
    }

    /// Transport setup can only fail on local client misconfiguration;
    /// the error is surfaced to the caller instead of panicking.
    pub(crate) async fn winml(&self) -> Result<&Mutex<manager_winml::WinMlRegistry>, ApiError> {
        if let Some(r) = self.winml.get() {
            return Ok(r);
        }
        let transport = manager_winml::ReqwestProbeTransport::new(Duration::from_secs(10))
            .map_err(|e| ApiError::WinMl(format!("winml transport setup failed: {e:?}")))?;
        Ok(self
            .winml
            .get_or_init(|| async { Mutex::new(manager_winml::WinMlRegistry::new(transport)) })
            .await)
    }

    pub(crate) async fn wizard(&self, mode: RunMode) -> &Mutex<Wizard> {
        self.wizard.get(mode).await
    }

    /// The full gateway drift check for hydration/restore: the
    /// on-disk YAML matches the current configuration AND the
    /// recorded spawn fingerprint matches the current spawn inputs
    /// (port, bind, PostgreSQL presence, credential hashes —
    /// code-review 2.3). The fingerprint is computed from the
    /// credentials as read NOW; a credential that cannot be read
    /// hashes as absent, which cannot match a recorded spawn that
    /// had one — unreadable therefore reads as drifted, the safe
    /// direction.
    pub async fn gateway_config_current(&self, cfg: &AppConfig) -> bool {
        let ops = RealSystemOps::default();
        let postgres = manager_wizard::steps::postgres_installed(&ops, &self.data_dir).await;
        let master = ops
            .credential_read(manager_wizard::LITELLM_MASTER_KEY_CRED_TARGET)
            .await;
        let tier = &cfg.gateway.openrouter;
        let openrouter = if tier.enabled {
            match tier.cred_ref.as_deref().filter(|r| !r.is_empty()) {
                Some(r) => ops.credential_read(r).await,
                None => None,
            }
        } else {
            None
        };
        // MCP arming for the drift path (MCP design §4.2, Phase 2):
        // resolved by credential PRESENCE (credential_exists), not
        // by reading values — the one arming rule in manager-wizard
        // documents this accepted gap (the fingerprint hash covers
        // the cred_ref name, never the value, so presence determines
        // set membership unless a credential exists but is
        // unreadable). Presence results are gathered async here and
        // handed to the sync helpers as a lookup set.
        let mcp_present = mcp_credential_presence(&[cfg]).await;
        let mcp_armed = manager_wizard::armed_gateway_mcp_servers(cfg, |t| mcp_present.contains(t));
        let fingerprint = manager_wizard::gateway_spawn_fingerprint(
            cfg,
            postgres,
            master.as_deref(),
            openrouter.as_deref(),
            &mcp_armed,
        );
        manager_wizard::gateway_config_is_current(cfg, &self.data_dir, &fingerprint, |t| {
            mcp_present.contains(t)
        })
    }

    /// Register the desktop shell's sink, once at startup: operations
    /// dispatched by a surface without an OS toast (the Admin API) use
    /// it where the toast is their whole purpose.
    pub fn set_desktop_sink(&self, sink: Arc<dyn EventSink>) {
        let _ = self.desktop_sink.set(sink);
    }

    /// The desktop shell's sink, when a shell registered one.
    pub(crate) fn desktop_sink(&self) -> Option<Arc<dyn EventSink>> {
        self.desktop_sink.get().cloned()
    }

    /// The configured supervision mode (process | service).
    pub(crate) async fn supervision_mode(&self) -> manager_config::SupervisionMode {
        self.config.read().await.supervision.mode
    }

    /// Whether lifecycle routing for `service_name` goes to the SCM:
    /// the global mode is Service AND the service is registered.
    /// Registration is the per-service opt-in, so an unregistered
    /// backend in service mode keeps process supervision (mixed mode;
    /// see docs/windows-services-design.md). Reads the shared SCM
    /// snapshot (at most 1 s old); a name outside it (an unconfigured
    /// backend) is queried directly on the blocking pool.
    pub(crate) async fn service_route_active(&self, service_name: &str) -> bool {
        if self.supervision_mode().await != manager_config::SupervisionMode::Service {
            return false;
        }
        let snap = lifecycle::scm_snapshot(self, Duration::from_secs(1)).await;
        if let Some(status) = snap.get(service_name) {
            return !matches!(status.state, manager_services::ServiceState::NotInstalled);
        }
        let name = service_name.to_string();
        tokio::task::spawn_blocking(move || {
            use manager_services::ServiceControl;
            manager_services::scm::platform_control()
                .is_registered(&name)
                .unwrap_or(false)
        })
        .await
        .unwrap_or(false)
    }

    /// Regenerate a backend's service definition from the supervisor's
    /// registered spawn spec — the same spec object process mode
    /// spawns — and write it to `<data_dir>/services/`. Returns the
    /// definition path.
    /// Regenerate a backend's service definition from the supervisor's
    /// registered spawn spec — the same spec object process mode
    /// spawns — and write it to `<data_dir>/services/`. Returns the
    /// definition path.
    pub(crate) async fn write_backend_service_definition(
        &self,
        backend_id: &str,
    ) -> Result<std::path::PathBuf, String> {
        ensure_backend_specs(self).await;
        let spec = {
            let sup = self.supervisor().await;
            let sup = sup.lock().await;
            sup.spec(backend_id).cloned()
        };
        let spec = spec.ok_or_else(|| format!("backend {backend_id} has no spawn spec"))?;
        let def = manager_services::backend_definition(&spec, &self.data_dir.join("logs"));
        manager_services::write_definition(&self.data_dir, &def).map_err(|e| e.to_string())
    }

    /// Definition sync for the config-save paths: regenerate the
    /// backend's service definition when service mode is on. A sync
    /// failure is written to the wizard log, never silently dropped —
    /// and the start path regenerates as well and fails visibly there,
    /// so a stale definition cannot quietly reach a service start.
    pub(crate) async fn sync_backend_service_definition(&self, backend_id: &str) {
        if self.supervision_mode().await != manager_config::SupervisionMode::Service {
            return;
        }
        if let Err(e) = self.write_backend_service_definition(backend_id).await {
            append_wizard_log(
                &self.data_dir,
                &[format!(
                    "[{}] service definition sync for backend {backend_id} failed: {e}",
                    chrono::Utc::now().to_rfc3339()
                )],
            );
        }
    }

    /// Reconcile the persisted `setup_completed` flag with the
    /// install wizard's actual state. The flag was historically set
    /// only by the wizard view's Finish button (`setup_mark_complete`);
    /// every other completion path — a full "Run setup" run, per-step
    /// runs, skip / mark-manual, and launch-time hydration from live
    /// machine checks — left it false, so a user with a functionally
    /// complete setup was routed back to the wizard on every launch
    /// (field-confirmed 2026-10-17: every step passing, flag false).
    /// When the wizard's report is complete
    /// ([`WizardReport::setup_complete`]: the required steps ended,
    /// the optional ones may never have run) the flag is set and
    /// persisted, so launch routing (the UI reads the flag via
    /// `setup_status`) sends the user to the Dashboard. The report
    /// saved by the previous session counts too: at launch the UI asks
    /// before `restore_wizard` has run. Best effort: a failed save
    /// keeps the in-memory value and the next trigger retries.
    pub async fn reconcile_setup_completed(&self) {
        if self.config.read().await.setup_completed {
            return;
        }
        let complete = {
            // try_lock: a step holds this lock for its whole run and
            // reconciles when it ends; a reader must not wait for it.
            let wizard = self.wizard(RunMode::Install).await;
            let live = wizard.try_lock().is_ok_and(|g| g.report().setup_complete());
            live || self
                .saved_wizard()
                .is_some_and(|p| p.report.setup_complete())
        };
        if !complete {
            return;
        }
        let mut cfg = self.config.write().await;
        if cfg.setup_completed {
            return;
        }
        let mut new_cfg = cfg.clone();
        new_cfg.setup_completed = true;
        match manager_config::save(&new_cfg, &self.config_path) {
            Ok(()) => {
                *cfg = new_cfg;
                append_wizard_log(
                    &self.data_dir,
                    &[format!(
                        "[{}] setup_completed reconciled: every required wizard step is done/manual/skipped, so the flag was set without a Finish click",
                        chrono::Utc::now().to_rfc3339()
                    )],
                );
            }
            Err(e) => {
                append_wizard_log(
                    &self.data_dir,
                    &[format!(
                        "[{}] setup_completed reconcile could not persist: {e}",
                        chrono::Utc::now().to_rfc3339()
                    )],
                );
            }
        }
    }

    /// The persisted MCP registry (`AppConfig.mcp`) — the source of
    /// truth since MCP Phase 1 (design §3/§10.1); exposure is persisted
    /// here. (The session-only `mcp_modes` overlay still exists but is
    /// never written; see the field's doc.)
    pub(crate) async fn mcp_registry(&self) -> Vec<manager_config::McpServerConfig> {
        self.config.read().await.mcp.servers.clone()
    }

    /// Seed the install wizard's checklist from the machine's actual
    /// state: run the audit wizard's checks once and adopt the passing
    /// outcomes, so the checklist does not reset to all-Queued on
    /// every launch. Runs in the background at startup; best-effort —
    /// the wizard stays usable (in its default state) while it runs.
    pub async fn hydrate_wizard(&self) {
        let ctx = self.wizard_ctx(RunMode::Audit).await;
        let audit = self.wizard(RunMode::Audit).await;
        let report = {
            let mut guard = audit.lock().await;
            let _ = guard.run_all(&ctx).await;
            // Audit-verified models return verified_at stamps (PS-4).
            self.apply_wizard_config_edits(&mut guard).await;
            guard.report()
        };
        for s in &report.steps {
            append_wizard_log(
                &self.data_dir,
                &wizard_log_block(
                    "hydration audit",
                    &s.id,
                    &format!("{:?}", s.state).to_lowercase(),
                    &s.message,
                    s.duration_ms,
                    &s.log_tail,
                ),
            );
        }
        let install = self.wizard(RunMode::Install).await;
        let mut guard = install.lock().await;
        // The audit's start-gateway pass only proves a gateway
        // answers; it is adopted as the install step's completion
        // only while the on-disk gateway config still matches the
        // current configuration (see Wizard::hydrate).
        let gateway_current = self.gateway_config_current(&ctx.config).await;
        guard.hydrate(&report, gateway_current);
        *self.wizard.steps_cache.write().await = Some(wizard_steps_payload(&guard));
    }

    /// Persist the install wizard's current report to
    /// `wizard-state.json` (atomic tmp + rename). Best effort: a
    /// failed write must never fail the step that triggered it.
    pub async fn persist_wizard(&self) {
        let wizard = self.wizard(RunMode::Install).await;
        let report = {
            let guard = wizard.lock().await;
            guard.report()
        };
        let persisted = PersistedWizard { version: 1, report };
        let Ok(text) = serde_json::to_string(&persisted) else {
            return;
        };
        let path = self.data_dir.join("wizard-state.json");
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// Reload the live config from `<data_dir>/config.json` after a
    /// wizard step or audit run. Steps receive a snapshot clone and
    /// persist their writes to the file (tensor-split placement,
    /// verified_at stamps); without this reload those writes never
    /// reach the in-memory config, so backend starts later in the
    /// same session would still use the pre-step values. Best
    /// effort: a missing or invalid file keeps the current config —
    /// validity was enforced when the file was saved, and a wedged
    /// file must not take the session down.
    /// Re-read config.json after a wizard step or audit (steps may
    /// write it directly). Two guards (finding PS-3,
    /// docs/code-review-v0.1.60.md):
    ///
    /// - The write lock is taken BEFORE reading the file, so an
    ///   in-flight mutator (which holds the write lock across its save)
    ///   finishes first; reading outside the lock could install a file
    ///   read before that mutator saved, reverting its change in memory
    ///   and, on the next save, on disk.
    /// - A load that only succeeded via the `.bak` fallback is not
    ///   installed: the primary is briefly absent during a save's
    ///   rotation, and `.bak` is the previous config by construction.
    pub(crate) async fn reload_config_from_disk(&self) {
        let path = self.data_dir.join("config.json");
        let mut live = self.config.write().await;
        if let Ok(report) = manager_config::load_detailed(&path) {
            if !report.used_backup {
                *live = report.config;
            }
        }
    }

    /// Apply the config edits wizard steps returned (code-review
    /// PS-4) under the config write lock: lock → clone → apply → save
    /// → install, one step's edits at a time, so app-side changes
    /// made while the step ran survive. An edit whose target was
    /// deleted meanwhile is skipped and logged. A failed save marks
    /// the step failed in `wizard` and is returned as `(step id,
    /// message)`.
    pub(crate) async fn apply_wizard_config_edits(
        &self,
        wizard: &mut Wizard,
    ) -> Vec<(String, String)> {
        let mut failed = Vec::new();
        for (step_id, edits) in wizard.take_config_edits() {
            let mut live = self.config.write().await;
            let mut new_cfg = live.clone();
            let applied = manager_wizard::apply_config_edits(&mut new_cfg, &edits);
            let now = chrono::Utc::now().to_rfc3339();
            let mut lines: Vec<String> = applied
                .log
                .iter()
                .map(|l| format!("[{now}] step {step_id}: {l}"))
                .collect();
            if applied.applied > 0 {
                match manager_config::save(&new_cfg, &self.config_path) {
                    Ok(()) => *live = new_cfg,
                    Err(e) => {
                        drop(live);
                        let message = format!(
                            "the step's configuration changes could not be saved to {}: {e}",
                            self.config_path.display()
                        );
                        lines.push(format!("[{now}] step {step_id}: {message}"));
                        wizard.fail_step(&step_id, message.clone(), message.clone());
                        failed.push((step_id.clone(), message));
                    }
                }
            }
            append_wizard_log(&self.data_dir, &lines);
        }
        failed
    }

    /// The wizard report saved by [`Self::persist_wizard`]; `None`
    /// when it is missing or unreadable.
    fn saved_wizard(&self) -> Option<PersistedWizard> {
        let text = std::fs::read_to_string(self.data_dir.join("wizard-state.json")).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Restore persisted wizard progress at launch, before the
    /// hydration audit fills still-Queued steps from machine checks.
    /// A missing or unreadable file leaves the wizard untouched.
    pub async fn restore_wizard(&self) {
        let Some(persisted) = self.saved_wizard() else {
            return;
        };
        // A step persisted as Running was in flight when the app
        // closed: its outcome was never written (outcomes land on
        // completion). Mark the interruption in the wizard log so
        // the log shows why there is a "started" with no outcome —
        // the gap that made the 2026-10-16 half-repaired gateway
        // undiagnosable from the bundle alone.
        for s in &persisted.report.steps {
            if s.state == StepState::Running {
                append_wizard_log(
                    &self.data_dir,
                    &[format!(
                        "[{}] === step {} (install): interrupted — the app closed while this step was running; re-run it to complete ===",
                        chrono::Utc::now().to_rfc3339(),
                        s.id
                    )],
                );
            }
        }
        let wizard = self.wizard(RunMode::Install).await;
        let mut guard = wizard.lock().await;
        // Same drift rule as hydration: a persisted start-gateway
        // Done is adopted only while the gateway config on disk
        // still matches the current configuration, or the gateway is
        // running (see Wizard::restore).
        let cfg = self.config.read().await.clone();
        let gateway_current = self.gateway_config_current(&cfg).await;
        let gateway_running = {
            let host = manager_wizard::steps::gateway_probe_host(&cfg);
            let port = cfg.gateway.port;
            tokio::task::spawn_blocking(move || crate::probe_gateway(&host, port))
                .await
                .unwrap_or(false)
        };
        guard.restore(&persisted.report, gateway_current, gateway_running);
        *self.wizard.steps_cache.write().await = Some(wizard_steps_payload(&guard));
    }

    /// Runtime views of the persisted MCP registry, in registry order
    /// (one per `AppConfig.mcp` entry), with in-memory mode overrides
    /// applied. Derived from config since PS-5, so curated and
    /// operator-added servers have specs like the built-in two.
    pub(crate) async fn mcp_servers(&self) -> Vec<manager_mcp::ServerSpec> {
        let specs = manager_mcp::server_specs_from_config(&self.config.read().await.mcp);
        let modes = self.mcp_modes.lock().await;
        specs
            .into_iter()
            .map(|mut s| {
                if let Some(m) = modes.get(&s.id) {
                    s.mode = *m;
                }
                s
            })
            .collect()
    }
}

/// Lock the shared telemetry store, recovering from a poisoned mutex
/// (a panicked poll write must not wedge every later read).
pub(crate) fn lock_telemetry(
    store: &Arc<std::sync::Mutex<TelemetryStore>>,
) -> std::sync::MutexGuard<'_, TelemetryStore> {
    store.lock().unwrap_or_else(|e| e.into_inner())
}

/// Resolve a config path against the data dir (absolute paths pass
/// through). Config model paths are relative (`models/{id}.gguf`); every
/// filesystem operation must go through this so it agrees with the path
/// list_models probes and download_model writes.
pub(crate) fn resolve_under_data_dir(
    data_dir: &std::path::Path,
    path: &std::path::Path,
) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        data_dir.join(path)
    }
}

/// Drop download handles that reached a terminal state (done, failed,
/// cancelled). Without this, a finished download sits in the map
/// forever: list_models reports `downloading: true` permanently and the
/// start preflight rejects the backend as "still downloading" until the
/// app restarts. Called on the paths that consult the map.
pub(crate) async fn reap_downloads(state: &AppState) {
    state.downloads.reap().await;
}

/// True when `sha` is a real pinned SHA-256 — 64 hex chars — as opposed
/// to the catalog placeholder (`PLACEHOLDER_SHA256`, which contains
/// non-hex characters and is not 64 chars) or an unset value. Matches
/// the check `download_spec_for` has always applied.
pub(crate) fn is_real_pin(sha: &str) -> bool {
    sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Apply `mutate` to a clone of the live configuration, save the
/// clone, and install it only when the save succeeded (PS-8): a
/// failed save leaves memory exactly as it was, so memory and disk
/// never diverge.
pub(crate) fn mutate_save_install(
    live: &mut AppConfig,
    path: &PathBuf,
    mutate: impl FnOnce(&mut AppConfig),
) -> Result<(), manager_config::ConfigError> {
    let mut next = live.clone();
    mutate(&mut next);
    manager_config::save(&next, path)?;
    *live = next;
    Ok(())
}

/// Seed the model config from the curated catalog on first run: the
/// Models view and the download path operate on configured models, so
/// without seeding a fresh install would show an empty list even
/// though the catalog exists. Seeding happens at most once per config
/// file, gated on `AppConfig.catalog_seeded`: after seeding, the flag
/// is set and saved, so models the operator later deletes stay deleted
/// across restarts. A config that already holds models but predates
/// the flag gets the flag set in memory only (it persists on the next
/// save) and is never reseeded.
///
/// Seeded models start enabled (the wizard's download step installs
/// enabled models) and pre-assigned to their default backend. A default
/// backend whose `model_file` still carries the untouched default
/// (`models/<backend-id>.gguf`) is aligned to the seeded model's path
/// and enabled, so the default fleet is ready to start once its models
/// are downloaded and verified, and config validation (an enabled
/// backend's model_file must be a registered model) passes.
///
/// Configurations written before the catalog carried real hash pins
/// and parameter counts get a one-time repair first
/// ([`manager_config::repair_catalog_defaults`], gated on
/// `AppConfig::defaults_repaired`): placeholder hashes become the real
/// pins, parameter counts are filled in, default backend assignments
/// and model_file alignment are applied, and the default fleet is
/// enabled — exactly the state the current seeding produces.
pub(crate) async fn ensure_catalog_models(state: &AppState) {
    let backend_for = |model_id: &str| -> Option<&'static str> {
        match model_id {
            "qwen3-27b-planner" => Some("planner"),
            "qwen3-coder-30b" => Some("coder"),
            "qwen2.5-coder-7b" => Some("coder-fast"),
            "qwen3-8b" => Some("tool-runner"),
            _ => None,
        }
    };
    // PS-8: work on a clone; `install_seeded` installs it only when
    // no save was needed or the save succeeded.
    let mut live = state.config.write().await;
    let mut cfg = live.clone();
    let mut dirty = false;
    if !cfg.defaults_repaired {
        let catalog: Vec<manager_config::CatalogModelDefaults> = manager_models::curated_models()
            .into_iter()
            .filter_map(|spec| {
                backend_for(&spec.id).map(|backend_id| manager_config::CatalogModelDefaults {
                    model_id: spec.id,
                    sha256: spec.sha256,
                    params_b: spec.params_b,
                    backend_id: backend_id.to_string(),
                })
            })
            .collect();
        dirty = manager_config::repair_catalog_defaults(
            &mut cfg,
            &catalog,
            manager_models::PLACEHOLDER_SHA256,
        );
        cfg.defaults_repaired = true;
    }
    // Catalog sync: when the curated catalog has gained entries
    // since this configuration last synced (revision bump), append
    // the missing models with their catalog defaults — opt-in
    // candidates arrive disabled and unassigned. Runs once per
    // revision, so a model the operator deleted is not resurrected
    // until the catalog next changes.
    if cfg.catalog_seeded && cfg.catalog_revision < manager_models::CATALOG_REVISION {
        let entries: Vec<manager_config::CatalogModelEntry> = manager_models::curated_models()
            .into_iter()
            .map(|spec| {
                let file_name = spec
                    .url
                    .rsplit('/')
                    .next()
                    .unwrap_or("model.gguf")
                    .to_string();
                manager_config::CatalogModelEntry {
                    model_id: spec.id.clone(),
                    gguf_file_name: file_name,
                    quant: spec.quant.clone(),
                    params_b: spec.params_b,
                    sha256: spec.sha256.clone(),
                    source_url: spec.url.clone(),
                    assigned_backends: backend_for(&spec.id)
                        .map(|b| vec![b.to_string()])
                        .unwrap_or_default(),
                    enabled: spec.enabled_by_default,
                }
            })
            .collect();
        // Definition replacements ride the same revision gate:
        // when a default model's catalog pin changed, the stored
        // definition is refreshed and the serving change that goes
        // with it is applied (see
        // manager_config::refresh_catalog_definition). Revision 2:
        // the planner's Qwen3.8-27B is GPU-resident, so the split
        // the Qwen3-32B needed is cleared; the coder's
        // Qwen3-Coder-Next is served by expert offload instead of
        // a tensor split.
        for entry in &entries {
            let Some(backend_id) = backend_for(&entry.model_id) else {
                continue;
            };
            let (clear_split, raw_flags) = match entry.model_id.as_str() {
                "qwen3-27b-planner" => (true, None),
                // The coder's riding serving change is its catalog
                // default offload (all experts) in canonical form.
                "qwen3-coder-30b" => (true, catalog_default_raw_flags(&entry.model_id)),
                _ => (false, None),
            };
            if !clear_split && raw_flags.is_none() {
                continue;
            }
            manager_config::refresh_catalog_definition(
                &mut cfg,
                &manager_config::CatalogDefinitionRefresh {
                    entry: entry.clone(),
                    backend_id: backend_id.to_string(),
                    clear_split,
                    raw_flags,
                },
            );
        }
        manager_config::append_missing_catalog_models(&mut cfg, &entries);
        cfg.catalog_revision = manager_models::CATALOG_REVISION;
        dirty = true;
    }
    if cfg.catalog_seeded {
        install_seeded(&mut live, cfg, dirty, &state.config_path);
        return;
    }
    if !cfg.models.is_empty() {
        cfg.catalog_seeded = true;
        // The catalog sync above keys off catalog_seeded, so it
        // applies from the next launch for this config.
        install_seeded(&mut live, cfg, dirty, &state.config_path);
        return;
    }
    cfg.models = manager_models::curated_models()
        .into_iter()
        .map(|spec| {
            let file_name = spec
                .url
                .rsplit('/')
                .next()
                .unwrap_or("model.gguf")
                .to_string();
            let assigned_backends = backend_for(&spec.id)
                .map(|b| vec![b.to_string()])
                .unwrap_or_default();
            manager_config::ModelConfig {
                id: spec.id,
                gguf_path: PathBuf::from("models").join(file_name),
                quant: spec.quant,
                params_b: spec.params_b,
                sha256: spec.sha256,
                verified_at: None,
                verified_sha256: None,
                source_url: spec.url,
                assigned_backends,
                enabled: spec.enabled_by_default,
                display_name: None,
                moe_layers: None,
                default_offload_layers: None,
                mtp: None,
            }
        })
        .collect();
    let seeded_paths: Vec<(String, PathBuf)> = cfg
        .models
        .iter()
        .flat_map(|m| {
            m.assigned_backends
                .iter()
                .map(move |b| (b.clone(), m.gguf_path.clone()))
        })
        .collect();
    for (backend_id, path) in seeded_paths {
        if let Some(backend) = cfg.backends.iter_mut().find(|b| b.id == backend_id) {
            if backend.model_file == format!("models/{backend_id}.gguf") {
                backend.model_file = path;
            }
            // The default fleet is enabled by default; a backend
            // whose model is not downloaded and verified yet fails
            // its start preflight rather than starting unconfigured.
            backend.enabled = true;
        }
    }
    cfg.catalog_seeded = true;
    cfg.catalog_revision = manager_models::CATALOG_REVISION;
    // A failed save leaves memory unseeded too (PS-8), so memory and
    // disk agree; the seeding repeats on the next launch.
    install_seeded(&mut live, cfg, true, &state.config_path);
}

/// The commit step of [`ensure_catalog_models`] (PS-8): install
/// `next` directly when nothing needs saving (the in-memory-only
/// flag updates), otherwise only after a successful save.
pub(crate) fn install_seeded(
    live: &mut AppConfig,
    next: AppConfig,
    needs_save: bool,
    path: &PathBuf,
) {
    if needs_save {
        if let Err(e) = manager_config::save(&next, path) {
            tracing::warn!("catalog seeding: could not save the configuration: {e}");
            return;
        }
    }
    *live = next;
}

/// Deep-merge a JSON patch into a base value (objects merge recursively).
pub(crate) fn merge_json(base: &mut Value, patch: Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                merge_json(b.entry(k).or_insert(Value::Null), v);
            }
        }
        (b, p) => *b = p,
    }
}

#[cfg(test)]
mod tests;
