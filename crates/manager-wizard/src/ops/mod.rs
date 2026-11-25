//! Production [`SystemOps`](crate::SystemOps) implementation.
//!
//! Every method shells out to the real system: `nvidia-smi` for the GPU
//! inventory, `tokio::process` for commands, `reqwest` for downloads and
//! HTTP probes, `zip` for archive extraction. All failures are mapped into
//! [`WizardError`](crate::WizardError); nothing panics on missing tools.
//!
//! Network calls carry timeouts so a hung peer cannot hang the wizard:
//! health probes fail fast, downloads fail fast on connect (a stalled
//! mid-download is surfaced by the progress callback going quiet, which the
//! UI renders — reqwest has no stall timeout to set here).

use crate::{CommandOutput, GpuDescriptor, HttpResponse, PlannedCommand, SystemOps, WizardError};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Fail fast when the server cannot be reached at all.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum gap between body chunks on a download before the
/// connection is treated as stalled (see the download loop).
const CHUNK_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Health probes must answer quickly; the retry loop handles slowness.
const HTTP_GET_TIMEOUT: Duration = Duration::from_secs(15);
/// Default budget for one planned command. Package installs
/// (`uv pip install`) and Prisma's first-run toolchain download are
/// the slow legitimate cases; anything past this is a stall. Before
/// this bound existed, a single stalled child held the wizard lock
/// for the rest of the session and every later step run reported
/// "busy" (2026-10-16). Callers with a genuinely longer job use
/// [`SystemOps::run_command_with_timeout`].
const COMMAND_TIMEOUT: Duration = Duration::from_secs(600);
/// Budget for an elevated (UAC) command: the firewall script is a
/// few `netsh` calls. The process is not killed on expiry — the
/// caller treats this failure as warning-only and the script is
/// left to finish on its own.
#[cfg(windows)]
const ELEVATED_WAIT_TIMEOUT_MS: u32 = 120_000;
/// Above `SMOKE_MAX_LATENCY_MS` (120 s) so the smoke step — not the client —
/// decides what "too slow" means.
const HTTP_POST_TIMEOUT: Duration = Duration::from_secs(180);

/// Production system effects.
#[derive(Debug, Default)]
pub struct RealSystemOps {
    /// Set by the step runner when a step's budget expires; checked
    /// by the blocking extraction loops, which run on threads a
    /// dropped future cannot cancel. Cleared when a step starts.
    step_cancel: Arc<AtomicBool>,
}

impl RealSystemOps {
    fn io_err(path: &Path, e: impl std::fmt::Display) -> WizardError {
        WizardError::IoError {
            path: path.to_path_buf(),
            reason: e.to_string(),
        }
    }

    /// Parse `nvidia-smi --query-gpu=... --format=csv,noheader` rows.
    ///
    /// Expected row: `0, NVIDIA RTX A4000, GPU-uuid, 16384 MiB, 8.6`
    fn parse_gpu_rows(text: &str, driver: &str) -> Vec<GpuDescriptor> {
        text.lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() {
                    return None;
                }
                // GPU names do not contain commas; split into exactly 5 fields.
                let mut parts = line.splitn(5, ',').map(str::trim);
                let index: u32 = parts.next()?.parse().ok()?;
                let name = parts.next()?.to_string();
                let uuid = parts.next()?.to_string();
                let vram: u64 = parts.next()?.split_whitespace().next()?.parse().ok()?;
                let cap = parts.next()?;
                let mut cc = cap.split('.');
                let major: u32 = cc.next()?.parse().ok()?;
                let minor: u32 = cc.next().unwrap_or("0").parse().ok()?;
                Some(GpuDescriptor {
                    index,
                    name,
                    uuid,
                    total_vram_mib: vram,
                    compute_capability: (major, minor),
                    driver_version: driver.to_string(),
                })
            })
            .collect()
    }

    /// Parse the `Driver Version: 581.57` line from plain `nvidia-smi` output.
    fn parse_driver_version(text: &str) -> String {
        for line in text.lines() {
            if let Some(idx) = line.find("Driver Version:") {
                let rest = line[idx + "Driver Version:".len()..].trim();
                if let Some(ver) = rest.split_whitespace().next() {
                    return ver.to_string();
                }
            }
        }
        "0.0".to_string()
    }
}

mod command;
mod http;
mod process;
mod system_impl;
#[cfg(any(windows, test))]
mod win32;

#[cfg(windows)]
use self::command::*;
use self::http::*;
pub use self::process::*;
#[cfg(windows)]
pub use self::win32::toasts_switched_off;
#[cfg(any(windows, test))]
use self::win32::*;

// The rotation sibling of a log file (`gateway.stdout.log` →
// `gateway.stdout.log.prev`) and the rotating opener. Both live in
// `manager_supervisor::lifecycle` so the service shim and the app share
// the one rotation rule; re-exported here so existing callers compile.
use manager_supervisor::lifecycle::open_rotated_log;
pub use manager_supervisor::lifecycle::prev_log_path;

#[cfg(test)]
mod tests;
