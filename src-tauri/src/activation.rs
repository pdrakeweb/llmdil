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
}

/// The persisted journal: unresolved-change snapshot plus entries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActivationJournal {
    /// Configuration before the first unresolved change; the cancel
    /// target. `None` when no change is pending.
    pub snapshot: Option<serde_json::Value>,
    pub entries: Vec<ActivationEntry>,
}

fn journal_path(data_dir: &Path) -> PathBuf {
    data_dir.join("activation-journal.json")
}

fn gateway_yaml_path(data_dir: &Path) -> PathBuf {
    data_dir.join("gateway").join("config.yaml")
}

pub fn load_journal(data_dir: &Path) -> ActivationJournal {
    match std::fs::read_to_string(journal_path(data_dir)) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => ActivationJournal::default(),
    }
}

fn save_journal(data_dir: &Path, journal: &ActivationJournal) {
    let Ok(text) = serde_json::to_string_pretty(journal) else {
        return;
    };
    let path = journal_path(data_dir);
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// When the running gateway's configuration was generated: the
/// mtime of the YAML the start-gateway step last wrote. Entries at
/// or before this instant were resolved by that start.
fn gateway_config_written_at(data_dir: &Path) -> Option<chrono::DateTime<chrono::Utc>> {
    let meta = std::fs::metadata(gateway_yaml_path(data_dir)).ok()?;
    let modified = meta.modified().ok()?;
    Some(chrono::DateTime::<chrono::Utc>::from(modified))
}

fn entry_ts(entry: &ActivationEntry) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(&entry.ts)
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc))
}

/// The entries nothing has resolved yet, under gateway-only
/// resolution (the YAML mtime rule). Backend-scope entries count
/// as unresolved here: callers that know backend start times use
/// [`unresolved_entries_scoped`] for the full rule.
pub fn unresolved_entries(data_dir: &Path, journal: &ActivationJournal) -> Vec<ActivationEntry> {
    let all_targets: std::collections::HashSet<String> = journal
        .entries
        .iter()
        .flat_map(|e| e.targets.iter().cloned())
        .collect();
    unresolved_entries_scoped(
        data_dir,
        journal,
        &std::collections::HashMap::new(),
        &all_targets,
    )
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

/// Whether the live configuration's generated gateway YAML differs
/// from the file the running gateway was started with. A missing
/// file (gateway never started) is not drift — nothing is running
/// on a stale config.
pub fn config_drift(cfg: &AppConfig, data_dir: &Path) -> bool {
    let Ok(generated) = manager_gateway::generate_config_yaml(cfg) else {
        return false;
    };
    let Ok(on_disk) = std::fs::read_to_string(gateway_yaml_path(data_dir)) else {
        return false;
    };
    normalize_yaml(&generated) != normalize_yaml(&on_disk)
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
pub fn record_change(
    data_dir: &Path,
    pre: &AppConfig,
    post: &AppConfig,
    kind: &str,
    summary: String,
    scope: &str,
    targets: Vec<String>,
) {
    let mut journal = load_journal(data_dir);
    if unresolved_entries(data_dir, &journal).len() != journal.entries.len()
        || journal.entries.is_empty()
    {
        journal.entries = unresolved_entries(data_dir, &journal);
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
    });
    save_journal(data_dir, &journal);
}

/// Clear the journal (activation completed, or changes cancelled).
pub fn clear_journal(data_dir: &Path) {
    save_journal(data_dir, &ActivationJournal::default());
}

/// Clear only the gateway-scope entries (a gateway (re)start made
/// them live). Backend-scope entries stay: they resolve through
/// their backends' (re)starts, which the scoped rule observes. When
/// no entries remain the snapshot goes too — it is the cancel
/// target for pending entries only.
pub fn clear_gateway_scope(data_dir: &Path) {
    let mut journal = load_journal(data_dir);
    journal.entries.retain(|e| e.scope != "gateway");
    if journal.entries.is_empty() {
        journal.snapshot = None;
    }
    save_journal(data_dir, &journal);
}

/// The cancel target and its guard: the snapshot to restore, when
/// the live configuration still matches the newest unresolved
/// entry's post-change state. `Err` explains why cancel is refused.
pub fn cancel_target(data_dir: &Path, current: &AppConfig) -> Result<Option<AppConfig>, String> {
    let journal = load_journal(data_dir);
    let unresolved = unresolved_entries(data_dir, &journal);
    let Some(last) = unresolved.last() else {
        return Ok(None);
    };
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
    fn legacy_entry_without_scope_defaults_to_gateway() {
        let text = r#"{"ts":"2026-10-16T12:00:00+00:00","kind":"backend_add","summary":"x","config_after":null}"#;
        let e: ActivationEntry = serde_json::from_str(text).unwrap();
        assert_eq!(e.scope, "gateway");
        assert!(e.targets.is_empty());
    }
}
