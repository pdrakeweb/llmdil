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
//! start, so a journal entry older than that file's mtime was
//! resolved by the restart that rewrote it. Pending state is the
//! union of unresolved journal entries and live config drift (the
//! generated YAML differing from the file), which also catches
//! changes made outside the journaled commands.
//!
//! Cancel semantics: the first unresolved entry snapshots the
//! pre-change configuration. Cancelling restores that snapshot —
//! refused when the current configuration no longer matches the
//! last entry's post-change state, because another path changed it
//! since and a blind restore would discard that change too.

use std::path::{Path, PathBuf};

use manager_config::AppConfig;
use serde::{Deserialize, Serialize};

/// One saved change waiting on gateway activation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivationEntry {
    /// RFC 3339 UTC timestamp of the save.
    pub ts: String,
    /// Machine kind of the mutation (`backend_add`, `backend_delete`,
    /// `backend_apply_config`, `gateway_config`, `gateway_bind`).
    pub kind: String,
    /// Operator-facing one-line summary of the change.
    pub summary: String,
    /// The full configuration as saved by this change. Cancel
    /// compares the live configuration against the newest entry's
    /// state before restoring the snapshot.
    pub config_after: serde_json::Value,
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

/// The entries no gateway start has resolved yet.
pub fn unresolved_entries(data_dir: &Path, journal: &ActivationJournal) -> Vec<ActivationEntry> {
    let Some(written_at) = gateway_config_written_at(data_dir) else {
        return journal.entries.clone();
    };
    journal
        .entries
        .iter()
        .filter(|e| entry_ts(e).map(|ts| ts > written_at).unwrap_or(true))
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
pub fn record_change(
    data_dir: &Path,
    pre: &AppConfig,
    post: &AppConfig,
    kind: &str,
    summary: String,
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
        config_after: serde_json::to_value(post)
            .unwrap_or(serde_json::Value::Null),
    });
    save_journal(data_dir, &journal);
}

/// Clear the journal (activation completed, or changes cancelled).
pub fn clear_journal(data_dir: &Path) {
    save_journal(data_dir, &ActivationJournal::default());
}

/// The cancel target and its guard: the snapshot to restore, when
/// the live configuration still matches the newest unresolved
/// entry's post-change state. `Err` explains why cancel is refused.
pub fn cancel_target(
    data_dir: &Path,
    current: &AppConfig,
) -> Result<Option<AppConfig>, String> {
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
}
