//! App self-update: check GitHub Releases for a newer LLMdil,
//! download and stage the installer, and apply it on an
//! operator-initiated restart.
//!
//! This is the *app's own* update path. It is deliberately separate
//! from the per-component pins in `AppConfig.updates` (LiteLLM,
//! llama.cpp builds, …) that the wizard manages: different source
//! (GitHub Releases of pdrakeweb/llmdil vs component pins), different
//! artifact (the NSIS installer vs component files), different
//! applier (a detached helper process vs the wizard). The only
//! configuration here is the two operator switches in
//! `AppConfig.app_update`; everything else is runtime state in
//! `<data_dir>/app-update/state.json`.
//!
//! Semantics, end to end:
//!
//! - **Check.** GET the releases list, drop drafts and prereleases,
//!   take the highest semver, compare against the running version.
//!   A check runs on demand (`app_update_check`), once shortly after
//!   startup, and every [`CHECK_INTERVAL`] while the app runs — the
//!   latter two only when `check_enabled` is set. Failures are
//!   logged and recorded in state as an outcome, never surfaced as
//!   a UI error: "already latest" and "check failed" are distinct
//!   outcomes in the state file and the log.
//! - **Stage.** With auto-update on (or the Settings Download
//!   action), the installer asset is downloaded into
//!   `<data_dir>/app-update/` and its SHA-256 verified against the
//!   digest the GitHub API publishes for the asset. Only a verified
//!   file is renamed out of its `.part` name and recorded as
//!   staged; a partial or unverified download is deleted and never
//!   counts as staged. An asset with no published digest cannot be
//!   staged at all — there is nothing to verify against.
//! - **Banner.** State-derived, not an event: whenever a staged
//!   version newer than the running version exists, the shell shows
//!   "Restart LLMdil to update". A plain close/reopen changes
//!   nothing: the staged installer stays on disk, the banner
//!   returns, and no update is ever applied over a running
//!   instance or without the operator pressing Restart.
//! - **Apply.** Restart records a pending apply in the state file,
//!   spawns this same executable detached with
//!   `--llmdil-apply-update`, and exits the process. The helper
//!   waits for the app process to exit, runs the staged installer
//!   silently (`/S`), records the outcome in the state file, and
//!   relaunches the app from its (possibly replaced) exe path. On
//!   failure the previous version remains installed, the staged
//!   installer is kept for a retry, and the failure is in the state
//!   file for the next start to surface (Settings → Updates, and a
//!   note on the banner).
//!
//! The running version is this crate's `CARGO_PKG_VERSION`: the
//! release bump moves every crate, src-tauri, and the UI version in
//! one commit, so the crate version is the app version.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{ApiError, AppState, EventSink};

/// Releases list endpoint for the product repo.
pub const RELEASES_API_URL: &str =
    "https://api.github.com/repos/pdrakeweb/llmdil/releases?per_page=30";

/// How often the background checker re-checks while the app runs.
/// Release cadence is days; six hours keeps a long-running desktop
/// instance from going stale without spending the unauthenticated
/// GitHub API budget (60 requests/hour/IP, shared with nothing else
/// here — the checker is the only caller).
pub const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Delay before the startup check, so the check never competes
/// with the app's own startup work (wizard hydration, sweeps).
pub const STARTUP_CHECK_DELAY: Duration = Duration::from_secs(20);

/// First CLI argument marking a process as the apply helper.
pub const HELPER_ARG: &str = "--llmdil-apply-update";

/// The running app version (see the module docs for why the crate
/// version is the app version).
pub fn current_version_str() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// ---------------------------------------------------------------------------
// Semver
// ---------------------------------------------------------------------------

/// A parsed semantic version. Comparison follows semver §11:
/// prerelease < release of the same core; numeric identifiers
/// compare numerically and sort before alphanumeric ones; a shorter
/// prerelease list that is a prefix of a longer one sorts first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Vec<PreId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreId {
    Num(u64),
    Str(String),
}

impl Version {
    /// Parse `v0.1.42`, `0.1.42`, `1.2.3-beta.1`, … Build metadata
    /// (`+…`) is accepted and ignored, per semver. Missing minor /
    /// patch components read as 0. Anything else is `None`.
    pub fn parse(raw: &str) -> Option<Version> {
        let s = raw.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let s = s.split('+').next().unwrap_or("");
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let mut parts = core.split('.');
        let major: u64 = parts.next()?.parse().ok()?;
        let minor: u64 = parts.next().map_or(Some(0), |p| p.parse().ok())?;
        let patch: u64 = parts.next().map_or(Some(0), |p| p.parse().ok())?;
        if parts.next().is_some() {
            return None;
        }
        let mut pre_ids = Vec::new();
        if let Some(p) = pre {
            if p.is_empty() {
                return None;
            }
            for id in p.split('.') {
                if id.is_empty() {
                    return None;
                }
                if id.chars().all(|c| c.is_ascii_digit()) {
                    pre_ids.push(PreId::Num(id.parse().ok()?));
                } else {
                    pre_ids.push(PreId::Str(id.to_string()));
                }
            }
        }
        Some(Version {
            major,
            minor,
            patch,
            pre: pre_ids,
        })
    }

    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }

    /// True when `self` is strictly newer than `other`.
    pub fn is_newer_than(&self, other: &Version) -> bool {
        self.cmp_semver(other) == std::cmp::Ordering::Greater
    }

    pub fn cmp_semver(&self, other: &Version) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        let core = (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch));
        if core != Ordering::Equal {
            return core;
        }
        match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            // A release sorts after every prerelease of the same core.
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => {
                for (a, b) in self.pre.iter().zip(other.pre.iter()) {
                    let ord = match (a, b) {
                        (PreId::Num(x), PreId::Num(y)) => x.cmp(y),
                        (PreId::Num(_), PreId::Str(_)) => Ordering::Less,
                        (PreId::Str(_), PreId::Num(_)) => Ordering::Greater,
                        (PreId::Str(x), PreId::Str(y)) => x.cmp(y),
                    };
                    if ord != Ordering::Equal {
                        return ord;
                    }
                }
                self.pre.len().cmp(&other.pre.len())
            }
        }
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            let parts: Vec<String> = self
                .pre
                .iter()
                .map(|p| match p {
                    PreId::Num(n) => n.to_string(),
                    PreId::Str(s) => s.clone(),
                })
                .collect();
            write!(f, "-{}", parts.join("."))?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// GitHub Releases payload
// ---------------------------------------------------------------------------

/// One release as the GitHub API returns it (only the fields the
/// checker reads).
#[derive(Debug, Clone, Deserialize)]
pub struct GhRelease {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<GhAsset>,
}

/// One release asset. `digest` is the API's content digest
/// (`"sha256:<hex>"`) when GitHub publishes one — it does for this
/// repo's installers (verified against the v0.1.42 asset).
#[derive(Debug, Clone, Deserialize)]
pub struct GhAsset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub digest: Option<String>,
}

/// A release that is a valid update candidate for the running app.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AvailableUpdate {
    /// Normalized version, no leading `v` (e.g. `0.1.43`).
    pub version: String,
    /// The release tag as published (e.g. `v0.1.43`).
    pub tag: String,
    pub asset_name: String,
    pub download_url: String,
    pub size_bytes: u64,
    /// Expected SHA-256 (lowercase hex) from the API asset digest,
    /// when published. Staging requires it.
    pub sha256: Option<String>,
}

/// Extract the hex SHA-256 from an API digest string
/// (`"sha256:<hex>"`). Any other algorithm or shape is `None`.
pub fn parse_asset_digest(digest: Option<&str>) -> Option<String> {
    let hex = digest?.strip_prefix("sha256:")?;
    let hex = hex.trim().to_ascii_lowercase();
    if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(hex)
    } else {
        None
    }
}

/// The installer asset of a release: the NSIS setup exe the
/// release pipeline publishes (`LLMdil_<version>_x64-setup.exe`).
fn installer_asset(release: &GhRelease) -> Option<&GhAsset> {
    release
        .assets
        .iter()
        .find(|a| a.name.starts_with("LLMdil_") && a.name.ends_with("_x64-setup.exe"))
}

/// Pick the update for `current` out of a releases list: drafts and
/// prereleases are never candidates (the API's `prerelease` flag or
/// a prerelease version suffix both exclude), candidates must be
/// strictly newer (no downgrade, no same-version reinstall), and
/// the highest candidate wins regardless of list order.
pub fn pick_update(releases: &[GhRelease], current: &Version) -> Option<AvailableUpdate> {
    let mut best: Option<(Version, AvailableUpdate)> = None;
    for rel in releases {
        if rel.draft || rel.prerelease {
            continue;
        }
        let Some(v) = Version::parse(&rel.tag_name) else {
            continue;
        };
        if v.is_prerelease() || !v.is_newer_than(current) {
            continue;
        }
        let Some(asset) = installer_asset(rel) else {
            continue;
        };
        let candidate = AvailableUpdate {
            version: v.to_string(),
            tag: rel.tag_name.clone(),
            asset_name: asset.name.clone(),
            download_url: asset.browser_download_url.clone(),
            size_bytes: asset.size,
            sha256: parse_asset_digest(asset.digest.as_deref()),
        };
        match &best {
            Some((bv, _)) if !v.is_newer_than(bv) => {}
            _ => best = Some((v, candidate)),
        }
    }
    best.map(|(_, c)| c)
}

// ---------------------------------------------------------------------------
// Persistent state
// ---------------------------------------------------------------------------

/// A downloaded installer whose SHA-256 verified against the API
/// digest. Only this counts as staged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StagedUpdate {
    pub version: String,
    pub asset_name: String,
    pub sha256: String,
    pub staged_at_unix_ms: i64,
}

/// Set by the Restart action, consumed by the apply helper. While
/// this is present an apply is in flight; the helper clears it
/// before doing any work so a crash cannot loop the apply.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingApply {
    pub version: String,
    pub asset_name: String,
    /// The app exe the helper relaunches after the install (the
    /// installer replaces the payload at this same path).
    pub relaunch_exe: String,
    pub requested_at_unix_ms: i64,
}

/// The outcome of the most recent apply attempt, surfaced on the
/// next start (Settings row, and a banner note while a staged
/// update for the same version is still pending a retry).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApplyRecord {
    pub version: String,
    pub ok: bool,
    pub message: String,
    pub at_unix_ms: i64,
}

/// The app-update state file's contents. Every field defaults so a
/// state file written by an older build loads cleanly.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct AppUpdateState {
    #[serde(default)]
    pub last_checked_unix_ms: Option<i64>,
    /// `"up_to_date"` | `"update_available"` | `"failed"`.
    #[serde(default)]
    pub last_check_outcome: Option<String>,
    #[serde(default)]
    pub last_check_error: Option<String>,
    #[serde(default)]
    pub available: Option<AvailableUpdate>,
    #[serde(default)]
    pub staged: Option<StagedUpdate>,
    #[serde(default)]
    pub last_stage_error: Option<String>,
    #[serde(default)]
    pub pending_apply: Option<PendingApply>,
    #[serde(default)]
    pub last_apply: Option<ApplyRecord>,
}

impl AppUpdateState {
    pub fn record_check(
        &mut self,
        now_ms: i64,
        outcome: &str,
        error: Option<String>,
        available: Option<AvailableUpdate>,
    ) {
        self.last_checked_unix_ms = Some(now_ms);
        self.last_check_outcome = Some(outcome.to_string());
        self.last_check_error = error;
        self.available = available;
    }
}

/// The directory holding the state file and staged installers.
pub fn update_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("app-update")
}

pub fn state_path(data_dir: &Path) -> PathBuf {
    update_dir(data_dir).join("state.json")
}

/// Load the state file; a missing or unreadable file is the empty
/// state (self-update state is reconstructible — worst case the
/// next check re-derives it).
pub fn load_state(data_dir: &Path) -> AppUpdateState {
    let path = state_path(data_dir);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => AppUpdateState::default(),
    }
}

/// Persist the state file atomically (temp file + rename in the
/// same directory), like the config save.
pub fn save_state(data_dir: &Path, state: &AppUpdateState) -> std::io::Result<()> {
    let dir = update_dir(data_dir);
    std::fs::create_dir_all(&dir)?;
    let path = state_path(data_dir);
    let tmp = dir.join("state.json.tmp");
    let bytes = serde_json::to_vec_pretty(state).expect("state serializes");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// The banner derives from state alone: a staged update strictly
/// newer than the running version. Nothing else — no event, no
/// toast history — makes it appear, and only applying the update
/// (running version catches up), superseding it, or removing the
/// staged record makes it disappear.
pub fn derive_banner(state: &AppUpdateState, current: &Version) -> Option<Value> {
    let staged = state.staged.as_ref()?;
    let staged_v = Version::parse(&staged.version)?;
    if !staged_v.is_newer_than(current) {
        return None;
    }
    let mut banner = json!({
        "version": staged.version,
        "staged_at_unix_ms": staged.staged_at_unix_ms,
    });
    // A failed apply for exactly this staged version rides the
    // banner: it is why the banner is still here after a restart.
    if let Some(rec) = &state.last_apply {
        if !rec.ok && rec.version == staged.version {
            banner["last_apply_error"] = json!(rec.message);
        }
    }
    Some(banner)
}

/// Drop state that a newer running version has made moot: a staged
/// (or merely available) update at or below the running version
/// has been applied or superseded by a manual install. Returns the
/// staged asset name when a staged record was dropped, so the
/// caller can delete the now-useless installer file.
pub fn reconcile_with_running(state: &mut AppUpdateState, current: &Version) -> Option<String> {
    let mut dropped_asset = None;
    if let Some(staged) = &state.staged {
        let stale = Version::parse(&staged.version)
            .map(|v| !v.is_newer_than(current))
            .unwrap_or(true);
        if stale {
            dropped_asset = Some(staged.asset_name.clone());
            state.staged = None;
        }
    }
    if let Some(avail) = &state.available {
        let stale = Version::parse(&avail.version)
            .map(|v| !v.is_newer_than(current))
            .unwrap_or(true);
        if stale {
            state.available = None;
        }
    }
    dropped_asset
}

/// The status payload every surface renders from (Settings block,
/// shell banner, REST `GET /api/v1/app-update`).
pub fn status_payload(state: &AppUpdateState, check_enabled: bool, auto_update: bool) -> Value {
    let current = Version::parse(current_version_str()).expect("crate version parses");
    json!({
        "current_version": current_version_str(),
        "check_enabled": check_enabled,
        "auto_update": auto_update,
        "last_checked_unix_ms": state.last_checked_unix_ms,
        "last_check_outcome": state.last_check_outcome,
        "last_check_error": state.last_check_error,
        "available": state.available,
        "staged": state.staged,
        "last_stage_error": state.last_stage_error,
        "last_apply": state.last_apply,
        "banner": derive_banner(state, &current),
    })
}

/// Load + reconcile + (when anything changed) persist. Every op
/// goes through here so stale staged records are cleaned up no
/// matter which surface runs first after an install.
fn load_reconciled(state: &Arc<AppState>) -> AppUpdateState {
    let mut st = load_state(&state.data_dir);
    let current = Version::parse(current_version_str()).expect("crate version parses");
    let before = st.clone();
    if let Some(asset) = reconcile_with_running(&mut st, &current) {
        let _ = std::fs::remove_file(update_dir(&state.data_dir).join(asset));
    }
    if st != before {
        let _ = save_state(&state.data_dir, &st);
    }
    st
}

async fn current_status(state: &Arc<AppState>) -> Value {
    let cfg = state.config.read().await;
    let st = load_reconciled(state);
    status_payload(&st, cfg.app_update.check_enabled, cfg.app_update.auto_update)
}

fn emit_changed(sink: &Arc<dyn EventSink>, payload: &Value) {
    sink.emit("app_update_changed", payload.clone());
}

// ---------------------------------------------------------------------------
// Check + stage flows
// ---------------------------------------------------------------------------

fn http_client() -> Result<reqwest::Client, ApiError> {
    reqwest::Client::builder()
        .user_agent(concat!("LLMdil/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| ApiError::Io(format!("update check HTTP client: {e}")))
}

/// Run one check against GitHub Releases and record the outcome.
/// Never fails the caller for a check-level problem (network, HTTP
/// status, malformed payload): the outcome lands in the state file
/// and the returned payload, per the silent-failure rule. When
/// auto-update is on and an update was found, staging runs inline.
pub async fn perform_check(state: &Arc<AppState>, sink: &Arc<dyn EventSink>, trigger: &str) -> Value {
    let now_ms = Utc::now().timestamp_millis();
    let (check_enabled, auto_update) = {
        let cfg = state.config.read().await;
        (cfg.app_update.check_enabled, cfg.app_update.auto_update)
    };
    let current = Version::parse(current_version_str()).expect("crate version parses");

    let fetched: Result<Vec<GhRelease>, String> = async {
        let client = http_client().map_err(|e| e.to_string())?;
        let resp = client
            .get(RELEASES_API_URL)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| format!("releases request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("releases request returned HTTP {}", resp.status()));
        }
        // The workspace reqwest build has no `json` feature; parse
        // from text with serde_json directly.
        let body = resp
            .text()
            .await
            .map_err(|e| format!("releases response body failed: {e}"))?;
        serde_json::from_str::<Vec<GhRelease>>(&body)
            .map_err(|e| format!("releases response did not parse: {e}"))
    }
    .await;

    let mut st = load_reconciled(state);
    match fetched {
        Err(msg) => {
            tracing::warn!("app update check ({trigger}) failed: {msg}");
            st.record_check(now_ms, "failed", Some(msg), st.available.clone());
        }
        Ok(releases) => match pick_update(&releases, &current) {
            Some(avail) => {
                tracing::info!(
                    "app update check ({trigger}): {} available (running {})",
                    avail.version,
                    current_version_str()
                );
                st.record_check(now_ms, "update_available", None, Some(avail));
            }
            None => {
                tracing::info!(
                    "app update check ({trigger}): already latest ({})",
                    current_version_str()
                );
                st.record_check(now_ms, "up_to_date", None, None);
            }
        },
    }
    if let Err(e) = save_state(&state.data_dir, &st) {
        tracing::warn!("app update state save failed: {e}");
    }

    // Auto-update: a found update stages immediately. Manual mode
    // leaves the download to the Settings action.
    if auto_update && check_enabled && st.available.is_some() && st.staged.is_none() {
        if let Err(msg) = stage_available(state, &mut st).await {
            tracing::warn!("app update auto-stage failed: {msg}");
            st.last_stage_error = Some(msg);
            let _ = save_state(&state.data_dir, &st);
        }
    }

    let payload = status_payload(&st, check_enabled, auto_update);
    emit_changed(sink, &payload);
    payload
}

/// SHA-256 of a file's bytes, lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The staging state machine's verify step, over a downloaded
/// file: the file counts as staged only when its digest equals
/// the expected one. A mismatch deletes the file — unverified
/// bytes are never left where the helper could run them.
pub fn finalize_download(part_path: &Path, final_path: &Path, expected_sha256: &str) -> Result<(), String> {
    let bytes = std::fs::read(part_path).map_err(|e| format!("reading the download: {e}"))?;
    let actual = sha256_hex(&bytes);
    if actual != expected_sha256 {
        let _ = std::fs::remove_file(part_path);
        return Err(format!(
            "downloaded installer failed SHA-256 verification (expected {expected_sha256}, got {actual}); the unverified file was deleted"
        ));
    }
    std::fs::rename(part_path, final_path)
        .map_err(|e| format!("moving the verified installer into place: {e}"))?;
    Ok(())
}

/// Download the available update's installer into the staging
/// directory and verify it. On success `st.staged` is set and the
/// state saved; the running installation is never touched.
pub async fn stage_available(state: &Arc<AppState>, st: &mut AppUpdateState) -> Result<(), String> {
    let avail = st
        .available
        .clone()
        .ok_or_else(|| "no update is available to download".to_string())?;
    let expected = avail.sha256.clone().ok_or_else(|| {
        format!(
            "release {} publishes no SHA-256 digest for {}; refusing to stage an unverifiable installer",
            avail.tag, avail.asset_name
        )
    })?;

    // A newer available version supersedes an older staged one.
    if let Some(old) = &st.staged {
        if old.version != avail.version {
            let _ = std::fs::remove_file(update_dir(&state.data_dir).join(&old.asset_name));
            st.staged = None;
        }
    }
    if st.staged.is_some() {
        return Ok(()); // this version is already staged and verified
    }

    let dir = update_dir(&state.data_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating the staging directory: {e}"))?;
    let final_path = dir.join(&avail.asset_name);
    let part_path = dir.join(format!("{}.part", avail.asset_name));

    // Idempotent re-stage: a verified installer already on disk
    // (e.g. a retry after the state save failed) needs no download.
    if final_path.exists() {
        let bytes = std::fs::read(&final_path).map_err(|e| format!("reading the staged installer: {e}"))?;
        if sha256_hex(&bytes) == expected {
            st.staged = Some(StagedUpdate {
                version: avail.version.clone(),
                asset_name: avail.asset_name.clone(),
                sha256: expected,
                staged_at_unix_ms: Utc::now().timestamp_millis(),
            });
            st.last_stage_error = None;
            save_state(&state.data_dir, st).map_err(|e| format!("saving update state: {e}"))?;
            return Ok(());
        }
        let _ = std::fs::remove_file(&final_path);
    }

    let client = http_client().map_err(|e| e.to_string())?;
    let resp = client
        .get(&avail.download_url)
        .timeout(Duration::from_secs(600))
        .send()
        .await
        .map_err(|e| format!("installer download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("installer download returned HTTP {}", resp.status()));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("installer download failed mid-body: {e}"))?;
    std::fs::write(&part_path, &bytes).map_err(|e| format!("writing the download: {e}"))?;
    finalize_download(&part_path, &final_path, &expected)?;

    st.staged = Some(StagedUpdate {
        version: avail.version.clone(),
        asset_name: avail.asset_name.clone(),
        sha256: expected,
        staged_at_unix_ms: Utc::now().timestamp_millis(),
    });
    st.last_stage_error = None;
    save_state(&state.data_dir, st).map_err(|e| format!("saving update state: {e}"))?;
    tracing::info!("app update {} downloaded and staged", avail.version);
    Ok(())
}

// ---------------------------------------------------------------------------
// Apply: plan, spawn, helper
// ---------------------------------------------------------------------------

/// The exact helper invocation, as data, so the apply path is
/// testable without spawning anything: this executable, the helper
/// flag, the pid to wait for, and the state file to work from.
pub fn build_apply_plan(exe: &Path, wait_pid: u32, state_file: &Path) -> (PathBuf, Vec<String>) {
    (
        exe.to_path_buf(),
        vec![
            HELPER_ARG.to_string(),
            "--wait-pid".to_string(),
            wait_pid.to_string(),
            "--state".to_string(),
            state_file.to_string_lossy().into_owned(),
        ],
    )
}

/// The installer's silent-install invocation, as data (NSIS `/S`,
/// the same form the release pipeline's install test uses).
pub fn installer_command(installer: &Path) -> (PathBuf, Vec<String>) {
    (installer.to_path_buf(), vec!["/S".to_string()])
}

/// Whether `args` invoke this process as the apply helper.
pub fn is_helper_invocation(args: &[String]) -> bool {
    args.get(1).map(|a| a == HELPER_ARG).unwrap_or(false)
}

/// Record the pending apply, spawn the detached helper, and exit
/// this process. Called by the `app_update_restart` op; returns
/// the op payload for the brief window before the exit lands.
pub async fn begin_apply(state: &Arc<AppState>) -> Result<Value, ApiError> {
    let st = load_reconciled(state);
    let staged = st.staged.clone().ok_or_else(|| {
        ApiError::InvalidInput("no verified update is staged; nothing to apply".to_string())
    })?;
    let exe = std::env::current_exe()
        .map_err(|e| ApiError::Io(format!("locating the running executable: {e}")))?;
    let mut st = st;
    st.pending_apply = Some(PendingApply {
        version: staged.version.clone(),
        asset_name: staged.asset_name.clone(),
        relaunch_exe: exe.to_string_lossy().into_owned(),
        requested_at_unix_ms: Utc::now().timestamp_millis(),
    });
    save_state(&state.data_dir, &st).map_err(|e| ApiError::Io(format!("saving update state: {e}")))?;

    let (helper, args) = build_apply_plan(&exe, std::process::id(), &state_path(&state.data_dir));
    let mut cmd = std::process::Command::new(&helper);
    cmd.args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NO_WINDOW: the helper must
        // outlive this process and never flash a console.
        cmd.creation_flags(0x0000_0008 | 0x0800_0000);
    }
    cmd.spawn()
        .map_err(|e| ApiError::Io(format!("spawning the update helper: {e}")))?;

    // Exit promptly: the helper is waiting on this pid, and the
    // installer cannot replace a running executable. The short
    // delay lets the op response reach the caller first.
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(400)).await;
        std::process::exit(0);
    });
    Ok(json!({ "ok": true, "restarting": true, "version": staged.version }))
}

fn helper_arg(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}

/// The apply helper's whole job, run synchronously from `main`
/// before any UI starts when [`is_helper_invocation`] matches.
/// Returns the process exit code: 0 when the install ran (whatever
/// its outcome — the outcome is recorded in state; a nonzero code
/// here means the helper itself could not proceed).
pub fn run_apply_helper(args: &[String]) -> i32 {
    let Some(state_file) = helper_arg(args, "--state") else {
        return 2;
    };
    let state_file = PathBuf::from(state_file);
    let Some(data_dir) = state_file.parent().and_then(|d| d.parent()).map(Path::to_path_buf) else {
        return 2;
    };
    let wait_pid: u32 = helper_arg(args, "--wait-pid").and_then(|v| v.parse().ok()).unwrap_or(0);

    let mut st = load_state(&data_dir);
    let Some(pending) = st.pending_apply.take() else {
        return 2; // not an apply run after all; nothing recorded
    };
    // The pending record is consumed up front: if the helper dies
    // mid-apply, the staged record remains and the operator can
    // press Restart again — the apply never loops on its own.
    let _ = save_state(&data_dir, &st);

    // Wait for the app process to exit (the installer cannot
    // replace a running executable). Two minutes is generous: the
    // app exits ~400 ms after spawning us.
    if wait_pid != 0 {
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        while manager_wizard::ops::process_alive(wait_pid) {
            if std::time::Instant::now() >= deadline {
                st.last_apply = Some(ApplyRecord {
                    version: pending.version.clone(),
                    ok: false,
                    message: "the app did not exit within two minutes; the update was not applied".to_string(),
                    at_unix_ms: Utc::now().timestamp_millis(),
                });
                let _ = save_state(&data_dir, &st);
                return 0;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    let dir = update_dir(&data_dir);
    let installer = dir.join(&pending.asset_name);
    let (exe, iargs) = installer_command(&installer);
    let outcome: Result<i32, String> = std::process::Command::new(&exe)
        .args(&iargs)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.code().unwrap_or(-1))
        .map_err(|e| format!("could not run the staged installer: {e}"));

    let (ok, message) = match outcome {
        Ok(0) => (true, format!("LLMdil {} installed", pending.version)),
        Ok(code) => (false, format!("the installer exited with code {code}; the previous version is still installed")),
        Err(msg) => (false, msg),
    };
    st.last_apply = Some(ApplyRecord {
        version: pending.version.clone(),
        ok,
        message,
        at_unix_ms: Utc::now().timestamp_millis(),
    });
    if ok {
        st.staged = None;
        if st.available.as_ref().map(|a| a.version == pending.version).unwrap_or(false) {
            st.available = None;
        }
        let _ = std::fs::remove_file(&installer);
    }
    let _ = save_state(&data_dir, &st);

    // Relaunch either way: on success the new version, on failure
    // the untouched previous one — the operator is never left
    // without the app they asked to restart.
    let _ = std::process::Command::new(&pending.relaunch_exe).spawn();
    0
}

// ---------------------------------------------------------------------------
// Operations (registry handlers)
// ---------------------------------------------------------------------------

pub async fn op_app_update_status(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> crate::ApiResult {
    Ok(current_status(state).await)
}

pub async fn op_app_update_check(state: &Arc<AppState>, sink: &Arc<dyn EventSink>) -> crate::ApiResult {
    let _guard = state.app_update_guard.lock().await;
    Ok(perform_check(state, sink, "manual").await)
}

pub async fn op_app_update_set_checking(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    enabled: bool,
) -> crate::ApiResult {
    set_app_update_flag(state, |cfg| cfg.check_enabled = enabled).await?;
    Ok(current_status(state).await)
}

pub async fn op_app_update_set_auto(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    enabled: bool,
) -> crate::ApiResult {
    set_app_update_flag(state, |cfg| cfg.auto_update = enabled).await?;
    Ok(current_status(state).await)
}

/// The clone → save → install pattern the other config ops use:
/// the flag is set on a clone, the clone is saved, and only a
/// successful save installs it in memory.
async fn set_app_update_flag(
    state: &Arc<AppState>,
    set: impl FnOnce(&mut manager_config::AppUpdateConfig),
) -> Result<(), ApiError> {
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    set(&mut new_cfg.app_update);
    manager_config::save(&new_cfg, &state.config_path).map_err(ApiError::from)?;
    *cfg = new_cfg;
    Ok(())
}

pub async fn op_app_update_download(state: &Arc<AppState>, sink: &Arc<dyn EventSink>) -> crate::ApiResult {
    let _guard = state.app_update_guard.lock().await;
    let mut st = load_reconciled(state);
    if st.available.is_none() {
        // Nothing known yet: a manual Download implies a check.
        drop(st);
        let payload = perform_check(state, sink, "manual-download").await;
        st = load_reconciled(state);
        if st.available.is_none() {
            return Ok(payload);
        }
    }
    stage_available(state, &mut st)
        .await
        .map_err(ApiError::InvalidInput)?;
    let payload = current_status(state).await;
    emit_changed(sink, &payload);
    Ok(payload)
}

pub async fn op_app_update_restart(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> crate::ApiResult {
    let _guard = state.app_update_guard.lock().await;
    begin_apply(state).await
}

// ---------------------------------------------------------------------------
// Background checker (spawned by the shell)
// ---------------------------------------------------------------------------

/// The background check loop's body: one startup check shortly
/// after launch, then one per [`CHECK_INTERVAL`], each only when
/// checking is enabled at that moment. Spawned once by the shell
/// next to the other background loops; the loop itself lives here
/// so the cadence and gating are testable, headless logic.
pub async fn checker_loop(state: Arc<AppState>, sink: Arc<dyn EventSink>) {
    tokio::time::sleep(STARTUP_CHECK_DELAY).await;
    loop {
        let enabled = state.config.read().await.app_update.check_enabled;
        if enabled {
            let _guard = state.app_update_guard.lock().await;
            perform_check(&state, &sink, "scheduled").await;
        }
        tokio::time::sleep(CHECK_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|| panic!("{s} parses"))
    }

    #[test]
    fn version_parse_and_compare() {
        assert_eq!(v("v0.1.42").to_string(), "0.1.42");
        assert_eq!(v("0.1.42"), v("v0.1.42"));
        assert!(v("0.1.43").is_newer_than(&v("0.1.42")));
        assert!(v("0.2.0").is_newer_than(&v("0.1.99")));
        assert!(v("1.0.0").is_newer_than(&v("0.99.99")));
        assert!(!v("0.1.42").is_newer_than(&v("0.1.42")));
        // No downgrade: an older candidate is never newer.
        assert!(!v("0.1.41").is_newer_than(&v("0.1.42")));
        // Semver prerelease ordering.
        assert!(v("0.1.43").is_newer_than(&v("0.1.43-beta.1")));
        assert!(v("0.1.43-beta.2").is_newer_than(&v("0.1.43-beta.1")));
        assert!(v("0.1.43-beta").is_newer_than(&v("0.1.43-alpha.1")));
        assert!(v("0.1.43-alpha.1").is_newer_than(&v("0.1.43-alpha")));
        // Numeric prerelease identifiers sort before alphanumeric.
        assert!(v("0.1.43-alpha").is_newer_than(&v("0.1.43-1")));
        // Build metadata is ignored; junk does not parse.
        assert_eq!(v("0.1.42+build5"), v("0.1.42"));
        assert!(Version::parse("not-a-version").is_none());
        assert!(Version::parse("v1.2.3.4").is_none());
        assert!(Version::parse("").is_none());
    }

    /// A fixture in the real GitHub API shape (field names and the
    /// `digest` format verified against the published v0.1.42
    /// release), with the entries the picker must skip mixed in.
    fn releases_fixture() -> Vec<GhRelease> {
        let raw = r#"[
          {"tag_name": "v0.1.44", "draft": true, "prerelease": false, "assets": [
            {"name": "LLMdil_0.1.44_x64-setup.exe", "browser_download_url": "https://example.invalid/44.exe", "size": 10, "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]},
          {"tag_name": "v0.1.43", "draft": false, "prerelease": true, "assets": [
            {"name": "LLMdil_0.1.43_x64-setup.exe", "browser_download_url": "https://example.invalid/43.exe", "size": 10, "digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}]},
          {"tag_name": "v0.1.42", "draft": false, "prerelease": false, "assets": [
            {"name": "LLMdil_0.1.42_x64-setup.exe", "browser_download_url": "https://example.invalid/42.exe", "size": 7544388, "digest": "sha256:7d7450e6b4f3b399c736a1af01f2d2b66686160b852a98a3b18d39547d2645b2"}]},
          {"tag_name": "v0.1.45", "draft": false, "prerelease": false, "assets": [
            {"name": "LLMdil_0.1.45_x64-setup.exe", "browser_download_url": "https://example.invalid/45.exe", "size": 7600000, "digest": "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"}]},
          {"tag_name": "v0.1.40", "draft": false, "prerelease": false, "assets": []}
        ]"#;
        serde_json::from_str(raw).expect("fixture parses")
    }

    #[test]
    fn pick_update_skips_drafts_prereleases_and_downgrades() {
        let rels = releases_fixture();
        // From 0.1.42: the highest non-draft, non-prerelease is 0.1.45.
        let up = pick_update(&rels, &v("0.1.42")).expect("an update");
        assert_eq!(up.version, "0.1.45");
        assert_eq!(up.tag, "v0.1.45");
        assert_eq!(up.asset_name, "LLMdil_0.1.45_x64-setup.exe");
        assert_eq!(up.sha256.as_deref(), Some("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"));
        // From 0.1.45 itself: nothing newer that qualifies.
        assert!(pick_update(&rels, &v("0.1.45")).is_none());
        // From ahead of everything: no downgrade is ever offered.
        assert!(pick_update(&rels, &v("0.2.0")).is_none());
        // Digest parsing: only a well-formed sha256 digest counts.
        assert_eq!(parse_asset_digest(Some("sha256:AB12")), None);
        assert_eq!(
            parse_asset_digest(Some("sha256:7d7450e6b4f3b399c736a1af01f2d2b66686160b852a98a3b18d39547d2645b2")).as_deref(),
            Some("7d7450e6b4f3b399c736a1af01f2d2b66686160b852a98a3b18d39547d2645b2")
        );
        assert_eq!(parse_asset_digest(Some("md5:7d7450e6b4f3b399c736a1af01f2d2b66686160b852a98a3b18d39547d2645b2")), None);
        assert_eq!(parse_asset_digest(None), None);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "llm-app-update-test-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finalize_download_verifies_then_stages() {
        let dir = temp_dir("finalize");
        let part = dir.join("LLMdil_9.9.9_x64-setup.exe.part");
        let fin = dir.join("LLMdil_9.9.9_x64-setup.exe");
        std::fs::write(&part, b"installer-bytes").unwrap();
        let good = sha256_hex(b"installer-bytes");
        finalize_download(&part, &fin, &good).expect("verified download finalizes");
        assert!(fin.exists());
        assert!(!part.exists());

        // Tampered bytes: verification fails and the file is gone —
        // never left where the helper could run it.
        std::fs::write(&part, b"tampered-bytes").unwrap();
        let err = finalize_download(&part, &fin, &good).unwrap_err();
        assert!(err.contains("SHA-256"), "{err}");
        assert!(!part.exists());
    }

    #[test]
    fn banner_derives_from_staged_state() {
        let current = v("0.1.42");
        let st = AppUpdateState::default();
        assert!(derive_banner(&st, &current).is_none(), "nothing staged, no banner");
        let st = AppUpdateState {
            staged: Some(StagedUpdate {
                version: "0.1.43".to_string(),
                asset_name: "LLMdil_0.1.43_x64-setup.exe".to_string(),
                sha256: "ab".repeat(32),
                staged_at_unix_ms: 1,
            }),
            ..AppUpdateState::default()
        };
        let banner = derive_banner(&st, &current).expect("staged newer shows the banner");
        assert_eq!(banner["version"], "0.1.43");
        assert!(banner.get("last_apply_error").is_none());
        // A failed apply for this version rides the banner.
        let mut st = AppUpdateState {
            last_apply: Some(ApplyRecord {
                version: "0.1.43".to_string(),
                ok: false,
                message: "the installer exited with code 1".to_string(),
                at_unix_ms: 2,
            }),
            ..st
        };
        let banner = derive_banner(&st, &current).unwrap();
        assert_eq!(banner["last_apply_error"], "the installer exited with code 1");
        // Running version caught up: a staged record at the running
        // version is stale and reconciles away (with its file).
        let dropped = reconcile_with_running(&mut st, &v("0.1.43"));
        assert_eq!(dropped.as_deref(), Some("LLMdil_0.1.43_x64-setup.exe"));
        assert!(st.staged.is_none());
    }

    #[test]
    fn state_round_trips_and_defaults() {
        let dir = temp_dir("state");
        // Missing file -> empty state.
        assert_eq!(load_state(&dir), AppUpdateState::default());
        let st = AppUpdateState {
            last_checked_unix_ms: Some(42),
            available: Some(AvailableUpdate {
                version: "0.1.43".to_string(),
                tag: "v0.1.43".to_string(),
                asset_name: "LLMdil_0.1.43_x64-setup.exe".to_string(),
                download_url: "https://example.invalid/x".to_string(),
                size_bytes: 1,
                sha256: None,
            }),
            ..AppUpdateState::default()
        };
        save_state(&dir, &st).unwrap();
        assert_eq!(load_state(&dir), st);
        // An older/partial state file still loads (defaults fill in).
        std::fs::write(state_path(&dir), b"{\"last_checked_unix_ms\": 7}").unwrap();
        let loaded = load_state(&dir);
        assert_eq!(loaded.last_checked_unix_ms, Some(7));
        assert!(loaded.staged.is_none());
    }

    #[test]
    fn apply_plan_is_exact() {
        let (exe, args) = build_apply_plan(
            Path::new("C:/LLMdil/llm-service-manager.exe"),
            4242,
            Path::new("C:/data/app-update/state.json"),
        );
        assert_eq!(exe, PathBuf::from("C:/LLMdil/llm-service-manager.exe"));
        assert_eq!(
            args,
            vec![
                "--llmdil-apply-update",
                "--wait-pid",
                "4242",
                "--state",
                "C:/data/app-update/state.json",
            ]
        );
        assert!(is_helper_invocation(&{
            let mut a = vec!["exe".to_string()];
            a.extend(args.iter().cloned());
            a
        }));
        assert!(!is_helper_invocation(&["exe".to_string()]));
        let (iex, iargs) = installer_command(Path::new("C:/data/app-update/LLMdil_0.1.43_x64-setup.exe"));
        assert_eq!(iex, PathBuf::from("C:/data/app-update/LLMdil_0.1.43_x64-setup.exe"));
        assert_eq!(iargs, vec!["/S".to_string()]);
    }
}
