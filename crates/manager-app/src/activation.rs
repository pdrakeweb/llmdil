//! Gateway activation plans: the record of saved configuration
//! changes that the running gateway has not picked up yet.
//!
//! The gateway (LiteLLM) generates its routes from the app
//! configuration when it starts; a change saved afterwards — adding
//! or removing a backend, enabling or disabling one, editing the
//! gateway section — does nothing until the gateway restarts. Before
//! this module existed, that fact surfaced only as a transient card
//! on the add-backend form: dismiss it and nothing anywhere showed
//! that a saved change was waiting on an operator decision (Pete's
//! report, 2026-10-15). The journal here is persisted, so the
//! pending plan survives view changes and app restarts, and the UI
//! presents the operator's options against it: quiesce and restart,
//! restart now, decide later, or cancel the changes.
//!
//! Resolution rule: the start-gateway step regenerates
//! `<data_dir>/gateway/config.yaml` from the live config on every
//! start, so a gateway-scope journal entry older than that file's
//! mtime was resolved by the restart that rewrote it. Backend-scope
//! entries (a backend's flags, tensor split, or model assignment —
//! changes the gateway YAML does not carry) resolve when every
//! target backend has (re)started at or after the entry, observed
//! from supervisor start times. Pending state is the union of
//! unresolved journal entries and live config drift (the generated
//! YAML differing from the file), which also catches changes made
//! outside the journaled commands.
//!
//! Unresolved is not the same as pending. A change is genuinely
//! pending only while a running process still holds the pre-change
//! state: a backend-scope entry whose targets are all stopped is
//! already effective (the next start launches from the saved
//! configuration — specs are re-registered from the live config on
//! every save), and a gateway-scope entry is already effective
//! while the gateway is down (its next start regenerates the YAML
//! from the saved configuration). [`pending_entries`] applies that
//! live-state rule; presenting an already-effective change as a
//! pending plan demands an operator decision that cannot be acted
//! on — its only offered resolution was cancelling the edit itself
//! (Pete's report, 2026-10-17).
//!
//! Cancel semantics: the first unresolved entry snapshots the
//! pre-change configuration. Cancelling restores that snapshot —
//! refused when the current configuration no longer matches the
//! last entry's post-change state, because another path changed it
//! since and a blind restore would discard that change too.

use std::path::{Path, PathBuf};

use manager_config::AppConfig;
use serde::{Deserialize, Serialize};

fn default_scope() -> String {
    "gateway".to_string()
}

/// One saved change waiting on activation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivationEntry {
    /// RFC 3339 UTC timestamp of the save.
    pub ts: String,
    /// Machine kind of the mutation (`backend_add`, `backend_delete`,
    /// `backend_apply_config`, `backend_flags`, `backend_placement`,
    /// `model_assign`, `gateway_config`, `gateway_bind`).
    pub kind: String,
    /// Operator-facing one-line summary of the change.
    pub summary: String,
    /// The full configuration as saved by this change. Cancel
    /// compares the live configuration against the newest entry's
    /// state before restoring the snapshot.
    pub config_after: serde_json::Value,
    /// What activates this change: `gateway` (a gateway restart —
    /// the pre-v0.1.32 behavior, and the default for journal entries
    /// written before scopes existed) or `backend` (the target
    /// backends' next (re)start).
    #[serde(default = "default_scope")]
    pub scope: String,
    /// Backend ids a `backend`-scope entry applies to. Empty for
    /// gateway-scope entries.
    #[serde(default)]
    pub targets: Vec<String>,
    /// Hash of the gateway YAML in effect BEFORE this change — the
    /// on-disk `config.yaml` when present, else the YAML generated
    /// from the pre-change configuration — recorded for drift
    /// visibility (MCP design §10.2). `None` when neither source
    /// exists. Serde-defaulted so journals written before this
    /// field existed still parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_change_yaml_hash: Option<String>,
}

/// A standing operator override for one backend's expert-offload
/// position: the operator explicitly saved a guardrail-blocked
/// position, so starts at exactly that position on exactly that GPU
/// placement are allowed. A placement change (a different placed
/// compute-capability set) invalidates the record — the position
/// was authorized for the GPUs it was saved against, not in the
/// abstract. Overrides are NOT pending changes: they survive
/// [`clear_journal`] and journal resolution, and are removed only
/// by an explicit clear (a verified save superseding them) or by
/// recording a replacement for the same backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OffloadOverride {
    pub backend_id: String,
    pub layers: u32,
    pub placed_ccs: Vec<(u32, u32)>,
    /// RFC 3339 UTC timestamp of the save that recorded it.
    pub ts: String,
}

/// The persisted journal: unresolved-change snapshot plus entries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActivationJournal {
    /// Configuration before the first unresolved change; the cancel
    /// target. `None` when no change is pending.
    pub snapshot: Option<serde_json::Value>,
    pub entries: Vec<ActivationEntry>,
    /// Standing offload overrides (see [`OffloadOverride`]).
    /// Defaulted so journal files written before overrides existed
    /// load with none.
    #[serde(default)]
    pub offload_overrides: Vec<OffloadOverride>,
}

fn journal_path(data_dir: &Path) -> PathBuf {
    data_dir.join("activation-journal.json")
}

fn gateway_yaml_path(data_dir: &Path) -> PathBuf {
    data_dir.join("gateway").join("config.yaml")
}

fn gateway_routing_path(data_dir: &Path) -> PathBuf {
    data_dir.join("gateway").join("llmdil_routing.py")
}

/// Serializes every journal load-modify-save in this process.
/// Commands run concurrently (a config save in one, an activation
/// completion in another), and an unsynchronized load-modify-save
/// loses entries silently — one writer's save overwrites the
/// other's just-recorded change (code-review 4.5). Every mutating
/// function below holds this lock across its whole sequence.
/// Poisoning is recovered: a panicked holder must not wedge the
/// journal for the rest of the session.
static JOURNAL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn journal_lock() -> std::sync::MutexGuard<'static, ()> {
    JOURNAL_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn load_journal(data_dir: &Path) -> ActivationJournal {
    match std::fs::read_to_string(journal_path(data_dir)) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => ActivationJournal::default(),
    }
}

/// Persist the journal, surfacing failure: a swallowed write here
/// used to strand a saved configuration change with no pending
/// plan anywhere (code-review 4.3). The write itself is
/// tmp-file + rename, so a failure leaves the previous journal
/// intact on disk.
fn save_journal(data_dir: &Path, journal: &ActivationJournal) -> Result<(), String> {
    let text = serde_json::to_string_pretty(journal)
        .map_err(|e| format!("the activation journal is not serializable: {e}"))?;
    let path = journal_path(data_dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("could not write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("could not replace {}: {e}", path.display()))?;
    Ok(())
}

/// When the running gateway's configuration was generated: the
/// mtime of the YAML the start-gateway step last wrote. Entries at
/// or before this instant were resolved by that start.
pub(crate) fn gateway_config_written_at(
    data_dir: &Path,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let meta = std::fs::metadata(gateway_yaml_path(data_dir)).ok()?;
    let modified = meta.modified().ok()?;
    Some(chrono::DateTime::<chrono::Utc>::from(modified))
}

fn entry_ts(entry: &ActivationEntry) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(&entry.ts)
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc))
}

/// The entries nothing has resolved yet, with per-scope resolution:
///
/// - `gateway` scope: the YAML mtime rule (a gateway start at or
///   after the entry resolved it).
/// - `backend` scope: resolved when every target backend has
///   (re)started at or after the entry — `started_at` carries each
///   running backend's start instant — or is no longer configured
///   (a deleted backend has nothing left to activate). A target
///   with no known start (stopped, or adopted with unknown uptime)
///   leaves the entry unresolved; the plan classifies it as
///   applies-at-next-start or needs-restart from live state.
pub fn unresolved_entries_scoped(
    data_dir: &Path,
    journal: &ActivationJournal,
    started_at: &std::collections::HashMap<String, chrono::DateTime<chrono::Utc>>,
    configured: &std::collections::HashSet<String>,
) -> Vec<ActivationEntry> {
    let written_at = gateway_config_written_at(data_dir);
    journal
        .entries
        .iter()
        .filter(|e| {
            let Some(ts) = entry_ts(e) else {
                return true; // unparseable timestamp: never resolved silently
            };
            if e.scope == "backend" {
                !e.targets.iter().all(|t| {
                    !configured.contains(t) || started_at.get(t).map(|s| *s >= ts).unwrap_or(false)
                })
            } else {
                written_at.map(|w| ts > w).unwrap_or(true)
            }
        })
        .cloned()
        .collect()
}

/// The subset of the strict-unresolved entries that is genuinely
/// pending once live process state is accounted for (see the module
/// docs): an entry is pending only while a running process still
/// holds its pre-change state.
///
/// - `backend` scope: pending when some target is running and has
///   not (re)started at or after the entry — the same staleness
///   test the plan's restart classification applies, so a pending
///   backend entry always classifies `restart`. A target that is
///   stopped makes its change already effective, never pending; a
///   running target with no known start (adopted) counts as stale,
///   so adopted processes stay pending, never silently effective.
/// - `gateway` scope: pending only while the gateway is up
///   (`gateway_up` is the caller's live probe result). A down
///   gateway serves nothing, and its next start regenerates the
///   YAML from the saved configuration.
///
/// Callers pass `running` from the same supervisor snapshot as
/// `started_at` ([`backend_running_ids`] in the Tauri layer:
/// Running or Starting processes).
pub fn pending_entries(
    data_dir: &Path,
    journal: &ActivationJournal,
    started_at: &std::collections::HashMap<String, chrono::DateTime<chrono::Utc>>,
    configured: &std::collections::HashSet<String>,
    running: &std::collections::HashSet<String>,
    gateway_up: bool,
) -> Vec<ActivationEntry> {
    unresolved_entries_scoped(data_dir, journal, started_at, configured)
        .into_iter()
        .filter(|e| {
            if e.scope == "backend" {
                let ts = entry_ts(e);
                e.targets.iter().any(|t| {
                    running.contains(t)
                        && started_at
                            .get(t)
                            .map(|s| ts.map(|ts| *s < ts).unwrap_or(true))
                            .unwrap_or(true)
                })
            } else {
                gateway_up
            }
        })
        .collect()
}

/// Whether the configuration a gateway start would generate NOW
/// differs from the files the running gateway was started with:
/// the YAML and its routing-callback sibling.
///
/// The generation input is the ARMED-only MCP view
/// ([`manager_wizard::config_with_armed_gateway_mcp_servers`]) —
/// the same view the start-gateway step writes from. Generating
/// from the raw registry instead made drift permanent whenever a
/// Gateway-exposure server was configured but unarmed (its
/// credential unresolvable): the raw YAML always carried an
/// `mcp_servers:` block the file on disk never has, so every
/// activation plan demanded a gateway restart — including
/// backend-only plans — and no restart could clear it (tracker
/// #42). `mcp_can_resolve` is the arming predicate, resolved by
/// the app layer through credential PRESENCE — the same predicate
/// (and documented gap) the wizard's own drift check uses
/// ([`manager_wizard::gateway_config_is_current`]), so the two
/// checks cannot diverge again.
///
/// A missing YAML file (gateway never started) is not drift —
/// nothing is running on a stale config.
/// Which of the gateway's generated files differ from what a
/// start would generate now — the element-level result behind
/// [`config_drift`], so the activation plan can state honestly
/// WHAT drifted. The comparison cannot say more: it knows the
/// files differ, not which edit made them differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriftDetail {
    /// `gateway/config.yaml` differs from the generated config.
    pub yaml: bool,
    /// `gateway/llmdil_routing.py` differs, is missing while a
    /// typed routing rule is enabled, or is present while none
    /// is.
    pub routing: bool,
}

impl DriftDetail {
    pub fn any(&self) -> bool {
        self.yaml || self.routing
    }
}

/// The element-level form of [`config_drift`] (whose doc comment
/// describes the comparison); identical semantics — a generation
/// failure or a missing YAML file is no drift at all, for either
/// element.
pub fn config_drift_detail(
    cfg: &AppConfig,
    data_dir: &Path,
    mcp_can_resolve: impl FnMut(&str) -> bool,
) -> DriftDetail {
    let none = DriftDetail {
        yaml: false,
        routing: false,
    };
    let filtered = manager_wizard::config_with_armed_gateway_mcp_servers(cfg, mcp_can_resolve);
    let Ok(generated) = manager_gateway::generate_config_yaml(&filtered) else {
        return none;
    };
    let Ok(on_disk) = std::fs::read_to_string(gateway_yaml_path(data_dir)) else {
        return none;
    };
    let yaml = normalize_yaml(&generated) != normalize_yaml(&on_disk);
    // The routing callback is a second generated artifact
    // (routing-rules design §6.3): a rule change can alter it
    // without altering the YAML, so it is compared too — present
    // and equal when a typed rule is enabled, absent when none
    // is — the same rule `gateway_config_is_current` applies.
    let routing_path = gateway_routing_path(data_dir);
    let routing = match manager_gateway::generate_routing_callback(&filtered) {
        Some(expected) => match std::fs::read_to_string(&routing_path) {
            Ok(disk_py) => normalize_yaml(&expected) != normalize_yaml(&disk_py),
            Err(_) => true,
        },
        None => routing_path.exists(),
    };
    DriftDetail { yaml, routing }
}

pub fn config_drift(
    cfg: &AppConfig,
    data_dir: &Path,
    mcp_can_resolve: impl FnMut(&str) -> bool,
) -> bool {
    config_drift_detail(cfg, data_dir, mcp_can_resolve).any()
}

/// Whether live config drift is pending work for an activation
/// plan: only while the gateway is up serving the stale files.
///
/// [`config_drift`] is a file comparison — the generated files
/// differ from the ones on disk. That difference is a plan fact
/// only when a running gateway is serving the on-disk version:
/// while the gateway is down, its next start regenerates both
/// files from the saved configuration, so the difference resolves
/// by itself and is not the plan's to state. The plan payload
/// leaked the raw bit instead, so a backend-only plan (scope:
/// backend, apply: restart one backend) asserted "Gateway config:
/// drifted" directly above an apply line that did not touch the
/// gateway — and the line's explanation claimed a RUNNING
/// gateway's configuration differed while the gateway was down
/// (Pete's report, 2026-10-18, tracker #45). This is the same
/// live-state rule [`pending_entries`] applies to gateway-scope
/// journal entries and `gateway_status` applies to its drift
/// count; the plan's drift field was the one place that skipped
/// it. Applying a backend-only plan neither addresses nor clears
/// a down-gateway file difference, so the plan must not present
/// it as pending work in either direction.
pub fn drift_is_pending(drift: bool, gateway_up: bool) -> bool {
    drift && gateway_up
}

/// The activation plan's change item for live config drift, so
/// drift that makes a plan pending is also an item the plan
/// COUNTS and LISTS (tracker #57). Before this, drift entered
/// the plan's `pending`/`gateway_pending`/`actionable` as a bare
/// boolean while `changes` came only from the journal, so a
/// drift-only state rendered an actionable plan with zero items
/// on every surface — the banner's "Pending changes: 0" beside
/// Drain/Apply buttons, the plan page's 0/0/0 tiles above an
/// enabled Apply. The journal can never contain this item:
/// drift's inputs include journal-free state — an MCP credential
/// stored or removed flips the armed view (hence the generated
/// YAML) with no config save — so the item is synthesized at
/// plan computation, never journaled. Applying the plan is its
/// resolution: the gateway restart regenerates both files.
///
/// The summary states only what the comparison can honestly
/// state — WHICH generated file differs. It must not name a
/// cause: the credential-arming flip above leaves no trace the
/// comparison could attribute. `written_at` (the YAML's mtime —
/// when the running gateway's files were generated) is the
/// item's timestamp; the current instant stands in when it is
/// unknown.
pub fn drift_plan_change(
    detail: &DriftDetail,
    written_at: Option<chrono::DateTime<chrono::Utc>>,
) -> serde_json::Value {
    let (what, noun) = match (detail.yaml, detail.routing) {
        (true, true) => (
            "The gateway config file (config.yaml) and the routing callback file (llmdil_routing.py) differ",
            "files",
        ),
        (true, false) => ("The gateway config file (config.yaml) differs", "file"),
        (false, true) => (
            "The routing callback file (llmdil_routing.py) differs",
            "file",
        ),
        (false, false) => ("The gateway's generated files differ", "files"),
    };
    let ts = written_at
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    serde_json::json!({
        "ts": ts,
        "kind": "gateway_drift",
        "summary": format!(
            "{what} from the saved configuration — the running gateway is serving the older {noun}."
        ),
        "scope": "gateway",
        "targets": [],
        "activation": "gateway_restart",
    })
}

fn normalize_yaml(text: &str) -> String {
    text.replace("\r\n", "\n").trim_end().to_string()
}

/// Record a saved change. `pre` is the configuration before the
/// change, `post` the configuration as saved. When every existing
/// entry is already resolved, the journal restarts from `pre` so
/// the snapshot always matches the oldest unresolved change.
/// `scope`/`targets` say what activates the change (see
/// [`ActivationEntry::scope`]).
///
/// Resolution of the EXISTING entries uses the full scoped rule —
/// the caller passes the running backends' real start times
/// (`started_at`), the configured backend ids, and the running set
/// (`running`). One prune goes further than the scoped rule:
/// a backend-scope entry whose targets are all stopped is already
/// effective (see [`pending_entries`]), so it prunes here too —
/// otherwise it would anchor the cancel snapshot across a change
/// no plan will ever show as pending. The running set is exact
/// supervisor state, like the start times, so this cannot prune a
/// change a running backend still holds. Gateway-scope entries
/// prune only on the mtime rule: gateway liveness is a probe the
/// record path does not perform. The gateway-only
/// approximation this used before treated every backend-scope
/// entry as unresolved forever (its start-time map was empty), so
/// the prune never fired for them: entries accumulated past their
/// activation, and a later cancel could restore a snapshot over
/// changes that were already live (code-review 4.1).
///
/// Callers record the change BEFORE persisting the configuration
/// and propagate the `Err`: a journal write failure then aborts
/// the change with the configuration untouched, instead of
/// stranding a saved change no plan will ever show (4.3).
#[allow(clippy::too_many_arguments)]
pub fn record_change(
    data_dir: &Path,
    pre: &AppConfig,
    post: &AppConfig,
    kind: &str,
    summary: String,
    scope: &str,
    targets: Vec<String>,
    started_at: &std::collections::HashMap<String, chrono::DateTime<chrono::Utc>>,
    configured: &std::collections::HashSet<String>,
    running: &std::collections::HashSet<String>,
) -> Result<(), String> {
    let _guard = journal_lock();
    let mut journal = load_journal(data_dir);
    let mut unresolved = unresolved_entries_scoped(data_dir, &journal, started_at, configured);
    // Already-effective prune (see the doc comment): a backend
    // entry with no running target is the configuration the next
    // start launches with, so it leaves the journal here instead of
    // anchoring the snapshot. An entry with any running target is
    // kept whole — the running target's activation is still owed.
    unresolved.retain(|e| e.scope != "backend" || e.targets.iter().any(|t| running.contains(t)));
    if unresolved.len() != journal.entries.len() || journal.entries.is_empty() {
        journal.entries = unresolved;
        if journal.entries.is_empty() {
            journal.snapshot = serde_json::to_value(pre).ok();
        }
    }
    journal.entries.push(ActivationEntry {
        ts: chrono::Utc::now().to_rfc3339(),
        kind: kind.to_string(),
        summary,
        config_after: serde_json::to_value(post).unwrap_or(serde_json::Value::Null),
        scope: scope.to_string(),
        targets,
        pre_change_yaml_hash: yaml_hash_before_change(data_dir, pre),
    });
    save_journal(data_dir, &journal)
}

/// The hash of the gateway YAML in effect before a change: the
/// on-disk `config.yaml` when it exists and is non-empty (that is
/// the YAML the running gateway was started with), else the YAML
/// generated from the pre-change configuration. `None` when
/// neither is available (no file yet and generation fails, e.g. a
/// pre-change config with no enabled groups).
fn yaml_hash_before_change(data_dir: &Path, pre: &AppConfig) -> Option<String> {
    if let Ok(text) = std::fs::read_to_string(gateway_yaml_path(data_dir)) {
        if !text.trim().is_empty() {
            return Some(manager_wizard::gateway_yaml_hash(&text));
        }
    }
    manager_gateway::generate_config_yaml(pre)
        .ok()
        .map(|yaml| manager_wizard::gateway_yaml_hash(&yaml))
}

/// Record a standing offload override for `backend_id`, replacing
/// any existing records for that backend (one standing override per
/// backend: the newest explicit save is the authorization in force).
pub fn record_offload_override(
    data_dir: &Path,
    backend_id: &str,
    layers: u32,
    placed_ccs: Vec<(u32, u32)>,
) -> Result<(), String> {
    let _guard = journal_lock();
    let mut journal = load_journal(data_dir);
    journal
        .offload_overrides
        .retain(|o| o.backend_id != backend_id);
    journal.offload_overrides.push(OffloadOverride {
        backend_id: backend_id.to_string(),
        layers,
        placed_ccs,
        ts: chrono::Utc::now().to_rfc3339(),
    });
    save_journal(data_dir, &journal)
}

/// Drop `backend_id`'s standing offload overrides, if any. Called
/// when a verified (non-blocked) save supersedes the override.
pub fn clear_offload_overrides(data_dir: &Path, backend_id: &str) -> Result<(), String> {
    let _guard = journal_lock();
    let mut journal = load_journal(data_dir);
    journal
        .offload_overrides
        .retain(|o| o.backend_id != backend_id);
    save_journal(data_dir, &journal)
}

/// Whether a journaled override covers exactly this position on
/// exactly this placement: same backend, same layer count, and the
/// same placed compute-capability set (order-insensitive).
pub fn offload_override_covers(
    journal: &ActivationJournal,
    backend_id: &str,
    layers: u32,
    placed_ccs: &[(u32, u32)],
) -> bool {
    let want: std::collections::BTreeSet<(u32, u32)> = placed_ccs.iter().copied().collect();
    journal.offload_overrides.iter().any(|o| {
        o.backend_id == backend_id
            && o.layers == layers
            && o.placed_ccs.iter().copied().collect::<std::collections::BTreeSet<_>>() == want
    })
}

/// Clear the journal (activation completed, or changes cancelled).
/// Offload overrides are PRESERVED: an override is a standing
/// operator decision about a value, not a pending change, so
/// resolving or cancelling the plan must not silently revoke it.
pub fn clear_journal(data_dir: &Path) -> Result<(), String> {
    let _guard = journal_lock();
    let journal = load_journal(data_dir);
    save_journal(
        data_dir,
        &ActivationJournal {
            snapshot: None,
            entries: Vec::new(),
            offload_overrides: journal.offload_overrides,
        },
    )
}

/// Clear only the gateway-scope entries (a gateway (re)start made
/// them live). Backend-scope entries stay: they resolve through
/// their backends' (re)starts, which the scoped rule observes. When
/// no entries remain the snapshot goes too — it is the cancel
/// target for pending entries only.
pub fn clear_gateway_scope(data_dir: &Path) -> Result<(), String> {
    let _guard = journal_lock();
    let mut journal = load_journal(data_dir);
    journal.entries.retain(|e| e.scope != "gateway");
    if journal.entries.is_empty() {
        journal.snapshot = None;
    }
    save_journal(data_dir, &journal)
}

/// The cancel target and its guard: the snapshot to restore, when
/// the live configuration still matches the newest pending entry's
/// post-change state. `Err` explains why cancel is refused.
///
/// Pending is computed with live state ([`pending_entries`], on top
/// of the full scoped rule): a backend-scope entry whose backends
/// already restarted is LIVE, not pending — cancelling must not
/// "restore" over it (code-review 4.1) — and an entry whose targets
/// are all stopped (or whose gateway is down) is already effective
/// the same way, so it is not the plan's to cancel either. When
/// nothing is pending the answer is `None`, however many
/// already-effective entries the journal still carries.
pub fn cancel_target(
    data_dir: &Path,
    current: &AppConfig,
    started_at: &std::collections::HashMap<String, chrono::DateTime<chrono::Utc>>,
    configured: &std::collections::HashSet<String>,
    running: &std::collections::HashSet<String>,
    gateway_up: bool,
) -> Result<Option<AppConfig>, String> {
    let _guard = journal_lock();
    let journal = load_journal(data_dir);
    let unresolved = unresolved_entries_scoped(data_dir, &journal, started_at, configured);
    let pending = pending_entries(
        data_dir, &journal, started_at, configured, running, gateway_up,
    );
    let Some(last) = pending.last() else {
        return Ok(None);
    };
    // Restoring the snapshot reverts EVERY journaled change back to
    // it, including strict-unresolved entries that live state has
    // already made effective (their targets are stopped; the change
    // applies at the next start and is not pending). When any such
    // entry exists, cancelling the pending set would silently
    // discard it too — refuse, on the same principle as the
    // post-change-state guard below.
    if unresolved.len() != pending.len() {
        return Err(
            "some saved changes are already in effect because their targets are stopped — \
             cancelling would discard those changes too, so the saved changes stand"
                .to_string(),
        );
    }
    let current_value = serde_json::to_value(current)
        .map_err(|e| format!("current configuration is not serializable: {e}"))?;
    if current_value != last.config_after {
        return Err(
            "the configuration changed through another path after these changes were saved; \
             cancelling would discard that change too, so the saved changes stand"
                .to_string(),
        );
    }
    let Some(snapshot) = journal.snapshot else {
        return Ok(None);
    };
    let cfg: AppConfig = serde_json::from_value(snapshot)
        .map_err(|e| format!("the saved snapshot cannot be restored: {e}"))?;
    Ok(Some(cfg))
}

/// Sum the `litellm_in_flight_requests` gauge from a Prometheus
/// exposition body (all label sets). `None` when the series is
/// absent — the callback that mounts /metrics may be enabled while
/// the gauge is only emitted once traffic has flowed.
pub fn parse_in_flight(metrics: &str) -> Option<f64> {
    let mut total = 0.0f64;
    let mut found = false;
    for line in metrics.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix("litellm_in_flight_requests") else {
            continue;
        };
        // Series line: optional {labels} then whitespace + value.
        let value_part = match rest.strip_prefix('{') {
            Some(with_labels) => match with_labels.rsplit_once('}') {
                Some((_, tail)) => tail.trim(),
                None => continue,
            },
            None => rest.trim(),
        };
        if let Ok(v) = value_part.parse::<f64>() {
            total += v;
            found = true;
        }
    }
    found.then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_flight_sums_labelled_and_plain_series() {
        let body = "\
# HELP litellm_in_flight_requests In flight
litellm_in_flight_requests{model=\"planner\"} 2.0
litellm_in_flight_requests 1.0
litellm_deployment_success_responses_total 9.0
";
        assert_eq!(parse_in_flight(body), Some(3.0));
        assert_eq!(parse_in_flight("litellm_other 1.0\n"), None);
    }

    #[test]
    fn normalize_yaml_ignores_crlf_and_trailing_newlines() {
        assert_eq!(normalize_yaml("a\r\nb\r\n"), normalize_yaml("a\nb\n\n"));
    }

    fn entry(scope: &str, targets: &[&str]) -> ActivationEntry {
        ActivationEntry {
            ts: "2026-10-16T12:00:00+00:00".to_string(),
            kind: "backend_flags".to_string(),
            summary: "change".to_string(),
            config_after: serde_json::Value::Null,
            scope: scope.to_string(),
            targets: targets.iter().map(|s| s.to_string()).collect(),
            pre_change_yaml_hash: None,
        }
    }

    fn at(rfc3339: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn backend_scope_resolves_on_restart_or_delete() {
        // No gateway YAML under this path, so gateway mtime never
        // resolves anything; backend scope resolves on start times.
        let dir = Path::new("/nonexistent-activation-test");
        let journal = ActivationJournal {
            snapshot: None,
            entries: vec![entry("backend", &["a", "b"])],
            ..Default::default()
        };
        let configured: std::collections::HashSet<String> =
            ["a".to_string(), "b".to_string()].into_iter().collect();
        let started = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), at(v)))
                .collect::<std::collections::HashMap<_, _>>()
        };
        // b has not restarted since the change: unresolved.
        let un = unresolved_entries_scoped(
            dir,
            &journal,
            &started(&[("a", "2026-10-16T13:00:00+00:00")]),
            &configured,
        );
        assert_eq!(un.len(), 1);
        // Both restarted after the entry: resolved.
        let un = unresolved_entries_scoped(
            dir,
            &journal,
            &started(&[
                ("a", "2026-10-16T13:00:00+00:00"),
                ("b", "2026-10-16T12:30:00+00:00"),
            ]),
            &configured,
        );
        assert!(un.is_empty());
        // A restart from BEFORE the entry does not resolve it.
        let un = unresolved_entries_scoped(
            dir,
            &journal,
            &started(&[
                ("a", "2026-10-16T11:00:00+00:00"),
                ("b", "2026-10-16T13:00:00+00:00"),
            ]),
            &configured,
        );
        assert_eq!(un.len(), 1);
        // A deleted target has nothing to activate: resolved when
        // the remaining target restarted.
        let configured: std::collections::HashSet<String> = ["a".to_string()].into_iter().collect();
        let un = unresolved_entries_scoped(
            dir,
            &journal,
            &started(&[("a", "2026-10-16T13:00:00+00:00")]),
            &configured,
        );
        assert!(un.is_empty());
        // Gateway scope ignores start times entirely (mtime rule).
        let journal = ActivationJournal {
            snapshot: None,
            entries: vec![entry("gateway", &[])],
            ..Default::default()
        };
        let un = unresolved_entries_scoped(
            dir,
            &journal,
            &started(&[("a", "2026-10-16T13:00:00+00:00")]),
            &configured,
        );
        assert_eq!(un.len(), 1);
    }

    #[test]
    fn record_and_cancel_use_scoped_resolution() {
        // Regression (code-review 4.1): record_change's prune and
        // cancel_target previously resolved with the gateway-only
        // rule, under which a backend-scope entry NEVER resolved —
        // a change already live via a backend restart stayed
        // "pending", and cancel would restore the snapshot over it.
        let dir = std::env::temp_dir().join(format!("activation-scoped-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = AppConfig::default_config();
        let journal = ActivationJournal {
            snapshot: Some(serde_json::to_value(&cfg).unwrap()),
            entries: vec![entry("backend", &["a"])],
            ..Default::default()
        };
        save_journal(&dir, &journal).unwrap();
        let configured: std::collections::HashSet<String> = ["a".to_string()].into_iter().collect();
        // Backend "a" restarted at 13:00, after the 12:00 entry.
        let started: std::collections::HashMap<String, chrono::DateTime<chrono::Utc>> =
            [("a".to_string(), at("2026-10-16T13:00:00+00:00"))]
                .into_iter()
                .collect();
        // The entry is live: nothing to cancel.
        let running: std::collections::HashSet<String> = ["a".to_string()].into_iter().collect();
        assert!(
            cancel_target(&dir, &cfg, &started, &configured, &running, true)
                .unwrap()
                .is_none()
        );
        // Recording a new change prunes the resolved entry and
        // restarts the snapshot from the new change's `pre`.
        let pre = cfg.clone();
        let mut post = cfg.clone();
        post.gateway.port += 1;
        record_change(
            &dir,
            &pre,
            &post,
            "gateway_config",
            "port".to_string(),
            "gateway",
            Vec::new(),
            &started,
            &configured,
            &running,
        )
        .unwrap();
        let journal = load_journal(&dir);
        assert_eq!(journal.entries.len(), 1);
        assert_eq!(journal.entries[0].kind, "gateway_config");
        let snap: AppConfig =
            serde_json::from_value(journal.snapshot.expect("snapshot restarted")).unwrap();
        assert_eq!(snap.gateway.port, cfg.gateway.port);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_change_records_pre_change_yaml_hash() {
        let dir = std::env::temp_dir().join(format!("activation-yaml-hash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = AppConfig::default_config();
        let mut post = cfg.clone();
        post.gateway.port += 1;
        let started = std::collections::HashMap::new();
        let configured = std::collections::HashSet::new();
        let running = std::collections::HashSet::new();
        record_change(
            &dir,
            &cfg,
            &post,
            "gateway_config",
            "port".to_string(),
            "gateway",
            Vec::new(),
            &started,
            &configured,
            &running,
        )
        .unwrap();
        let journal = load_journal(&dir);
        assert_eq!(journal.entries.len(), 1);
        // No on-disk YAML in the fresh dir: the hash is of the YAML
        // generated from the pre-change config (when generation
        // succeeds for it).
        match manager_gateway::generate_config_yaml(&cfg) {
            Ok(yaml) => assert_eq!(
                journal.entries[0].pre_change_yaml_hash.as_deref(),
                Some(manager_wizard::gateway_yaml_hash(&yaml).as_str())
            ),
            Err(_) => assert!(journal.entries[0].pre_change_yaml_hash.is_none()),
        }
        // With an on-disk config.yaml present, its hash wins — it
        // is the YAML in effect, whatever the config now says.
        let yaml_path = gateway_yaml_path(&dir);
        std::fs::create_dir_all(yaml_path.parent().unwrap()).unwrap();
        std::fs::write(&yaml_path, "sentinel: true\n").unwrap();
        let mut post2 = post.clone();
        post2.gateway.port += 1;
        record_change(
            &dir,
            &post,
            &post2,
            "gateway_config",
            "port again".to_string(),
            "gateway",
            Vec::new(),
            &started,
            &configured,
            &running,
        )
        .unwrap();
        let journal = load_journal(&dir);
        let last = journal.entries.last().unwrap();
        assert_eq!(
            last.pre_change_yaml_hash.as_deref(),
            Some(manager_wizard::gateway_yaml_hash("sentinel: true\n").as_str())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_entry_without_scope_defaults_to_gateway() {
        let text = r#"{"ts":"2026-10-16T12:00:00+00:00","kind":"backend_add","summary":"x","config_after":null}"#;
        let e: ActivationEntry = serde_json::from_str(text).unwrap();
        assert_eq!(e.scope, "gateway");
        assert!(e.targets.is_empty());
        assert!(e.pre_change_yaml_hash.is_none());
    }

    fn ids(names: &[&str]) -> std::collections::HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn starts(
        pairs: &[(&str, &str)],
    ) -> std::collections::HashMap<String, chrono::DateTime<chrono::Utc>> {
        pairs.iter().map(|(k, v)| (k.to_string(), at(v))).collect()
    }

    #[test]
    fn stopped_backend_change_is_not_pending() {
        // Pete's report (2026-10-17): an offload edit on a stopped
        // backend produced a pending plan with no Apply. The entry
        // is strict-unresolved (the backend has never started since)
        // but already effective — nothing runs the old flags, and
        // the next start launches from the saved configuration.
        let dir = Path::new("/nonexistent-activation-test");
        let journal = ActivationJournal {
            snapshot: None,
            entries: vec![entry("backend", &["coder-nvidia"])],
            ..Default::default()
        };
        let configured = ids(&["coder-nvidia"]);
        let pending = pending_entries(dir, &journal, &starts(&[]), &configured, &ids(&[]), false);
        assert!(pending.is_empty());
        // The same for a gateway-scope entry while the gateway is
        // down: its next start regenerates the YAML from the saved
        // configuration, so the change is effective, not pending.
        let journal = ActivationJournal {
            snapshot: None,
            entries: vec![entry("gateway", &[])],
            ..Default::default()
        };
        let pending = pending_entries(dir, &journal, &starts(&[]), &configured, &ids(&[]), false);
        assert!(pending.is_empty());
        // ...but it IS pending while the gateway is up.
        let pending = pending_entries(dir, &journal, &starts(&[]), &configured, &ids(&[]), true);
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn running_backend_change_is_pending() {
        let dir = Path::new("/nonexistent-activation-test");
        let journal = ActivationJournal {
            snapshot: None,
            entries: vec![entry("backend", &["coder"])],
            ..Default::default()
        };
        let configured = ids(&["coder"]);
        // Running, started BEFORE the 12:00 entry: the process
        // holds the pre-change state — pending (restart class).
        let pending = pending_entries(
            dir,
            &journal,
            &starts(&[("coder", "2026-10-16T11:00:00+00:00")]),
            &configured,
            &ids(&["coder"]),
            false,
        );
        assert_eq!(pending.len(), 1);
        // Running with NO known start (adopted): stays pending —
        // unknown uptime must never read as already effective.
        let pending = pending_entries(
            dir,
            &journal,
            &starts(&[]),
            &configured,
            &ids(&["coder"]),
            false,
        );
        assert_eq!(pending.len(), 1);
        // Running, (re)started AFTER the entry: the change is live
        // (the strict rule already resolved it) — not pending.
        let pending = pending_entries(
            dir,
            &journal,
            &starts(&[("coder", "2026-10-16T13:00:00+00:00")]),
            &configured,
            &ids(&["coder"]),
            false,
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn mixed_plan_only_the_running_targets_entry_is_pending() {
        let dir = Path::new("/nonexistent-activation-test");
        let journal = ActivationJournal {
            snapshot: None,
            entries: vec![
                entry("backend", &["coder-nvidia"]),
                entry("backend", &["coder"]),
            ],
            ..Default::default()
        };
        let configured = ids(&["coder-nvidia", "coder"]);
        let pending = pending_entries(
            dir,
            &journal,
            &starts(&[("coder", "2026-10-16T11:00:00+00:00")]),
            &configured,
            &ids(&["coder"]),
            false,
        );
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].targets, vec!["coder".to_string()]);
    }

    #[test]
    fn next_start_of_stopped_backend_leaves_no_residue() {
        // The stopped backend's entry is already effective before
        // the start; after the start the strict rule resolves it,
        // and the next record_change prunes it from the journal —
        // no residue in the plan or the journal.
        let dir = std::env::temp_dir().join(format!(
            "activation-next-start-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = AppConfig::default_config();
        let journal = ActivationJournal {
            snapshot: Some(serde_json::to_value(&cfg).unwrap()),
            entries: vec![entry("backend", &["coder"])],
            ..Default::default()
        };
        save_journal(&dir, &journal).unwrap();
        let configured = ids(&["coder"]);
        // Before the start: not pending (already effective).
        assert!(
            pending_entries(&dir, &journal, &starts(&[]), &configured, &ids(&[]), false,)
                .is_empty()
        );
        // After a start at 13:00 (entry is 12:00): strictly
        // resolved, and recording a further change prunes it.
        let started = starts(&[("coder", "2026-10-16T13:00:00+00:00")]);
        assert!(unresolved_entries_scoped(&dir, &journal, &started, &configured).is_empty());
        let mut post = cfg.clone();
        post.gateway.port += 1;
        record_change(
            &dir,
            &cfg,
            &post,
            "gateway_config",
            "port".to_string(),
            "gateway",
            Vec::new(),
            &started,
            &configured,
            &ids(&["coder"]),
        )
        .unwrap();
        let journal = load_journal(&dir);
        assert_eq!(journal.entries.len(), 1);
        assert_eq!(journal.entries[0].kind, "gateway_config");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_change_prunes_an_already_effective_backend_entry() {
        // A change recorded for a stopped backend, then another:
        // the first entry prunes at the second record and the
        // snapshot restarts from the second change's `pre` — the
        // journal never accumulates already-effective entries.
        let dir = std::env::temp_dir().join(format!(
            "activation-effective-prune-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = AppConfig::default_config();
        let configured = ids(&["coder"]);
        let no_starts = starts(&[]);
        let nobody_running = ids(&[]);
        let mut post1 = cfg.clone();
        post1.gateway.port += 1;
        record_change(
            &dir,
            &cfg,
            &post1,
            "backend_flags",
            "first".to_string(),
            "backend",
            vec!["coder".to_string()],
            &no_starts,
            &configured,
            &nobody_running,
        )
        .unwrap();
        let mut post2 = post1.clone();
        post2.gateway.port += 1;
        record_change(
            &dir,
            &post1,
            &post2,
            "backend_flags",
            "second".to_string(),
            "backend",
            vec!["coder".to_string()],
            &no_starts,
            &configured,
            &nobody_running,
        )
        .unwrap();
        let journal = load_journal(&dir);
        assert_eq!(journal.entries.len(), 1);
        assert_eq!(journal.entries[0].summary, "second");
        let snap: AppConfig =
            serde_json::from_value(journal.snapshot.expect("snapshot restarted")).unwrap();
        assert_eq!(snap.gateway.port, post1.gateway.port);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancel_uses_live_pending_state() {
        let dir = std::env::temp_dir().join(format!(
            "activation-cancel-live-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg0 = AppConfig::default_config();
        let mut cfg1 = cfg0.clone();
        cfg1.gateway.port += 1;
        let with_after = |scope: &str, targets: &[&str], after: &AppConfig| {
            let mut e = entry(scope, targets);
            e.config_after = serde_json::to_value(after).unwrap();
            e
        };
        let configured = ids(&["coder", "coder-nvidia"]);
        // Only an already-effective change (stopped target):
        // nothing to cancel — the edit stands (Pete's case).
        let journal = ActivationJournal {
            snapshot: Some(serde_json::to_value(&cfg0).unwrap()),
            entries: vec![with_after("backend", &["coder-nvidia"], &cfg1)],
            ..Default::default()
        };
        save_journal(&dir, &journal).unwrap();
        assert!(
            cancel_target(&dir, &cfg1, &starts(&[]), &configured, &ids(&[]), false,)
                .unwrap()
                .is_none()
        );
        // A genuinely pending change (running target, started
        // before the entry): cancel restores the snapshot.
        let journal = ActivationJournal {
            snapshot: Some(serde_json::to_value(&cfg0).unwrap()),
            entries: vec![with_after("backend", &["coder"], &cfg1)],
            ..Default::default()
        };
        save_journal(&dir, &journal).unwrap();
        let started = starts(&[("coder", "2026-10-16T11:00:00+00:00")]);
        let restored = cancel_target(&dir, &cfg1, &started, &configured, &ids(&["coder"]), false)
            .unwrap()
            .expect("pending change cancels");
        assert_eq!(restored.gateway.port, cfg0.gateway.port);
        // Pending + already-effective together: cancel refuses —
        // restoring the snapshot would discard the effective change
        // along with the pending one.
        let mut cfg2 = cfg1.clone();
        cfg2.gateway.port += 1;
        let journal = ActivationJournal {
            snapshot: Some(serde_json::to_value(&cfg0).unwrap()),
            entries: vec![
                with_after("backend", &["coder"], &cfg1),
                with_after("backend", &["coder-nvidia"], &cfg2),
            ],
            ..Default::default()
        };
        save_journal(&dir, &journal).unwrap();
        let err = cancel_target(&dir, &cfg2, &started, &configured, &ids(&["coder"]), false)
            .expect_err("mixed pending + effective must refuse");
        assert!(
            err.contains("already in effect"),
            "unexpected refusal: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn override_test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "activation-offload-override-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn offload_override_record_covers_round_trip() {
        let dir = override_test_dir("round-trip");
        record_offload_override(&dir, "coder", 46, vec![(6, 0)]).unwrap();
        let journal = load_journal(&dir);
        assert!(offload_override_covers(&journal, "coder", 46, &[(6, 0)]));
        // Another backend at the same position is not covered.
        assert!(!offload_override_covers(&journal, "planner", 46, &[(6, 0)]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn offload_override_wrong_layers_does_not_cover() {
        let dir = override_test_dir("wrong-layers");
        record_offload_override(&dir, "coder", 46, vec![(6, 0)]).unwrap();
        let journal = load_journal(&dir);
        assert!(!offload_override_covers(&journal, "coder", 45, &[(6, 0)]));
        assert!(!offload_override_covers(&journal, "coder", 48, &[(6, 0)]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn offload_override_different_placed_cc_set_does_not_cover() {
        // A placement change invalidates the override: the position
        // was authorized against the placed GPUs it was saved on.
        let dir = override_test_dir("different-ccs");
        record_offload_override(&dir, "coder", 46, vec![(6, 0), (8, 6)]).unwrap();
        let journal = load_journal(&dir);
        // Order does not matter: the same set reversed still covers.
        assert!(offload_override_covers(&journal, "coder", 46, &[(8, 6), (6, 0)]));
        assert!(!offload_override_covers(&journal, "coder", 46, &[(6, 0)]));
        assert!(!offload_override_covers(&journal, "coder", 46, &[(8, 6)]));
        assert!(!offload_override_covers(&journal, "coder", 46, &[]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn offload_override_rerecord_replaces() {
        let dir = override_test_dir("rerecord");
        record_offload_override(&dir, "coder", 46, vec![(6, 0)]).unwrap();
        record_offload_override(&dir, "coder", 24, vec![(6, 0)]).unwrap();
        let journal = load_journal(&dir);
        assert_eq!(journal.offload_overrides.len(), 1);
        assert!(!offload_override_covers(&journal, "coder", 46, &[(6, 0)]));
        assert!(offload_override_covers(&journal, "coder", 24, &[(6, 0)]));
        // Clearing drops only that backend's records.
        record_offload_override(&dir, "planner", 10, vec![(6, 0)]).unwrap();
        clear_offload_overrides(&dir, "coder").unwrap();
        let journal = load_journal(&dir);
        assert!(!offload_override_covers(&journal, "coder", 24, &[(6, 0)]));
        assert!(offload_override_covers(&journal, "planner", 10, &[(6, 0)]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_journal_preserves_offload_overrides() {
        let dir = override_test_dir("clear-preserves");
        record_offload_override(&dir, "coder", 46, vec![(6, 0)]).unwrap();
        clear_journal(&dir).unwrap();
        let journal = load_journal(&dir);
        assert!(journal.entries.is_empty());
        assert!(journal.snapshot.is_none());
        assert!(offload_override_covers(&journal, "coder", 46, &[(6, 0)]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_journal_without_overrides_field_loads_empty() {
        let dir = override_test_dir("legacy");
        std::fs::write(
            dir.join("activation-journal.json"),
            r#"{"snapshot":null,"entries":[]}"#,
        )
        .unwrap();
        let journal = load_journal(&dir);
        assert!(journal.offload_overrides.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn drift_test_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "activation-drift-{label}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join("gateway")).unwrap();
        dir
    }

    #[test]
    fn config_drift_ignores_an_unarmed_gateway_mcp_server() {
        // Regression (tracker #42): the start-gateway step writes
        // the YAML from the armed-only MCP view, so a Gateway
        // server whose credential does not resolve is omitted from
        // the file. Drift generated from the raw registry carried
        // that server's mcp_servers block forever: every plan —
        // including backend-only ones — demanded a gateway restart
        // no restart could satisfy. The default config's curated
        // seed is exactly the production case: brave_search is
        // Gateway-exposed with an env key whose credential most
        // installs have not stored.
        let dir = drift_test_dir("unarmed");
        let mut cfg = AppConfig::default_config();
        for b in cfg.backends.iter_mut() {
            b.enabled = true;
        }
        assert!(cfg
            .mcp
            .servers
            .iter()
            .any(|s| s.exposure == manager_config::McpExposure::Gateway
                && !s.env_keys.is_empty()));
        // The file a start writes while the server is unarmed.
        let unarmed = manager_wizard::config_with_armed_gateway_mcp_servers(&cfg, |_| false);
        let yaml = manager_gateway::generate_config_yaml(&unarmed).unwrap();
        assert!(!yaml.contains("mcp_servers:"), "{yaml}");
        std::fs::write(gateway_yaml_path(&dir), &yaml).unwrap();
        assert!(!config_drift(&cfg, &dir, |_| false));
        // The same file reads as drift once the credential
        // resolves: a start now would write the server in.
        assert!(config_drift(&cfg, &dir, |_| true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_drift_detects_a_real_gateway_change() {
        let dir = drift_test_dir("real-change");
        let mut cfg = AppConfig::default_config();
        for b in cfg.backends.iter_mut() {
            b.enabled = true;
        }
        let armed = manager_wizard::config_with_armed_gateway_mcp_servers(&cfg, |_| true);
        let yaml = manager_gateway::generate_config_yaml(&armed).unwrap();
        assert!(yaml.contains("mcp_servers:"), "{yaml}");
        std::fs::write(gateway_yaml_path(&dir), &yaml).unwrap();
        assert!(!config_drift(&cfg, &dir, |_| true));
        // A saved change the running gateway has not picked up is
        // still drift: a backend's port is carried in the YAML as
        // the deployment's api_base. (The gateway's own listen
        // port is not — it travels via the spawn argv.)
        cfg.backends[0].port += 1;
        assert!(config_drift(&cfg, &dir, |_| true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_drift_covers_the_routing_callback_file() {
        // A stale llmdil_routing.py beside an identical YAML is
        // drift (the wizard's check has always compared it); a
        // callback file with no enabled rule is drift too.
        let dir = drift_test_dir("routing");
        let mut cfg = AppConfig::default_config();
        for b in cfg.backends.iter_mut() {
            b.enabled = true;
        }
        // The file a start writes: the armed-only generation.
        let view = manager_wizard::config_with_armed_gateway_mcp_servers(&cfg, |_| false);
        let yaml = manager_gateway::generate_config_yaml(&view).unwrap();
        std::fs::write(gateway_yaml_path(&dir), &yaml).unwrap();
        assert!(!config_drift(&cfg, &dir, |_| false));
        std::fs::write(gateway_routing_path(&dir), "# stale callback\n").unwrap();
        assert!(config_drift(&cfg, &dir, |_| false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_drift_is_pending_only_while_the_gateway_is_up() {
        // The raw file comparison is true whenever the on-disk
        // YAML is stale; as PLAN state it counts only while a
        // running gateway serves that file. Down, the next start
        // regenerates it — nothing is pending and nothing may be
        // stated as the plan's reason (tracker #45).
        let dir = drift_test_dir("plan-gate");
        let mut cfg = AppConfig::default_config();
        for b in cfg.backends.iter_mut() {
            b.enabled = true;
        }
        let view = manager_wizard::config_with_armed_gateway_mcp_servers(&cfg, |_| false);
        let yaml = manager_gateway::generate_config_yaml(&view).unwrap();
        std::fs::write(gateway_yaml_path(&dir), &yaml).unwrap();
        cfg.backends[0].port += 1;
        let raw = config_drift(&cfg, &dir, |_| false);
        assert!(raw, "the stale file is raw drift");
        assert!(!drift_is_pending(raw, false));
        assert!(drift_is_pending(raw, true));
        assert!(!drift_is_pending(false, true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backend_only_plan_with_a_stale_gateway_file_reports_no_gateway_drift() {
        // Regression (tracker #45), the production shape: one
        // backend-scope journal entry (a coder flags save — flags
        // are not carried in the gateway YAML, so no gateway entry
        // exists), the gateway DOWN (Pete's start had failed), and
        // an on-disk gateway YAML left stale by an earlier start.
        // The plan built from this state must show exactly the
        // backend change and no gateway drift: the raw drift bit
        // leaked into the plan payload and the card asserted
        // "Gateway config: drifted" above "Apply restarts coder.",
        // a restart that neither addresses nor clears any drift.
        let dir = drift_test_dir("backend-only-plan");
        let mut cfg = AppConfig::default_config();
        for b in cfg.backends.iter_mut() {
            b.enabled = true;
        }
        // The file an earlier start wrote (coder on its old port).
        let view = manager_wizard::config_with_armed_gateway_mcp_servers(&cfg, |_| false);
        let yaml = manager_gateway::generate_config_yaml(&view).unwrap();
        std::fs::write(gateway_yaml_path(&dir), &yaml).unwrap();
        // Journal the backend-only change: coder's flags, with
        // coder running a process started before the save.
        let pre = cfg.clone();
        let mut post = cfg.clone();
        post.backends
            .iter_mut()
            .find(|b| b.id == "coder")
            .unwrap()
            .raw_flags = Some("-ot exps=CPU".to_string());
        let configured: std::collections::HashSet<String> =
            post.backends.iter().map(|b| b.id.clone()).collect();
        let running: std::collections::HashSet<String> =
            ["coder".to_string()].into_iter().collect();
        let started_at: std::collections::HashMap<String, chrono::DateTime<chrono::Utc>> =
            [("coder".to_string(), chrono::Utc::now() - chrono::Duration::seconds(60))]
                .into_iter()
                .collect();
        record_change(
            &dir,
            &pre,
            &post,
            "backend_flags",
            "Backend `coder` flags changed".to_string(),
            "backend",
            vec!["coder".to_string()],
            &started_at,
            &configured,
            &running,
        )
        .unwrap();
        // The on-disk file predates the current configuration (a
        // port move since the start that wrote it), so the raw
        // comparison is drift — the state Pete's plan read.
        cfg.backends
            .iter_mut()
            .find(|b| b.id == "coder")
            .unwrap()
            .port += 1;
        let raw = config_drift(&cfg, &dir, |_| false);
        assert!(raw, "precondition: the stale file is raw drift");
        // The plan's composition (op_gateway_activation_plan):
        // pending entries plus drift gated on the gateway probe.
        let journal = load_journal(&dir);
        let pending = pending_entries(&dir, &journal, &started_at, &configured, &running, false);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].scope, "backend");
        assert_eq!(pending[0].targets, vec!["coder".to_string()]);
        assert!(!pending[0].summary.to_lowercase().contains("gateway"));
        assert!(
            !drift_is_pending(raw, false),
            "a backend-only plan must not report gateway drift while the gateway is down"
        );
        // The same stale file IS plan drift once the gateway is up
        // serving it: then the plan owes a gateway restart, and
        // gateway_pending (hence the apply line) must say so.
        assert!(drift_is_pending(raw, true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_drift_detail_names_the_differing_element() {
        // Tracker #57: the plan's drift item must say WHICH
        // generated file differs, so the element-level comparison
        // reports the YAML and the routing callback separately.
        let dir = drift_test_dir("detail");
        let mut cfg = AppConfig::default_config();
        for b in cfg.backends.iter_mut() {
            b.enabled = true;
        }
        let view = manager_wizard::config_with_armed_gateway_mcp_servers(&cfg, |_| false);
        let yaml = manager_gateway::generate_config_yaml(&view).unwrap();
        std::fs::write(gateway_yaml_path(&dir), &yaml).unwrap();
        let detail = config_drift_detail(&cfg, &dir, |_| false);
        assert_eq!(
            detail,
            DriftDetail {
                yaml: false,
                routing: false
            }
        );
        assert!(!detail.any());
        // A callback file no enabled rule justifies: routing only.
        std::fs::write(gateway_routing_path(&dir), "# stale callback\n").unwrap();
        let detail = config_drift_detail(&cfg, &dir, |_| false);
        assert_eq!(
            detail,
            DriftDetail {
                yaml: false,
                routing: true
            }
        );
        // A saved change the YAML carries: YAML too.
        std::fs::remove_file(gateway_routing_path(&dir)).unwrap();
        cfg.backends[0].port += 1;
        let detail = config_drift_detail(&cfg, &dir, |_| false);
        assert_eq!(
            detail,
            DriftDetail {
                yaml: true,
                routing: false
            }
        );
        assert!(detail.any());
        assert_eq!(config_drift(&cfg, &dir, |_| false), detail.any());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drift_plan_change_item_shape_and_summary() {
        // The synthesized item carries the fields every plan
        // surface renders (ts/kind/summary/scope/targets/
        // activation), classifies as a gateway restart, and names
        // exactly the differing element(s) — never a cause.
        let at = chrono::DateTime::parse_from_rfc3339("2026-10-18T22:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let item = drift_plan_change(
            &DriftDetail {
                yaml: true,
                routing: false,
            },
            Some(at),
        );
        assert_eq!(item["kind"], "gateway_drift");
        assert_eq!(item["scope"], "gateway");
        assert_eq!(item["activation"], "gateway_restart");
        assert_eq!(item["targets"], serde_json::json!([]));
        assert_eq!(item["ts"], "2026-10-18T22:00:00+00:00");
        let summary = item["summary"].as_str().unwrap();
        assert!(summary.contains("config.yaml"), "{summary}");
        assert!(!summary.contains("llmdil_routing.py"), "{summary}");

        let item = drift_plan_change(
            &DriftDetail {
                yaml: false,
                routing: true,
            },
            None,
        );
        let summary = item["summary"].as_str().unwrap();
        assert!(summary.contains("llmdil_routing.py"), "{summary}");
        assert!(!summary.contains("config.yaml"), "{summary}");

        let item = drift_plan_change(
            &DriftDetail {
                yaml: true,
                routing: true,
            },
            None,
        );
        let summary = item["summary"].as_str().unwrap();
        assert!(summary.contains("config.yaml"), "{summary}");
        assert!(summary.contains("llmdil_routing.py"), "{summary}");
    }

    #[test]
    fn drift_only_state_composes_a_one_item_plan() {
        // Regression (tracker #57), Pete's production shape: the
        // gateway is UP serving files an earlier start wrote; the
        // saved state has since drifted with NO journal entry —
        // here the journal-free trigger itself: an MCP credential
        // stored after the start arms a Gateway server, so a start
        // now would write an mcp_servers block the on-disk file
        // lacks (the #42 fixture's armed side). The plan composed
        // the way op_gateway_activation_plan composes it must
        // contain exactly one change — the drift item — so no
        // surface can render "Pending changes: 0" beside an Apply.
        let dir = drift_test_dir("drift-only-plan");
        let mut cfg = AppConfig::default_config();
        for b in cfg.backends.iter_mut() {
            b.enabled = true;
        }
        // The file the running gateway was started with: written
        // while the seeded Gateway server was unarmed.
        let unarmed = manager_wizard::config_with_armed_gateway_mcp_servers(&cfg, |_| false);
        let yaml = manager_gateway::generate_config_yaml(&unarmed).unwrap();
        std::fs::write(gateway_yaml_path(&dir), &yaml).unwrap();
        // No journal file at all: nothing was ever recorded.
        let journal = load_journal(&dir);
        let started_at = std::collections::HashMap::new();
        let configured: std::collections::HashSet<String> =
            cfg.backends.iter().map(|b| b.id.clone()).collect();
        let running = std::collections::HashSet::new();
        let pending = pending_entries(&dir, &journal, &started_at, &configured, &running, true);
        assert!(pending.is_empty(), "precondition: no journal entries");
        // The credential now resolves: drift, gateway up.
        let detail = config_drift_detail(&cfg, &dir, |_| true);
        assert!(detail.any(), "precondition: the armed view drifted");
        assert!(detail.yaml);
        let drift = drift_is_pending(detail.any(), true);
        assert!(drift);
        // The op's composition: entry changes (none) plus the
        // synthesized drift item.
        let mut changes: Vec<serde_json::Value> = Vec::new();
        if drift {
            changes.push(drift_plan_change(
                &detail,
                gateway_config_written_at(&dir),
            ));
        }
        assert_eq!(changes.len(), 1, "a pending plan never has zero items");
        assert_eq!(changes[0]["kind"], "gateway_drift");
        assert_eq!(changes[0]["activation"], "gateway_restart");
        // The same state with the gateway DOWN composes no item:
        // the stale file is information, not pending work (#45).
        assert!(!drift_is_pending(detail.any(), false));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
