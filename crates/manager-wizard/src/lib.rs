//! Setup wizard step state machine (spec §4.1).
//!
//! The wizard is a path, not a place: ten ordered steps run automatically,
//! each a `queued → running → done | failed` state machine with `skipped`
//! and `manual` terminal states. [`RunMode::Audit`] re-runs check-type work
//! without reinstalling.
//!
//! Steps that touch the real system go through [`SystemOps`], a small
//! injectable trait. [`RealSystemOps`](ops::RealSystemOps) implements it for
//! production; [`FakeSystemOps`](fake::FakeSystemOps) is the hermetic test
//! harness. The ten concrete steps live in [`steps`]; build them with
//! [`steps::default_steps`].

use async_trait::async_trait;
use manager_config::{AppConfig, McpExposure, McpServerConfig, McpTransport};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub mod config_edit;
pub mod fake;
pub mod gateway_lock;
mod gateway_state;
mod mcp;
mod model;
pub mod ops;
pub mod steps;
mod system_ops;
mod wizard;

pub use self::gateway_state::*;
pub use self::mcp::*;
pub use self::model::*;
pub use self::system_ops::*;
pub use self::wizard::*;

pub use config_edit::{apply_config_edits, AppliedEdits, ConfigEdit, ConfigEditMode};

pub use ops::{
    gateway_pid_path, pick_gateway_pid, resolve_gateway_pid, stop_gateway, stop_gateway_and_wait,
    FoundBy, GatewayStopOutcome,
};

/// Number of log lines surfaced inline on a failed step (spec §4.1).
pub const LOG_TAIL_LINES: usize = 20;

/// Minimum NVIDIA driver major version accepted by the hardware step.
/// Spec §3.4: R550+ for the pinned cuda-12.4 llama.cpp build (NVIDIA documents
/// Windows driver >= 551.61 for CUDA 12.4 GA; R535 only satisfies CUDA 12.2).
/// R550+ branches also carry the P100.
pub const MIN_DRIVER_MAJOR: u32 = 550;

/// Stable entry point for manual NVIDIA driver downloads (driver search;
/// per-GPU prefilled deep links are not stable, so the wizard links the
/// search page and states the required branch in text).
pub const NVIDIA_DRIVER_DOWNLOAD_URL: &str = "https://www.nvidia.com/Download/index.aspx";

/// Permanent aka.ms link to the latest VC++ redistributable (x64).
/// Microsoft documents this as the stable "latest" URL.
pub const VC_REDIST_DOWNLOAD_URL: &str = "https://aka.ms/vs/17/release/vc_redist.x64.exe";

/// Pinned uv version bootstrapped by the gateway step when `uv` is not on
/// PATH. uv is a self-contained binary: no install, no admin needed.
pub const UV_PINNED_VERSION: &str = "0.13.0";

/// Download URL for the pinned uv standalone archive for the current
/// platform. Windows uses the MSVC zip (extracted with the `zip` crate);
/// other platforms are unsupported for auto-bootstrap.
pub fn uv_download_url() -> Option<String> {
    let asset = if cfg!(target_os = "windows") && cfg!(target_arch = "x86_64") {
        "uv-x86_64-pc-windows-msvc.zip"
    } else {
        return None;
    };
    Some(format!(
        "https://github.com/astral-sh/uv/releases/download/{UV_PINNED_VERSION}/{asset}"
    ))
}

/// Install docs for platforms where uv cannot be auto-bootstrapped.
pub const UV_INSTALL_DOCS_URL: &str = "https://docs.astral.sh/uv/getting-started/installation/";

/// CPython version the gateway venv is built with. Pinned because the
/// LiteLLM dependency tree lags new CPython releases: under 3.15,
/// `hiredis` (a `litellm[proxy]` dependency) ships no wheel and its
/// source build requires MSVC (verified on PyPI 2026-10-15: hiredis
/// 3.4.2 has a cp312 win_amd64 wheel and no cp315 files at all), so an
/// unpinned `uv venv` — which picks the newest Python — cannot install
/// the gateway on a stock Windows machine.
pub const GATEWAY_PYTHON_VERSION: &str = "3.12";

/// prisma-client-py version the gateway venv is pinned to, exactly.
///
/// LiteLLM 1.104/1.105's PrismaWrapper (`litellm/proxy/db/prisma_client.py`)
/// reads and writes the generated client's name-mangled `_Prisma__engine`
/// attribute. prisma-client-py PRs #873/#876 moved engine storage into a
/// new `BasePrisma` base class, so from 0.13.0 on the generated `Prisma`
/// class no longer defines that attribute and LiteLLM's connect raises
/// `AttributeError: 'Prisma' object has no attribute '_Prisma__engine'`
/// (field-reproduced on 0.15.0, 2026-10-16; 0.11.0 is clean). 0.11.0 is
/// also LiteLLM's own pyproject/lockfile pin for the proxy, which is why
/// the pin is exact rather than a range cap: 0.12/0.13 are untested
/// against LiteLLM. See docs/debugging/gateway-start-troubleshooting.md.
pub const PRISMA_PINNED_VERSION: &str = "0.11.0";

/// Minimum LiteLLM version for MCP stdio servers (MCP design §4.3):
/// the CVE-2026-30623 command allowlist and runtime re-validation
/// are present from LiteLLM v1.83.7-stable. This is a floor, not a
/// pin — the repo does not pin LiteLLM in code, and any installed
/// version at or above the floor is acceptable. Enforcement lives
/// in the start-gateway step (Phase 2): when an armed stdio Gateway
/// server exists and the probed version is below the floor — or
/// could not be determined at all — the step fails closed before
/// spawning.
pub const LITELLM_MCP_FLOOR_VERSION: &str = "1.83.7";

/// Parse a dotted version string tolerantly: a leading `v`/`V` is
/// stripped, a `-`/`+` suffix (e.g. `1.83.7-stable`) is dropped, and
/// each remaining component must be a plain number. Returns `None`
/// for anything unparseable — callers render that as "not measured",
/// never as a guess (design principle 12).
fn parse_version_components(raw: &str) -> Option<Vec<u64>> {
    let trimmed = raw.trim();
    let core = trimmed.strip_prefix(['v', 'V']).unwrap_or(trimmed);
    let core = core.split(['-', '+']).next().unwrap_or("");
    if core.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for component in core.split('.') {
        if component.is_empty() || !component.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        parts.push(component.parse::<u64>().ok()?);
    }
    Some(parts)
}

/// Whether `version` is at or above [`LITELLM_MCP_FLOOR_VERSION`].
/// Pure comparison helper for the MCP version floor: `Some(true)` /
/// `Some(false)` for a parseable version, `None` when the version
/// cannot be parsed (unknown — the caller decides what unknown
/// means; Phase 0 records it and does not enforce).
pub fn litellm_version_meets_mcp_floor(version: &str) -> Option<bool> {
    let installed = parse_version_components(version)?;
    let floor = parse_version_components(LITELLM_MCP_FLOOR_VERSION)?;
    let len = installed.len().max(floor.len());
    for i in 0..len {
        let a = installed.get(i).copied().unwrap_or(0);
        let b = floor.get(i).copied().unwrap_or(0);
        if a != b {
            return Some(a > b);
        }
    }
    Some(true)
}

/// Credential Manager target holding the LiteLLM gateway's master
/// key. Current LiteLLM refuses to boot without one. The key is
/// generated on first gateway start and stored here; the generated
/// config.yaml references it as `os.environ/LITELLM_MASTER_KEY` and
/// the wizard injects it into the gateway process's environment at
/// spawn. Like the OpenRouter key, it never appears in config files
/// or logs.
pub const LITELLM_MASTER_KEY_CRED_TARGET: &str = "local-llm-service-manager/litellm-master-key";

/// Generate a new gateway master key: `sk-` plus 64 lowercase hex
/// characters (256 bits) drawn from the OS CSPRNG (SEC-N1; the
/// format is unchanged from the earlier SipHash-derived keys).
/// The key is a localhost bearer secret; it is stored only in
/// Windows Credential Manager (and, by design, in client configs the
/// user applies, e.g. the editor config) and injected into the
/// gateway process's environment at spawn.
pub fn generate_master_key() -> String {
    let mut bytes = [0u8; 32];
    // The OS CSPRNG failing means the platform cannot produce a
    // secret at all; refusing loudly beats minting a guessable key.
    getrandom::fill(&mut bytes).expect("OS CSPRNG unavailable");
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("sk-{hex}")
}

/// The gateway master key from Credential Manager, generating and
/// storing one when absent. An existing key is always reused, so
/// clients already configured with it keep working across restarts.
/// Shared by the gateway spawn, the editor-config writers, and the
/// Tauri layer.
pub async fn ensure_gateway_master_key(ops: &dyn SystemOps) -> Result<String, WizardError> {
    if let Some(existing) = ops.credential_read(LITELLM_MASTER_KEY_CRED_TARGET).await {
        if !existing.is_empty() {
            return Ok(existing);
        }
    }
    let key = generate_master_key();
    ops.credential_write(LITELLM_MASTER_KEY_CRED_TARGET, &key)
        .await?;
    Ok(key)
}

/// Credential Manager target for the app-managed PostgreSQL
/// server's password.
pub const POSTGRES_PASSWORD_CRED_TARGET: &str = "local-llm-service-manager/postgres-password";

/// Generate a PostgreSQL password: the same CSPRNG construction as
/// the master key, without the `sk-` prefix. Hex output is URL-safe,
/// so the value can be embedded in `DATABASE_URL` verbatim (no
/// percent-encoding) and used raw for `initdb --pwfile` and
/// `PGPASSWORD`.
pub fn generate_postgres_password() -> String {
    generate_master_key()
        .strip_prefix("sk-")
        .unwrap_or_default()
        .to_string()
}

/// The app-managed PostgreSQL password from Credential Manager,
/// generating and storing one when absent. An existing password is
/// always reused: the database cluster was initialized with it, so
/// rotating it here would lock the gateway out of its own database.
pub async fn ensure_postgres_password(ops: &dyn SystemOps) -> Result<String, WizardError> {
    if let Some(existing) = ops.credential_read(POSTGRES_PASSWORD_CRED_TARGET).await {
        if !existing.is_empty() {
            return Ok(existing);
        }
    }
    let password = generate_postgres_password();
    ops.credential_write(POSTGRES_PASSWORD_CRED_TARGET, &password)
        .await?;
    Ok(password)
}

/// Wizard errors.
#[derive(Debug, thiserror::Error)]
pub enum WizardError {
    #[error("prerequisite not satisfied for step {step}: waiting on {waiting_on}")]
    PrerequisiteFailed { step: String, waiting_on: String },
    #[error("step {0} failed: {1}")]
    StepFailed(String, String),
    #[error("unknown step: {0}")]
    UnknownStep(String),
    #[error("duplicate wizard step id: {0}")]
    DuplicateStepId(String),
    #[error("wizard cancelled")]
    Cancelled,
    #[error("command failed: {argv:?} exited with code {code}: {stderr}")]
    CommandFailed {
        argv: Vec<String>,
        code: i32,
        stderr: String,
    },
    #[error("download failed for {url}: {reason}")]
    DownloadFailed { url: String, reason: String },
    #[error("checksum mismatch for {}: expected {expected}, got {actual}",
        .path.display())]
    ChecksumMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("HTTP error for {url}: {reason}")]
    HttpError { url: String, reason: String },
    #[error("I/O error for {}: {reason}", .path.display())]
    IoError { path: PathBuf, reason: String },
    #[error("GPU probe failed: {0}")]
    GpuProbeFailed(String),
    #[error("unsupported llama.cpp build {asset}: {reason}")]
    UnsupportedBuild { asset: String, reason: String },
    #[error("gateway error: {0}")]
    Gateway(String),
    #[error("MXC error: {0}")]
    Mxc(String),
    #[error("Windows ML error: {0}")]
    WinMl(String),
    #[error(transparent)]
    Config(#[from] manager_config::ConfigError),
}

/// Case-insensitive hex comparison for checksums.
pub fn checksums_match(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// True when `sha` is a real pinned SHA-256 (64 hex chars), as opposed
/// to an empty value or a placeholder. Only a pinned hash can verify a
/// file; unpinned models download and run unverified by design.
pub fn is_pinned_hash(sha: &str) -> bool {
    sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests;
