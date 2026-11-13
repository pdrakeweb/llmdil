//! Component updates (spec §12, phase P6): `updates_check_all` and
//! `updates_apply`.
//!
//! The components are the ones the setup wizard installs. Each has a
//! different trust model, and this module keeps every one of them:
//!
//! - **llama.cpp build** (`llamacpp`). The wizard installs exactly one
//!   build: the verified pin compiled into this app
//!   ([`LlamaCppPin::default_pin`], both archive SHA-256s computed from
//!   the official downloads). The check reports the newest upstream
//!   Windows CUDA 12.4 build (the Pascal constraint, spec §3.4) from
//!   GitHub Releases as *latest available*, but Apply only ever moves
//!   the install to the built-in verified pin — a newer upstream build
//!   needs a verified pin shipped in an app update. Apply = stop the
//!   running backends, set the old install aside, run the wizard's
//!   `install-llamacpp` step (download + SHA-256 verify + extract),
//!   confirm the installed tag, restart the backends and smoke each
//!   one with an inference request; any failure restores the old
//!   install and restarts the backends on it.
//! - **LiteLLM gateway** (`litellm`). Always pinned (finding F1,
//!   `docs/code-review-v0.1.60.md`): moving it forward means setting a
//!   newer explicit pin, so Apply requires the target `version` and
//!   never tracks "latest" on its own (that needs hash-locked
//!   requirements, out of scope). Apply = stop the gateway, set the
//!   pin, run the wizard's `start-gateway` step (it installs the pinned
//!   version into the venv and starts the gateway), confirm the
//!   measured version; on failure the previous pin is restored and the
//!   step re-run, which reinstalls the previous version. A version
//!   PyPI no longer serves (yanked or deleted) or that is on the
//!   known-bad list is refused. Because LiteLLM is always pinned,
//!   "apply all" never moves it.
//! - **PostgreSQL** (`postgres`) and **MXC runtime** (`mxc`) are
//!   report-only: installed vs built-in pin (and the npm latest for
//!   MXC). Moving PostgreSQL's binaries under an initialized cluster
//!   and re-running MXC's elevated host preparation are not automated
//!   here; Apply says so.
//!
//! Pins are respected: "apply all" skips every pinned component. A
//! per-component Apply installs only what the wizard itself would
//! install (the llama.cpp pin guard still fails closed in the step).
//!
//! Network failures are per component (`check_error`), never fatal to
//! the check. Applies run as a cancellable task (`task_abort` cancels
//! the task token; the running wizard step is stopped through
//! [`crate::run_wizard_step_linked`]); a cancel before the new version
//! is committed rolls back like a failure. Dependent services are
//! stopped and started only through the app's existing operations
//! (`stop_backend` / `start_backend` / `gateway_stop` / the
//! `start-gateway` step). The outcome of every apply is appended to
//! `<data_dir>/updates/history.json`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use manager_config::{AppConfig, LITELLM_COMPONENT};
use manager_wizard::steps::{LlamaCppPin, MxcSdkPin, PostgresPin};
use serde::Serialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::{events, notify, ApiError, ApiResult, AppState, EventSink, TaskStatus};

/// Component key of the llama.cpp build.
pub const LLAMACPP_COMPONENT: &str = "llamacpp";
/// Component key of the app-managed PostgreSQL.
pub const POSTGRES_COMPONENT: &str = "postgres";
/// Component key of the MXC runtime.
pub const MXC_COMPONENT: &str = "mxc";
/// `updates_apply` target meaning every applicable, unpinned component.
pub const ALL_COMPONENTS: &str = "all";

/// llama.cpp builds: the releases list (every `bNNNN` build is
/// published as a GitHub pre-release, so `/releases/latest` does not
/// name them).
pub const LLAMACPP_RELEASES_URL: &str =
    "https://api.github.com/repos/ggml-org/llama.cpp/releases?per_page=30";
/// LiteLLM releases on PyPI.
pub const LITELLM_PYPI_URL: &str = "https://pypi.org/pypi/litellm/json";
/// MXC SDK latest on the npm registry.
pub const MXC_NPM_LATEST_URL: &str = "https://registry.npmjs.org/@microsoft/mxc-sdk/latest";

/// The Windows CUDA asset family the app installs (cuda-12.4 keeps the
/// Pascal sm_60 targets; spec §3.4).
const LLAMACPP_ASSET_MARKER: &str = "-bin-win-cuda-12.4-x64.zip";

/// LiteLLM releases that must never be pinned: 1.107.0 was published
/// malicious on Oct 21, 2026 (finding F1).
pub const LITELLM_KNOWN_BAD: &[&str] = &["1.107.0"];

/// How long a restarted backend may take to load and answer the smoke
/// request before the update is rolled back.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const SMOKE_POLL: Duration = Duration::from_secs(2);

/// History entries retained in `<data_dir>/updates/history.json`.
const HISTORY_CAP: usize = 50;

// ---------------------------------------------------------------------------
// Release feeds
// ---------------------------------------------------------------------------

/// Where release metadata comes from. The real feed is HTTPS; tests
/// substitute a map of canned bodies and errors.
#[async_trait]
pub trait ReleaseFeed: Send + Sync {
    /// GET `url` and return the body text; any transport failure or
    /// non-success status is an `Err` with a readable reason.
    async fn get_text(&self, url: &str) -> Result<String, String>;
}

/// HTTPS release feed (reqwest, same client shape as the app
/// self-update check).
pub struct HttpFeed;

#[async_trait]
impl ReleaseFeed for HttpFeed {
    async fn get_text(&self, url: &str) -> Result<String, String> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("LLMdil/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        let mut req = client.get(url);
        if url.starts_with("https://api.github.com/") {
            req = req.header("Accept", "application/vnd.github+json");
        } else {
            req = req.header("Accept", "application/json");
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("HTTP {} from {url}", status.as_u16()));
        }
        resp.text().await.map_err(|e| format!("read failed: {e}"))
    }
}

/// Numeric components of a version-ish string, in order: `b11658` →
/// `[11658]`, `1.105.1` → `[1, 105, 1]`, `18.6-1` → `[18, 6, 1]`.
fn numeric_parts(v: &str) -> Vec<u64> {
    v.split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.parse().ok())
        .collect()
}

/// `a` is strictly newer than `b` (component-wise numeric order).
pub fn version_newer(a: &str, b: &str) -> bool {
    numeric_parts(a) > numeric_parts(b)
}

/// A final LiteLLM release number: exactly `X.Y.Z`, digits only (no
/// rc/dev/post segments).
pub fn is_final_release(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// Newest llama.cpp build tag (`bNNNN`) in a GitHub releases-list body
/// that carries the Windows CUDA 12.4 x64 binaries asset. Drafts are
/// ignored; pre-releases are not (every build is one).
pub fn latest_llamacpp_build(body: &str) -> Result<Option<String>, String> {
    let releases: Vec<Value> =
        serde_json::from_str(body).map_err(|e| format!("unparseable releases list: {e}"))?;
    let mut best: Option<(u64, String)> = None;
    for r in &releases {
        if r.get("draft").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(tag) = r.get("tag_name").and_then(Value::as_str) else {
            continue;
        };
        let Some(num) = tag.strip_prefix('b').and_then(|n| n.parse::<u64>().ok()) else {
            continue;
        };
        let has_asset = r.get("assets").and_then(Value::as_array).is_some_and(|a| {
            a.iter().any(|x| {
                x.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| n.ends_with(LLAMACPP_ASSET_MARKER))
            })
        });
        if has_asset && best.as_ref().is_none_or(|(b, _)| num > *b) {
            best = Some((num, tag.to_string()));
        }
    }
    Ok(best.map(|(_, t)| t))
}

/// What PyPI says about one LiteLLM version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LitellmRelease {
    /// Served, with at least one non-yanked file.
    Available,
    /// Every file yanked.
    Yanked,
    /// Not on PyPI (never published, or deleted).
    Missing,
}

fn pypi_releases(body: &str) -> Result<serde_json::Map<String, Value>, String> {
    let doc: Value =
        serde_json::from_str(body).map_err(|e| format!("unparseable PyPI response: {e}"))?;
    doc.get("releases")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| "PyPI response has no releases map".to_string())
}

fn release_files_status(files: &Value) -> LitellmRelease {
    match files.as_array() {
        Some(f) if f.is_empty() => LitellmRelease::Missing,
        Some(f) => {
            if f.iter()
                .all(|x| x.get("yanked").and_then(Value::as_bool) == Some(true))
            {
                LitellmRelease::Yanked
            } else {
                LitellmRelease::Available
            }
        }
        None => LitellmRelease::Missing,
    }
}

/// Newest final LiteLLM release on PyPI that is served (not yanked)
/// and not on [`LITELLM_KNOWN_BAD`].
pub fn latest_litellm_release(body: &str) -> Result<Option<String>, String> {
    let releases = pypi_releases(body)?;
    Ok(releases
        .iter()
        .filter(|(v, files)| {
            is_final_release(v)
                && !LITELLM_KNOWN_BAD.contains(&v.as_str())
                && release_files_status(files) == LitellmRelease::Available
        })
        .map(|(v, _)| v.clone())
        .max_by_key(|v| numeric_parts(v)))
}

/// PyPI's status for one LiteLLM version.
pub fn litellm_release_status(body: &str, version: &str) -> Result<LitellmRelease, String> {
    let releases = pypi_releases(body)?;
    Ok(releases
        .get(version)
        .map(release_files_status)
        .unwrap_or(LitellmRelease::Missing))
}

/// `version` from an npm `/<pkg>/latest` document.
pub fn npm_latest_version(body: &str) -> Result<String, String> {
    let doc: Value =
        serde_json::from_str(body).map_err(|e| format!("unparseable npm response: {e}"))?;
    doc.get("version")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "npm response has no version".to_string())
}

// ---------------------------------------------------------------------------
// Inventory + check
// ---------------------------------------------------------------------------

fn marker_field(path: &Path, field: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
}

/// The llama.cpp install directory the wizard step writes.
pub fn llamacpp_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("llamacpp")
}

/// Where an apply sets the previous llama.cpp install aside.
pub fn llamacpp_backup_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("llamacpp.update-backup")
}

/// Installed version of a component, from the wizard's own records:
/// the install markers, and for LiteLLM the version the start-gateway
/// step measured at the last spawn. `None` = not installed / not
/// measured — never a guess.
pub fn installed_version(data_dir: &Path, component: &str) -> Option<String> {
    match component {
        LLAMACPP_COMPONENT => marker_field(&llamacpp_dir(data_dir).join(".installed.json"), "tag"),
        LITELLM_COMPONENT => manager_wizard::recorded_litellm_version(data_dir),
        POSTGRES_COMPONENT => marker_field(
            &manager_wizard::steps::postgres_root(data_dir).join(".installed.json"),
            "version",
        ),
        MXC_COMPONENT => marker_field(&data_dir.join("mxc").join(".installed.json"), "version"),
        _ => None,
    }
}

fn pinned_version(cfg: &AppConfig, component: &str) -> Option<String> {
    cfg.updates
        .components
        .get(component)
        .and_then(|c| c.pinned_version.clone())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The LiteLLM pin in effect (the operator pin, else the default the
/// wizard falls back to).
fn litellm_pin(cfg: &AppConfig) -> String {
    pinned_version(cfg, LITELLM_COMPONENT)
        .unwrap_or_else(|| manager_config::DEFAULT_LITELLM_PIN.to_string())
}

/// One component row of `updates_check_all`.
#[derive(Debug, Clone, Serialize)]
pub struct ComponentReport {
    pub id: String,
    pub name: String,
    /// What is installed (`None`: not installed / not measured).
    pub installed_version: Option<String>,
    /// Newest version upstream publishes (`None`: no feed, or the
    /// check failed — see `check_error`).
    pub latest_available: Option<String>,
    /// What Apply would install: the built-in verified pin for the
    /// binary components, the newest acceptable release for LiteLLM.
    pub target_version: Option<String>,
    pub pinned: bool,
    pub pinned_version: Option<String>,
    pub auto_update: bool,
    /// `target_version` is newer than what is installed (or pinned,
    /// for LiteLLM).
    pub update_available: bool,
    /// Apply can perform this update from the app.
    pub applyable: bool,
    /// Why not, or what else the operator should know.
    pub note: Option<String>,
    /// This component's feed failed (the check itself did not).
    pub check_error: Option<String>,
    /// The release source consulted (`None`: none exists).
    pub source: Option<String>,
}

/// Check every managed component against its release source.
pub async fn check_components(
    cfg: &AppConfig,
    data_dir: &Path,
    feed: &dyn ReleaseFeed,
) -> Vec<ComponentReport> {
    let (llama_body, litellm_body, mxc_body) = tokio::join!(
        feed.get_text(LLAMACPP_RELEASES_URL),
        feed.get_text(LITELLM_PYPI_URL),
        feed.get_text(MXC_NPM_LATEST_URL),
    );
    let auto = |c: &str| {
        cfg.updates
            .components
            .get(c)
            .map(|c| c.auto_update)
            .unwrap_or(false)
    };

    // llama.cpp
    let llama = {
        let pin = LlamaCppPin::default_pin();
        let installed = installed_version(data_dir, LLAMACPP_COMPONENT);
        let pinned = pinned_version(cfg, LLAMACPP_COMPONENT);
        let (latest, check_error) = match llama_body.and_then(|b| latest_llamacpp_build(&b)) {
            Ok(v) => (v, None),
            Err(e) => (None, Some(e)),
        };
        let update_available = installed
            .as_deref()
            .is_some_and(|i| i != pin.tag && version_newer(&pin.tag, i));
        let mut notes = Vec::new();
        if installed.is_none() {
            notes.push("not installed — run the install-llamacpp setup step".to_string());
        }
        if let Some(p) = &pinned {
            if *p != pin.tag {
                notes.push(format!(
                    "pinned to {p}, but this build carries a verified pin only for {}; the install step refuses other versions",
                    pin.tag
                ));
            }
        }
        if let Some(l) = &latest {
            if version_newer(l, &pin.tag) {
                notes.push(format!(
                    "upstream {l} is newer than this app's verified build {}; it becomes installable when an app update ships a verified pin for it",
                    pin.tag
                ));
            }
        }
        let applyable = update_available && pinned.as_deref().is_none_or(|p| p == pin.tag);
        ComponentReport {
            id: LLAMACPP_COMPONENT.to_string(),
            name: "llama.cpp build".to_string(),
            installed_version: installed,
            latest_available: latest,
            target_version: Some(pin.tag.clone()),
            pinned: pinned.is_some(),
            pinned_version: pinned,
            auto_update: auto(LLAMACPP_COMPONENT),
            update_available,
            applyable,
            note: (!notes.is_empty()).then(|| notes.join("; ")),
            check_error,
            source: Some(LLAMACPP_RELEASES_URL.to_string()),
        }
    };

    // LiteLLM
    let litellm = {
        let pin = litellm_pin(cfg);
        let installed = installed_version(data_dir, LITELLM_COMPONENT);
        let (latest, check_error) = match litellm_body.and_then(|b| latest_litellm_release(&b)) {
            Ok(v) => (v, None),
            Err(e) => (None, Some(e)),
        };
        let current = installed.clone().unwrap_or_else(|| pin.clone());
        let update_available = latest
            .as_deref()
            .is_some_and(|l| version_newer(l, &current) && version_newer(l, &pin));
        let mut notes = vec![
            "always pinned (finding F1): Apply moves the pin to the chosen version; nothing updates LiteLLM automatically"
                .to_string(),
        ];
        if installed.is_none() {
            notes.push(format!(
                "installed version not measured yet (no gateway start recorded); the pin is {pin}"
            ));
        } else if installed.as_deref() != Some(pin.as_str()) {
            notes.push(format!(
                "the gateway last ran {} but the pin is {pin}; the next gateway start installs the pin",
                installed.as_deref().unwrap_or_default()
            ));
        }
        ComponentReport {
            id: LITELLM_COMPONENT.to_string(),
            name: "LiteLLM gateway".to_string(),
            installed_version: installed,
            latest_available: latest.clone(),
            target_version: latest,
            pinned: true,
            pinned_version: Some(pin),
            auto_update: auto(LITELLM_COMPONENT),
            update_available,
            applyable: update_available,
            note: Some(notes.join("; ")),
            check_error,
            source: Some(LITELLM_PYPI_URL.to_string()),
        }
    };

    // PostgreSQL: no machine-readable feed for the EDB binaries zips.
    let postgres = {
        let pin = PostgresPin::default_pin();
        let installed = installed_version(data_dir, POSTGRES_COMPONENT);
        let pinned = pinned_version(cfg, POSTGRES_COMPONENT);
        let update_available = installed
            .as_deref()
            .is_some_and(|i| version_newer(&pin.version, i));
        ComponentReport {
            id: POSTGRES_COMPONENT.to_string(),
            name: "PostgreSQL".to_string(),
            installed_version: installed,
            latest_available: None,
            target_version: Some(pin.version.clone()),
            pinned: pinned.is_some(),
            pinned_version: pinned,
            auto_update: auto(POSTGRES_COMPONENT),
            update_available,
            applyable: false,
            note: Some(
                "report-only: EDB publishes no release feed for the binaries zip, and replacing the server binaries under an initialized cluster is not automated — re-run install-postgres from Setup"
                    .to_string(),
            ),
            check_error: None,
            source: None,
        }
    };

    // MXC runtime.
    let mxc = {
        let pin = MxcSdkPin::default_pin();
        let installed = installed_version(data_dir, MXC_COMPONENT);
        let pinned = pinned_version(cfg, MXC_COMPONENT);
        let (latest, check_error) = match mxc_body.and_then(|b| npm_latest_version(&b)) {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e)),
        };
        let update_available = installed
            .as_deref()
            .is_some_and(|i| version_newer(&pin.version, i));
        let mut notes = vec![
            "report-only: the install re-runs an elevated host preparation — re-run install-mxc from Setup"
                .to_string(),
        ];
        if let Some(l) = &latest {
            if version_newer(l, &pin.version) {
                notes.push(format!(
                    "upstream {l} is newer than this app's verified pin {}",
                    pin.version
                ));
            }
        }
        ComponentReport {
            id: MXC_COMPONENT.to_string(),
            name: "MXC runtime".to_string(),
            installed_version: installed,
            latest_available: latest,
            target_version: Some(pin.version.clone()),
            pinned: pinned.is_some(),
            pinned_version: pinned,
            auto_update: auto(MXC_COMPONENT),
            update_available,
            applyable: false,
            note: Some(notes.join("; ")),
            check_error,
            source: Some(MXC_NPM_LATEST_URL.to_string()),
        }
    };

    vec![llama, litellm, postgres, mxc]
}

fn updates_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("updates")
}

/// Best-effort JSON write (cache / history): never fails the caller.
fn write_json_best_effort(path: &Path, value: &Value) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(value).unwrap_or_default();
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// `updates_check_all` with an injectable feed.
pub(crate) async fn check_all_with(state: &Arc<AppState>, feed: &dyn ReleaseFeed) -> ApiResult {
    let cfg = state.config.read().await.clone();
    let components = check_components(&cfg, &state.data_dir, feed).await;
    let checked_at = Utc::now();
    let payload = json!({
        "checked_at": checked_at.to_rfc3339(),
        "components": components,
    });
    write_json_best_effort(
        &updates_dir(&state.data_dir).join("last-check.json"),
        &payload,
    );
    // Stamp last_checked on the components the config already tracks
    // (clone → save → install; a failed save leaves memory as it was
    // and does not fail the check). Entries are never created here.
    {
        let mut cfg_guard = state.config.write().await;
        let mut new_cfg = cfg_guard.clone();
        let checked_ok: HashSet<&str> = components
            .iter()
            .filter(|c| c.check_error.is_none())
            .map(|c| c.id.as_str())
            .collect();
        let mut touched = false;
        for (id, comp) in new_cfg.updates.components.iter_mut() {
            if checked_ok.contains(id.as_str()) {
                comp.last_checked = Some(checked_at);
                touched = true;
            }
        }
        if touched && manager_config::save(&new_cfg, &state.config_path).is_ok() {
            *cfg_guard = new_cfg;
        }
    }
    Ok(payload)
}

/// `updates_check_all`.
pub async fn op_check_all(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
    check_all_with(state, &HttpFeed).await
}

// ---------------------------------------------------------------------------
// Apply: planning
// ---------------------------------------------------------------------------

/// One update an apply will perform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedUpdate {
    pub component: String,
    pub from: Option<String>,
    pub to: String,
}

/// A component an "apply all" left alone, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedUpdate {
    pub component: String,
    pub reason: String,
}

/// The validated work of one `updates_apply` call.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ApplyPlan {
    pub updates: Vec<PlannedUpdate>,
    pub skipped: Vec<SkippedUpdate>,
}

fn validate_litellm_version(version: &str) -> Result<(), ApiError> {
    if !is_final_release(version) {
        return Err(ApiError::InvalidInput(format!(
            "LiteLLM version '{version}' is not a final release number (X.Y.Z)"
        )));
    }
    if LITELLM_KNOWN_BAD.contains(&version) {
        return Err(ApiError::InvalidInput(format!(
            "LiteLLM {version} is a known-bad release and cannot be pinned"
        )));
    }
    Ok(())
}

/// Plan an apply. Pure apart from the PyPI lookup that vets an
/// explicit LiteLLM version (an unreachable PyPI does not block: the
/// install itself then decides).
pub async fn plan_apply(
    cfg: &AppConfig,
    data_dir: &Path,
    component_id: &str,
    version: Option<&str>,
    feed: &dyn ReleaseFeed,
) -> Result<ApplyPlan, ApiError> {
    let version = version.map(str::trim).filter(|v| !v.is_empty());
    let mut plan = ApplyPlan::default();
    match component_id {
        LLAMACPP_COMPONENT => {
            let pin = LlamaCppPin::default_pin();
            if let Some(v) = version {
                if v != pin.tag {
                    return Err(ApiError::InvalidInput(format!(
                        "llama.cpp {v} has no verified pin in this build (only {}); it needs an app update",
                        pin.tag
                    )));
                }
            }
            if let Some(p) = pinned_version(cfg, LLAMACPP_COMPONENT) {
                if p != pin.tag {
                    return Err(ApiError::InvalidInput(format!(
                        "llama.cpp is pinned to {p}; this build can install only its verified {} — change or clear the pin first",
                        pin.tag
                    )));
                }
            }
            let Some(installed) = installed_version(data_dir, LLAMACPP_COMPONENT) else {
                return Err(ApiError::InvalidInput(
                    "llama.cpp is not installed; run the install-llamacpp setup step".to_string(),
                ));
            };
            if installed == pin.tag {
                plan.skipped.push(SkippedUpdate {
                    component: LLAMACPP_COMPONENT.to_string(),
                    reason: format!("already on {installed}"),
                });
            } else {
                plan.updates.push(PlannedUpdate {
                    component: LLAMACPP_COMPONENT.to_string(),
                    from: Some(installed),
                    to: pin.tag,
                });
            }
        }
        LITELLM_COMPONENT => {
            let Some(v) = version else {
                return Err(ApiError::InvalidInput(
                    "LiteLLM moves only by setting a newer explicit pin (finding F1): pass `version`"
                        .to_string(),
                ));
            };
            validate_litellm_version(v)?;
            if let Ok(body) = feed.get_text(LITELLM_PYPI_URL).await {
                match litellm_release_status(&body, v) {
                    Ok(LitellmRelease::Available) | Err(_) => {}
                    Ok(LitellmRelease::Yanked) => {
                        return Err(ApiError::InvalidInput(format!(
                            "LiteLLM {v} is yanked on PyPI"
                        )))
                    }
                    Ok(LitellmRelease::Missing) => {
                        return Err(ApiError::InvalidInput(format!(
                            "LiteLLM {v} is not on PyPI"
                        )))
                    }
                }
            }
            let installed = installed_version(data_dir, LITELLM_COMPONENT);
            if litellm_pin(cfg) == v && installed.as_deref() == Some(v) {
                plan.skipped.push(SkippedUpdate {
                    component: LITELLM_COMPONENT.to_string(),
                    reason: format!("already on {v}"),
                });
            } else {
                plan.updates.push(PlannedUpdate {
                    component: LITELLM_COMPONENT.to_string(),
                    // The previous PIN, not the measured version: a
                    // rollback restores the pin that was in force.
                    from: Some(litellm_pin(cfg)),
                    to: v.to_string(),
                });
            }
        }
        POSTGRES_COMPONENT => {
            return Err(ApiError::Unsupported(
                "PostgreSQL updates are report-only: re-run install-postgres from Setup"
                    .to_string(),
            ))
        }
        MXC_COMPONENT => {
            return Err(ApiError::Unsupported(
                "MXC runtime updates are report-only: re-run install-mxc from Setup (it needs an elevated host preparation)"
                    .to_string(),
            ))
        }
        ALL_COMPONENTS => {
            if version.is_some() {
                return Err(ApiError::InvalidInput(
                    "`version` applies to a single component, not to 'all'".to_string(),
                ));
            }
            let pin = LlamaCppPin::default_pin();
            match (
                pinned_version(cfg, LLAMACPP_COMPONENT),
                installed_version(data_dir, LLAMACPP_COMPONENT),
            ) {
                (Some(p), _) => plan.skipped.push(SkippedUpdate {
                    component: LLAMACPP_COMPONENT.to_string(),
                    reason: format!("pinned to {p}"),
                }),
                (None, None) => plan.skipped.push(SkippedUpdate {
                    component: LLAMACPP_COMPONENT.to_string(),
                    reason: "not installed".to_string(),
                }),
                (None, Some(i)) if i == pin.tag || !version_newer(&pin.tag, &i) => {
                    plan.skipped.push(SkippedUpdate {
                        component: LLAMACPP_COMPONENT.to_string(),
                        reason: format!("already on {i}"),
                    })
                }
                (None, Some(i)) => plan.updates.push(PlannedUpdate {
                    component: LLAMACPP_COMPONENT.to_string(),
                    from: Some(i),
                    to: pin.tag,
                }),
            }
            plan.skipped.push(SkippedUpdate {
                component: LITELLM_COMPONENT.to_string(),
                reason: format!(
                    "pinned to {} — LiteLLM moves only by an explicit pin (finding F1)",
                    litellm_pin(cfg)
                ),
            });
            for c in [POSTGRES_COMPONENT, MXC_COMPONENT] {
                plan.skipped.push(SkippedUpdate {
                    component: c.to_string(),
                    reason: "report-only; update from Setup".to_string(),
                });
            }
        }
        other => return Err(ApiError::NotFound(format!("component {other}"))),
    }
    Ok(plan)
}

// ---------------------------------------------------------------------------
// Apply: execution
// ---------------------------------------------------------------------------

/// The side effects an apply needs, all routed through existing app
/// operations in the real implementation ([`AppUpdateEnv`]); tests use
/// a recording fake.
#[async_trait]
pub trait UpdateEnv: Send + Sync {
    /// Ids of the backends currently running or starting.
    async fn running_backends(&self) -> Vec<String>;
    async fn stop_backend(&self, id: &str) -> Result<(), String>;
    async fn start_backend(&self, id: &str) -> Result<(), String>;
    /// Wait for a (re)started backend to load and answer one inference
    /// request.
    async fn smoke_backend(&self, id: &str, cancel: &CancellationToken) -> Result<(), String>;
    async fn gateway_running(&self) -> bool;
    async fn stop_gateway(&self) -> Result<(), String>;
    /// Run one wizard install step; `Err` carries the step's message.
    /// `cancel` (when given) stops the step.
    async fn run_step(
        &self,
        step_id: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), String>;
    async fn set_litellm_pin(&self, version: &str) -> Result<(), String>;
    async fn installed_version(&self, component: &str) -> Option<String>;
    /// Set the current install aside so a failed update can restore it.
    async fn backup_install(&self, component: &str) -> Result<(), String>;
    async fn restore_install(&self, component: &str) -> Result<(), String>;
    async fn discard_backup(&self, component: &str) -> Result<(), String>;
    /// Progress line (task events / log).
    fn progress(&self, component: &str, message: &str);
}

/// How one planned update ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateOutcome {
    Updated,
    Failed,
    Cancelled,
}

/// The record of one planned update.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateResult {
    pub component: String,
    pub from: Option<String>,
    pub to: String,
    pub outcome: UpdateOutcome,
    pub message: String,
    /// `Some(true)`: the previous version was restored; `Some(false)`:
    /// the rollback itself failed (see `log`); `None`: nothing to roll
    /// back (failed before anything changed, or updated).
    pub rolled_back: Option<bool>,
    pub log: Vec<String>,
}

struct Run<'a> {
    env: &'a dyn UpdateEnv,
    component: &'a str,
    log: Vec<String>,
}

impl Run<'_> {
    fn note(&mut self, line: impl Into<String>) {
        let line = line.into();
        self.env.progress(self.component, &line);
        self.log.push(line);
    }
}

fn cancelled_err() -> String {
    "cancelled".to_string()
}

/// Execute a plan's updates in order. A cancel stops the run: the
/// update in flight rolls back, later ones are recorded cancelled
/// without being started.
pub async fn execute_plan(
    env: &dyn UpdateEnv,
    plan: &ApplyPlan,
    cancel: &CancellationToken,
) -> Vec<UpdateResult> {
    let mut results = Vec::new();
    for u in &plan.updates {
        if cancel.is_cancelled() {
            results.push(UpdateResult {
                component: u.component.clone(),
                from: u.from.clone(),
                to: u.to.clone(),
                outcome: UpdateOutcome::Cancelled,
                message: "cancelled before it started; nothing changed".to_string(),
                rolled_back: None,
                log: Vec::new(),
            });
            continue;
        }
        let result = match u.component.as_str() {
            LLAMACPP_COMPONENT => apply_llamacpp(env, u, cancel).await,
            LITELLM_COMPONENT => apply_litellm(env, u, cancel).await,
            other => UpdateResult {
                component: other.to_string(),
                from: u.from.clone(),
                to: u.to.clone(),
                outcome: UpdateOutcome::Failed,
                message: format!("no apply path for component {other}"),
                rolled_back: None,
                log: Vec::new(),
            },
        };
        results.push(result);
    }
    results
}

fn finish(
    u: &PlannedUpdate,
    run: Run<'_>,
    outcome: UpdateOutcome,
    message: String,
    rolled_back: Option<bool>,
) -> UpdateResult {
    UpdateResult {
        component: u.component.clone(),
        from: u.from.clone(),
        to: u.to.clone(),
        outcome,
        message,
        rolled_back,
        log: run.log,
    }
}

fn failure_outcome(err: &str, cancel: &CancellationToken) -> UpdateOutcome {
    if cancel.is_cancelled() || err == "cancelled" {
        UpdateOutcome::Cancelled
    } else {
        UpdateOutcome::Failed
    }
}

/// llama.cpp: stop backends → back up → install step → verify tag →
/// restart + smoke → discard backup. Rollback restores the backup and
/// restarts the backends on it.
async fn apply_llamacpp(
    env: &dyn UpdateEnv,
    u: &PlannedUpdate,
    cancel: &CancellationToken,
) -> UpdateResult {
    let mut run = Run {
        env,
        component: LLAMACPP_COMPONENT,
        log: Vec::new(),
    };
    let running = env.running_backends().await;
    run.note(format!(
        "updating llama.cpp {} → {}; running backends: {}",
        u.from.as_deref().unwrap_or("?"),
        u.to,
        if running.is_empty() {
            "none".to_string()
        } else {
            running.join(", ")
        }
    ));
    // Stop dependents first: llama-server holds its own binaries open.
    let mut stopped = Vec::new();
    for id in &running {
        match env.stop_backend(id).await {
            Ok(()) => {
                run.note(format!("stopped backend {id}"));
                stopped.push(id.clone());
            }
            Err(e) => {
                run.note(format!("could not stop backend {id}: {e}"));
                restart_all(&mut run, &stopped).await;
                return finish(
                    u,
                    run,
                    UpdateOutcome::Failed,
                    format!("could not stop backend {id}: {e}; nothing was changed"),
                    None,
                );
            }
        }
    }
    if cancel.is_cancelled() {
        restart_all(&mut run, &stopped).await;
        return finish(
            u,
            run,
            UpdateOutcome::Cancelled,
            "cancelled before the install; nothing was changed".to_string(),
            None,
        );
    }
    if let Err(e) = env.backup_install(LLAMACPP_COMPONENT).await {
        run.note(format!("could not set the current install aside: {e}"));
        restart_all(&mut run, &stopped).await;
        return finish(
            u,
            run,
            UpdateOutcome::Failed,
            format!("could not back up the current install: {e}; nothing was changed"),
            None,
        );
    }
    run.note("previous install set aside");

    let attempt: Result<(), String> = async {
        env.run_step(manager_wizard::steps::STEP_INSTALL_LLAMACPP, Some(cancel))
            .await?;
        if cancel.is_cancelled() {
            return Err(cancelled_err());
        }
        let now = env.installed_version(LLAMACPP_COMPONENT).await;
        if now.as_deref() != Some(u.to.as_str()) {
            return Err(format!(
                "install step finished but the installed build is {}, expected {}",
                now.as_deref().unwrap_or("unknown"),
                u.to
            ));
        }
        Ok(())
    }
    .await;
    if let Err(e) = attempt {
        run.note(format!("install failed: {e}"));
        let restored = rollback_llamacpp(&mut run, &[], &stopped).await;
        let outcome = failure_outcome(&e, cancel);
        return finish(
            u,
            run,
            outcome,
            format!("install failed: {e}"),
            Some(restored),
        );
    }
    run.note(format!("installed and verified {}", u.to));

    // Restart what was running on the new build and smoke each one.
    let mut started = Vec::new();
    let mut smoke_err: Option<String> = None;
    for id in &stopped {
        if cancel.is_cancelled() {
            smoke_err = Some(cancelled_err());
            break;
        }
        if let Err(e) = env.start_backend(id).await {
            smoke_err = Some(format!(
                "backend {id} failed to start on the new build: {e}"
            ));
            break;
        }
        started.push(id.clone());
        run.note(format!("started backend {id} on {}", u.to));
        if let Err(e) = env.smoke_backend(id, cancel).await {
            smoke_err = Some(if e == "cancelled" {
                e
            } else {
                format!("backend {id} failed the smoke request on the new build: {e}")
            });
            break;
        }
        run.note(format!("backend {id} answered the smoke request"));
    }
    if stopped.is_empty() {
        run.note(
            "no backend was running: smoke inference skipped (the build was checksum-verified)",
        );
    }
    if let Some(e) = smoke_err {
        run.note(e.clone());
        let restored = rollback_llamacpp(&mut run, &started, &stopped).await;
        let outcome = failure_outcome(&e, cancel);
        return finish(u, run, outcome, e, Some(restored));
    }
    if let Err(e) = env.discard_backup(LLAMACPP_COMPONENT).await {
        run.note(format!(
            "could not remove the previous install's backup: {e}"
        ));
    }
    finish(
        u,
        run,
        UpdateOutcome::Updated,
        format!("llama.cpp updated to {}", u.to),
        None,
    )
}

async fn restart_all(run: &mut Run<'_>, ids: &[String]) -> bool {
    let mut ok = true;
    for id in ids {
        match run.env.start_backend(id).await {
            Ok(()) => run.note(format!("restarted backend {id}")),
            Err(e) => {
                ok = false;
                run.note(format!("could not restart backend {id}: {e}"));
            }
        }
    }
    ok
}

/// Restore the previous llama.cpp install: stop what the update
/// started, put the backup back, restart what was running before.
async fn rollback_llamacpp(run: &mut Run<'_>, started: &[String], previously: &[String]) -> bool {
    run.note("rolling back to the previous build");
    for id in started {
        if let Err(e) = run.env.stop_backend(id).await {
            run.note(format!("could not stop backend {id} for the rollback: {e}"));
        }
    }
    let restored = match run.env.restore_install(LLAMACPP_COMPONENT).await {
        Ok(()) => {
            run.note("previous install restored");
            true
        }
        Err(e) => {
            run.note(format!(
                "ROLLBACK FAILED: could not restore the previous install: {e}"
            ));
            false
        }
    };
    let restarted = restart_all(run, previously).await;
    restored && restarted
}

/// LiteLLM: stop gateway → set pin → start-gateway step (installs the
/// pin, starts) → verify measured version → restore stopped state.
/// Rollback re-pins the previous version and re-runs the step.
async fn apply_litellm(
    env: &dyn UpdateEnv,
    u: &PlannedUpdate,
    cancel: &CancellationToken,
) -> UpdateResult {
    let mut run = Run {
        env,
        component: LITELLM_COMPONENT,
        log: Vec::new(),
    };
    let previous_pin = u
        .from
        .clone()
        .unwrap_or_else(|| manager_config::DEFAULT_LITELLM_PIN.to_string());
    let was_running = env.gateway_running().await;
    run.note(format!(
        "updating LiteLLM {previous_pin} → {} (gateway {})",
        u.to,
        if was_running { "running" } else { "stopped" }
    ));
    // The venv's files must not be in use while pip replaces them.
    if let Err(e) = env.stop_gateway().await {
        run.note(format!("could not stop the gateway: {e}"));
        return finish(
            u,
            run,
            UpdateOutcome::Failed,
            format!("could not stop the gateway: {e}; nothing was changed"),
            None,
        );
    }
    run.note("gateway stopped");
    if cancel.is_cancelled() {
        if was_running {
            let _ = env
                .run_step(manager_wizard::steps::STEP_START_GATEWAY, None)
                .await;
        }
        return finish(
            u,
            run,
            UpdateOutcome::Cancelled,
            "cancelled before the pin changed; nothing was changed".to_string(),
            None,
        );
    }
    if let Err(e) = env.set_litellm_pin(&u.to).await {
        run.note(format!("could not set the pin: {e}"));
        if was_running {
            let _ = env
                .run_step(manager_wizard::steps::STEP_START_GATEWAY, None)
                .await;
        }
        return finish(
            u,
            run,
            UpdateOutcome::Failed,
            format!("could not set the LiteLLM pin: {e}; nothing was changed"),
            None,
        );
    }
    run.note(format!("pin set to {}", u.to));
    let attempt: Result<(), String> = async {
        env.run_step(manager_wizard::steps::STEP_START_GATEWAY, Some(cancel))
            .await?;
        if cancel.is_cancelled() {
            return Err(cancelled_err());
        }
        let now = env.installed_version(LITELLM_COMPONENT).await;
        if now.as_deref() != Some(u.to.as_str()) {
            return Err(format!(
                "the gateway started but reports LiteLLM {}, expected {}",
                now.as_deref().unwrap_or("unknown"),
                u.to
            ));
        }
        Ok(())
    }
    .await;
    if let Err(e) = attempt {
        run.note(format!("update failed: {e}"));
        run.note(format!("rolling back to LiteLLM {previous_pin}"));
        let _ = env.stop_gateway().await;
        let restored = match env.set_litellm_pin(&previous_pin).await {
            Err(pe) => {
                run.note(format!("ROLLBACK FAILED: could not restore the pin: {pe}"));
                false
            }
            Ok(()) => match env
                .run_step(manager_wizard::steps::STEP_START_GATEWAY, None)
                .await
            {
                Ok(()) => {
                    run.note(format!(
                        "LiteLLM {previous_pin} reinstalled and the gateway started"
                    ));
                    if !was_running {
                        let _ = env.stop_gateway().await;
                    }
                    true
                }
                Err(se) => {
                    run.note(format!(
                        "ROLLBACK FAILED: the pin is back on {previous_pin} but start-gateway failed: {se}"
                    ));
                    false
                }
            },
        };
        let outcome = failure_outcome(&e, cancel);
        return finish(
            u,
            run,
            outcome,
            format!("update failed: {e}"),
            Some(restored),
        );
    }
    run.note(format!("gateway running LiteLLM {}", u.to));
    if !was_running {
        match env.stop_gateway().await {
            Ok(()) => run.note("gateway stopped again (it was stopped before the update)"),
            Err(e) => run.note(format!("could not stop the gateway again: {e}")),
        }
    }
    finish(
        u,
        run,
        UpdateOutcome::Updated,
        format!("LiteLLM updated to {} (pin moved)", u.to),
        None,
    )
}

// ---------------------------------------------------------------------------
// Apply: the real environment and the operation
// ---------------------------------------------------------------------------

/// [`UpdateEnv`] over the app's existing operations.
pub(crate) struct AppUpdateEnv {
    pub state: Arc<AppState>,
    pub sink: Arc<dyn EventSink>,
    pub task_id: String,
}

fn api_err(e: ApiError) -> String {
    e.to_string()
}

#[async_trait]
impl UpdateEnv for AppUpdateEnv {
    async fn running_backends(&self) -> Vec<String> {
        let cfg = self.state.config.read().await.clone();
        let mut ids: Vec<String> = crate::backend_running_ids(&self.state, &cfg)
            .await
            .into_iter()
            .collect();
        ids.sort();
        ids
    }
    async fn stop_backend(&self, id: &str) -> Result<(), String> {
        crate::op_stop_backend(
            &self.state,
            &self.sink,
            id.to_string(),
            true,
            Some("wizard".to_string()),
        )
        .await
        .map(|_| ())
        .map_err(api_err)
    }
    async fn start_backend(&self, id: &str) -> Result<(), String> {
        crate::op_start_backend(
            &self.state,
            &self.sink,
            id.to_string(),
            None,
            Some("wizard".to_string()),
        )
        .await
        .map(|_| ())
        .map_err(api_err)
    }
    async fn smoke_backend(&self, id: &str, cancel: &CancellationToken) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + SMOKE_TIMEOUT;
        let mut last = String::from("no answer yet");
        loop {
            if cancel.is_cancelled() {
                return Err(cancelled_err());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "no successful inference within {} min ({last})",
                    SMOKE_TIMEOUT.as_secs() / 60
                ));
            }
            let list = crate::op_list_backends(&self.state, &self.sink)
                .await
                .map_err(api_err)?;
            let row = list
                .get("backends")
                .and_then(Value::as_array)
                .or_else(|| list.as_array())
                .and_then(|a| {
                    a.iter()
                        .find(|b| b.get("id").and_then(Value::as_str) == Some(id))
                        .cloned()
                });
            let st = row
                .as_ref()
                .and_then(|r| r.get("state"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            match st.as_str() {
                "failed" | "stopped" => {
                    let reason = row
                        .as_ref()
                        .and_then(|r| r.get("failure_reason"))
                        .and_then(Value::as_str)
                        .unwrap_or("no reason recorded");
                    return Err(format!("backend is {st}: {reason}"));
                }
                "running" => {
                    match crate::op_test_backend(
                        &self.state,
                        &self.sink,
                        id.to_string(),
                        "Reply with OK.".to_string(),
                    )
                    .await
                    {
                        Ok(_) => return Ok(()),
                        Err(e) => last = e.to_string(),
                    }
                }
                other => last = format!("state {other}"),
            }
            tokio::select! {
                _ = cancel.cancelled() => return Err(cancelled_err()),
                _ = tokio::time::sleep(SMOKE_POLL) => {}
            }
        }
    }
    async fn gateway_running(&self) -> bool {
        let cfg = self.state.config.read().await.clone();
        let host = manager_wizard::steps::gateway_probe_host(&cfg);
        let port = cfg.gateway.port;
        tokio::task::spawn_blocking(move || crate::probe_gateway(&host, port))
            .await
            .unwrap_or(false)
    }
    async fn stop_gateway(&self) -> Result<(), String> {
        crate::op_gateway_stop(&self.state, &self.sink, None, Some("wizard".to_string()))
            .await
            .map(|_| ())
            .map_err(api_err)
    }
    async fn run_step(
        &self,
        step_id: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), String> {
        let payload = crate::run_wizard_step_linked(
            &self.state,
            &self.sink,
            step_id.to_string(),
            cancel.cloned(),
        )
        .await
        .map_err(api_err)?;
        if payload.get("busy").and_then(Value::as_bool) == Some(true) {
            return Err(format!(
                "another setup step is running ({}); try again when it finishes",
                payload
                    .get("running_step")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown step")
            ));
        }
        let outcome = payload.get("outcome").cloned().unwrap_or(Value::Null);
        match outcome.get("state").and_then(Value::as_str) {
            Some("done") => Ok(()),
            other => {
                if cancel.is_some_and(CancellationToken::is_cancelled) {
                    return Err(cancelled_err());
                }
                Err(format!(
                    "step {step_id} ended {}: {}",
                    other.unwrap_or("without an outcome"),
                    outcome
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("no message")
                ))
            }
        }
    }
    async fn set_litellm_pin(&self, version: &str) -> Result<(), String> {
        crate::op_updates_pin(
            &self.state,
            &self.sink,
            LITELLM_COMPONENT.to_string(),
            Some(version.to_string()),
        )
        .await
        .map(|_| ())
        .map_err(api_err)
    }
    async fn installed_version(&self, component: &str) -> Option<String> {
        installed_version(&self.state.data_dir, component)
    }
    async fn backup_install(&self, component: &str) -> Result<(), String> {
        if component != LLAMACPP_COMPONENT {
            return Err(format!("no backup path for {component}"));
        }
        let data_dir = self.state.data_dir.clone();
        tokio::task::spawn_blocking(move || backup_dir(&data_dir))
            .await
            .map_err(|e| e.to_string())?
    }
    async fn restore_install(&self, component: &str) -> Result<(), String> {
        if component != LLAMACPP_COMPONENT {
            return Err(format!("no backup path for {component}"));
        }
        let data_dir = self.state.data_dir.clone();
        tokio::task::spawn_blocking(move || restore_dir(&data_dir))
            .await
            .map_err(|e| e.to_string())?
    }
    async fn discard_backup(&self, component: &str) -> Result<(), String> {
        if component != LLAMACPP_COMPONENT {
            return Ok(());
        }
        let backup = llamacpp_backup_dir(&self.state.data_dir);
        tokio::task::spawn_blocking(move || match std::fs::remove_dir_all(&backup) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("{}: {e}", backup.display())),
        })
        .await
        .map_err(|e| e.to_string())?
    }
    fn progress(&self, component: &str, message: &str) {
        tracing::info!(task_id = %self.task_id, component, "{message}");
        self.sink.emit(
            events::TASK_PROGRESS,
            json!({
                "task_id": self.task_id,
                "kind": "updates_apply",
                "component": component,
                "message": message,
            }),
        );
    }
}

/// Move `<data_dir>/llamacpp` aside to the backup dir (replacing a
/// stale backup from an interrupted earlier apply).
pub fn backup_dir(data_dir: &Path) -> Result<(), String> {
    let dir = llamacpp_dir(data_dir);
    let backup = llamacpp_backup_dir(data_dir);
    match std::fs::remove_dir_all(&backup) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", backup.display())),
    }
    std::fs::rename(&dir, &backup)
        .map_err(|e| format!("{} → {}: {e}", dir.display(), backup.display()))
}

/// Put the backup back in place of whatever the failed install left.
pub fn restore_dir(data_dir: &Path) -> Result<(), String> {
    let dir = llamacpp_dir(data_dir);
    let backup = llamacpp_backup_dir(data_dir);
    if !backup.exists() {
        return Err(format!("no backup at {}", backup.display()));
    }
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    }
    std::fs::rename(&backup, &dir)
        .map_err(|e| format!("{} → {}: {e}", backup.display(), dir.display()))
}

/// Append one apply's results to `<data_dir>/updates/history.json`
/// (capped, best effort).
fn record_history(data_dir: &Path, task_id: &str, results: &[UpdateResult]) {
    let path = updates_dir(data_dir).join("history.json");
    let mut entries: Vec<Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    entries.push(json!({
        "at": Utc::now().to_rfc3339(),
        "task_id": task_id,
        "results": results,
    }));
    if entries.len() > HISTORY_CAP {
        let drop = entries.len() - HISTORY_CAP;
        entries.drain(..drop);
    }
    write_json_best_effort(&path, &Value::Array(entries));
}

/// Apps (keyed by state) with an apply in flight: one at a time.
static APPLYING: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

struct ApplyGuard(usize);

impl ApplyGuard {
    fn acquire(state: &Arc<AppState>) -> Option<Self> {
        let key = Arc::as_ptr(state) as usize;
        let mut set = APPLYING.lock().unwrap_or_else(|e| e.into_inner());
        if set.contains(&key) {
            return None;
        }
        set.push(key);
        Some(Self(key))
    }
}

impl Drop for ApplyGuard {
    fn drop(&mut self) {
        let mut set = APPLYING.lock().unwrap_or_else(|e| e.into_inner());
        set.retain(|k| *k != self.0);
    }
}

/// `updates_apply` with an injectable feed (plan vetting) and env.
/// Returns once the work is planned; the updates run as the recorded
/// task in the background.
pub(crate) async fn apply_with(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    component_id: String,
    version: Option<String>,
    feed: &dyn ReleaseFeed,
    make_env: impl FnOnce(String) -> Arc<dyn UpdateEnv>,
) -> ApiResult {
    let cfg = state.config.read().await.clone();
    let plan = plan_apply(
        &cfg,
        &state.data_dir,
        &component_id,
        version.as_deref(),
        feed,
    )
    .await?;
    if plan.updates.is_empty() {
        return Ok(json!({
            "task_id": null,
            "plan": plan,
            "message": "nothing to update",
        }));
    }
    let Some(guard) = ApplyGuard::acquire(state) else {
        return Err(ApiError::InvalidInput(
            "a component update is already running".to_string(),
        ));
    };
    let task_id = state
        .record_task(
            "updates_apply",
            json!({ "component_id": component_id, "version": version, "plan": plan }),
        )
        .await;
    let token = state.tasks.controls.token(&task_id);
    let env = make_env(task_id.clone());
    let state2 = state.clone();
    let sink2 = sink.clone();
    let plan2 = plan.clone();
    let tid = task_id.clone();
    tokio::spawn(
        async move {
            let _guard = guard;
            let results = execute_plan(env.as_ref(), &plan2, &token).await;
            record_history(&state2.data_dir, &tid, &results);
            let any_cancelled = results
                .iter()
                .any(|r| r.outcome == UpdateOutcome::Cancelled);
            let any_failed = results.iter().any(|r| r.outcome == UpdateOutcome::Failed);
            let status = if any_cancelled {
                TaskStatus::Aborted
            } else if any_failed {
                TaskStatus::Failed
            } else {
                TaskStatus::Done
            };
            let summary = results
                .iter()
                .map(|r| format!("{} {}: {}", r.component, r.to, r.message))
                .collect::<Vec<_>>()
                .join("; ");
            state2
                .finish_task(&tid, status, json!({ "plan": plan2, "results": results }))
                .await;
            match status {
                TaskStatus::Done => {
                    notify::notify(
                        sink2.as_ref(),
                        &state2,
                        notify::Category::Operations,
                        "Component update finished",
                        &summary,
                    )
                    .await
                }
                TaskStatus::Failed => {
                    notify::notify(
                        sink2.as_ref(),
                        &state2,
                        notify::Category::Failures,
                        "Component update failed",
                        &summary,
                    )
                    .await
                }
                _ => {}
            }
        }
        .in_current_span(),
    );
    Ok(json!({ "task_id": task_id, "plan": plan }))
}

/// `updates_apply`.
pub async fn op_apply(
    state: &Arc<AppState>,
    sink: &Arc<dyn EventSink>,
    component_id: String,
    version: Option<String>,
) -> ApiResult {
    let st = state.clone();
    let sk = sink.clone();
    apply_with(
        state,
        sink,
        component_id,
        version,
        &HttpFeed,
        move |task_id| {
            Arc::new(AppUpdateEnv {
                state: st,
                sink: sk,
                task_id,
            }) as Arc<dyn UpdateEnv>
        },
    )
    .await
}

#[cfg(test)]
mod tests;
