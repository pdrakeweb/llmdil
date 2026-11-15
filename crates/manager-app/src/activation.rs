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
//! since and a blind restore would discard that change too. The
//! comparison ignores background bookkeeping stamps, the restore
//! keeps the current stamps, and a restore that would roll back a
//! security-relevant section is refused — see [`cancel_plan`] and
//! [`crate::activation_cancel`] (PS-10).

use std::path::{Path, PathBuf};

use manager_config::AppConfig;

use serde::{Deserialize, Serialize};

fn default_scope() -> String {
    "gateway".to_string()
}

/// One saved change waiting on activation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivationEntry {
    /// Unique id of this entry (PS-6): what a command rolls back by
    /// when its configuration save fails after the entry was
    /// recorded. `ts` is not unique — two records in the same
    /// instant share it — and removing by position (pop-last) would
    /// drop a concurrent writer's entry instead. Serde-defaulted:
    /// journals written before ids existed load with an empty id,
    /// which no rollback ever targets (rollback only removes entries
    /// the same command just recorded, and those always carry one),
    /// so legacy entries need no backfill.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
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
    let path = journal_path(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(journal) => journal,
            Err(e) => {
                // PS-9: an unparseable journal used to load as Default
                // and the next write silently erased it — standing
                // offload overrides included. Move it aside first
                // (same `.corrupt-<unix-ms>` naming as the config
                // file's F2 preservation) so the evidence survives and
                // a repair is possible, and say so in the log.
                let preserved = preserve_corrupt_journal(&path);
                match preserved {
                    Ok(dest) => tracing::warn!(
                        "activation journal: {} is unreadable ({e}); preserved as {} and starting empty",
                        path.display(),
                        dest.display()
                    ),
                    Err(err) => tracing::warn!(
                        "activation journal: {} is unreadable ({e}) and could not be preserved: {err}",
                        path.display()
                    ),
                }
                ActivationJournal::default()
            }
        },
        Err(_) => ActivationJournal::default(),
    }
}

/// Move an unparseable journal to `<file>.corrupt-<unix-ms>` beside it
/// (PS-9). A move, not a copy: the file is reloaded on every journal
/// access, and a copy would mint a fresh `.corrupt-*` file each time
/// until the next save replaced the original.
fn preserve_corrupt_journal(path: &Path) -> Result<PathBuf, String> {
    let unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut dest = path.as_os_str().to_owned();
    dest.push(format!(".corrupt-{unix_ms}"));
    let dest = PathBuf::from(dest);
    std::fs::rename(path, &dest).map_err(|e| format!("{e}"))?;
    Ok(dest)
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

/// When the configuration the running gateway serves was generated:
/// the `at` of the applied stamp the start-gateway step writes only
/// after a fully successful start (`manager_wizard::gateway_applied_at`).
/// Entries at or before this instant were resolved by that start. The
/// step writes the YAML early — before the stop, the PostgreSQL start,
/// the spawn and the health waits — so a failed restart leaves a newer
/// YAML behind; its mtime is used only when no stamp exists at all
/// (installs from before the stamp), never to override one (CR-1).
pub(crate) fn gateway_config_written_at(data_dir: &Path) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Some(at) = manager_wizard::gateway_applied_at(data_dir) {
        return Some(at);
    }
    // No stamp (or an unparseable one): the pre-stamp rule.
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
///
/// Returns the new entry's id. When the configuration save that
/// follows fails, the caller removes exactly that entry with
/// [`remove_entries`] (via [`JournalRecords::roll_back`]) — see
/// there for why that is safe against this function's prune and
/// snapshot reset (PS-6).
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
) -> Result<String, String> {
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
    let id = new_entry_id();
    journal.entries.push(ActivationEntry {
        id: id.clone(),
        ts: chrono::Utc::now().to_rfc3339(),
        kind: kind.to_string(),
        summary,
        config_after: serde_json::to_value(post).unwrap_or(serde_json::Value::Null),
        scope: scope.to_string(),
        targets,
        pre_change_yaml_hash: yaml_hash_before_change(data_dir, pre),
    });
    save_journal(data_dir, &journal)?;
    Ok(id)
}

/// A fresh journal-entry id: wall-clock nanoseconds, the process
/// id, and a process-wide counter. The counter makes ids unique
/// within a process even when the clock does not advance between
/// two records; the pid and clock separate processes (the app and
/// a CLI run against the same data dir). Opaque to every reader —
/// only equality matters.
fn new_entry_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{:x}-{n:x}", std::process::id())
}

/// Remove the entries with these ids — the rollback for a command
/// whose configuration save failed after it journaled (PS-6).
///
/// The record-before-save contract (code-review 4.3) stays: the
/// entry is still written first, so a crash between the journal
/// write and the config save leaves a phantom entry (the safe
/// direction — a plan that asks for a restart nobody needed) and
/// never a saved change no plan shows. This only covers the case
/// the command itself observes: the save returned an error, the
/// configuration on disk and in memory is unchanged, and the entry
/// describes a change that never happened.
///
/// Removal is by id, under the journal lock, on a freshly loaded
/// journal — never pop-last and never a restore of the whole
/// pre-record journal: either would clobber whatever another
/// writer journaled in between (a concurrent
/// [`record_offload_override`], another command's entry).
///
/// [`record_change`] may also have pruned resolved entries and reset
/// the snapshot to the command's `pre` configuration. Neither needs
/// undoing when the save fails:
///
/// - the pruned entries were already resolved or already effective
///   — no plan shows them, cancel ignores them, and the next record
///   prunes them anyway;
/// - the snapshot is reset only when no unresolved entry remained,
///   so after removing this entry the journal is empty again; the
///   snapshot is then dropped here (it is the cancel target for
///   pending entries only), and even if it were kept it equals the
///   unchanged on-disk configuration, which the next record would
///   overwrite from its own `pre` regardless.
///
/// Empty ids are ignored (legacy entries carry none). Entries that
/// are already gone (cleared by a concurrent activation) are not an
/// error.
pub fn remove_entries(data_dir: &Path, ids: &[String]) -> Result<(), String> {
    let ids: Vec<&String> = ids.iter().filter(|id| !id.is_empty()).collect();
    if ids.is_empty() {
        return Ok(());
    }
    let _guard = journal_lock();
    let mut journal = load_journal(data_dir);
    let before = journal.entries.len();
    journal.entries.retain(|e| !ids.contains(&&e.id));
    if journal.entries.len() == before {
        return Ok(());
    }
    if journal.entries.is_empty() {
        journal.snapshot = None;
    }
    save_journal(data_dir, &journal)
}

/// The journal entries one command recorded ahead of its
/// configuration save (PS-6): the ids its rollback removes when the
/// save — or a later record in the same command — fails.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct JournalRecords {
    ids: Vec<String>,
}

impl JournalRecords {
    /// No entries recorded (a change the gateway rule did not
    /// journal).
    pub fn none() -> Self {
        Self::default()
    }

    /// The records of one [`record_change`] call.
    pub fn one(id: String) -> Self {
        Self { ids: vec![id] }
    }

    /// Fold another record's ids into this command's set.
    pub fn extend(&mut self, other: JournalRecords) {
        self.ids.extend(other.ids);
    }

    pub fn ids(&self) -> &[String] {
        &self.ids
    }

    /// Remove every entry this command recorded ([`remove_entries`]).
    pub fn roll_back(&self, data_dir: &Path) -> Result<(), String> {
        remove_entries(data_dir, &self.ids)
    }
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
            && o.placed_ccs
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                == want
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
///
/// This is [`cancel_plan`] with the stale-journal outcome folded into
/// `None` (nothing to restore).
pub fn cancel_target(
    data_dir: &Path,
    current: &AppConfig,
    started_at: &std::collections::HashMap<String, chrono::DateTime<chrono::Utc>>,
    configured: &std::collections::HashSet<String>,
    running: &std::collections::HashSet<String>,
    gateway_up: bool,
) -> Result<Option<AppConfig>, String> {
    match cancel_plan(
        data_dir, current, started_at, configured, running, gateway_up,
    )? {
        CancelPlan::Restore(cfg) => Ok(Some(*cfg)),
        CancelPlan::Nothing | CancelPlan::StaleJournal => Ok(None),
    }
}

/// What a cancel request should do (see [`cancel_plan`]).
#[derive(Debug)]
pub enum CancelPlan {
    /// Nothing is pending: no restore.
    Nothing,
    /// The configuration already equals the snapshot (apart from
    /// bookkeeping) while the journal still carries entries: a
    /// previous cancel saved the restored configuration and stopped
    /// (crash) before clearing the journal. The caller clears the
    /// journal; there is nothing to restore.
    StaleJournal,
    /// Install this configuration: the snapshot, with bookkeeping
    /// carried over from the current configuration.
    Restore(Box<AppConfig>),
}

/// The cancel decision (PS-10). Field treatment follows
/// [`crate::activation_cancel`]:
///
/// - The guard compares the current configuration with the newest
///   pending entry's `config_after` with bookkeeping stripped from
///   both sides, so background stamps (`verified_at`,
///   `setup_completed`, …) no longer make cancel permanently
///   refused, while any other field — including one added to
///   `AppConfig` later — still refuses on a difference.
/// - Cancel is refused when a security section differs between the
///   snapshot and the current configuration: restoring would roll it
///   back to an older value.
/// - The restore is the snapshot with bookkeeping copied over from
///   the current configuration.
/// - Self-heal: when the journal still has entries but the current
///   configuration equals the snapshot (and differs from the newest
///   entry's post-change state), the journal is stale — see
///   [`CancelPlan::StaleJournal`].
///
/// Callers hold the configuration write lock across this call and
/// the restore's save, so the guard sees exactly the configuration
/// the restore replaces.
pub fn cancel_plan(
    data_dir: &Path,
    current: &AppConfig,
    started_at: &std::collections::HashMap<String, chrono::DateTime<chrono::Utc>>,
    configured: &std::collections::HashSet<String>,
    running: &std::collections::HashSet<String>,
    gateway_up: bool,
) -> Result<CancelPlan, String> {
    use crate::activation_cancel as fields;
    let _guard = journal_lock();
    let journal = load_journal(data_dir);
    let current_value = serde_json::to_value(current)
        .map_err(|e| format!("current configuration is not serializable: {e}"))?;
    if let (Some(snapshot), Some(newest)) = (&journal.snapshot, journal.entries.last()) {
        if fields::same_configuration(snapshot, &current_value)
            && !fields::same_configuration(&newest.config_after, &current_value)
        {
            return Ok(CancelPlan::StaleJournal);
        }
    }
    let unresolved = unresolved_entries_scoped(data_dir, &journal, started_at, configured);
    let pending = pending_entries(
        data_dir, &journal, started_at, configured, running, gateway_up,
    );
    let Some(last) = pending.last() else {
        return Ok(CancelPlan::Nothing);
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
    if !fields::same_configuration(&current_value, &last.config_after) {
        return Err(
            "the configuration changed through another path after these changes were saved; \
             cancelling would discard that change too, so the saved changes stand"
                .to_string(),
        );
    }
    let Some(snapshot) = journal.snapshot else {
        return Ok(CancelPlan::Nothing);
    };
    let security = fields::changed_security_sections(&snapshot, &current_value);
    if !security.is_empty() {
        return Err(format!(
            "security-relevant settings ({}) changed since these changes were first saved; \
             cancelling would roll them back to their earlier values, so the saved changes \
             stand — revert the pending changes individually instead",
            security.join(", ")
        ));
    }
    fields::restored_config(&snapshot, current).map(|cfg| CancelPlan::Restore(Box::new(cfg)))
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
mod tests;
