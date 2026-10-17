//! GPU telemetry (spec §5).
//!
//! Primary source: NVML (see [`nvml`]) — device handles, per-process
//! accounting. Load-bearing fallback: `nvidia-smi --query --xml-format`
//! (see [`smi`]) — parsed on NVML init failure or per-GPU handle failure,
//! so the app starts and monitors correctly with NVML fully unavailable
//! (Pascal-era quirks on the P100s). The UI shows the source per GPU
//! ("NVML" / "nvidia-smi fallback").
//!
//! [`store::TelemetryStore`] is the single-writer snapshot store fed by the
//! poll loop. GPUs are labeled STALE after [`STALE_AFTER_SECS`] seconds
//! without a successful poll; last-known values stay visible and are never
//! synthesized (spec global UI rules).

pub mod nvml;
pub mod pdh;
pub mod smi;
pub mod store;

pub use nvml::NvmlSource;
pub use pdh::{
    AdapterLuid, ProcessMemoryFixupSource, ProcessMemoryReading, ProcessMemorySource,
    SystemProcessMemorySource,
};
pub use smi::{default_smi_path, NvidiaSmiSource};
pub use store::{
    FallbackSource, PeakMark, PeakMarks, PollCadence, PollError, Poller, TelemetryStore,
    HISTORY_CAP,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Seconds without a successful poll before a GPU is labeled STALE.
///
/// Spec §5 says "after one missed tick" for the poll loop while the global UI
/// rules (§4) say "older than 10 s"; the crate contract fixes this at 10 s,
/// which also covers the 5 s cadence without flapping. Resolved in favor of
/// the contract value.
pub const STALE_AFTER_SECS: u64 = 10;

/// Which source produced a sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TelemetrySourceKind {
    Nvml,
    NvidiaSmi,
}

/// The Windows driver model a GPU's driver is running, when the
/// telemetry source reports it (nvidia-smi's `driver_model/current_dm`,
/// NVML's `nvmlDeviceGetDriverModel` equivalent). Load-bearing for
/// per-process accounting: NVML/nvidia-smi per-process memory figures
/// are real under TCC but read 0 under WDDM (see [`pdh`]), so consumers
/// must know which model produced a process row before trusting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriverModel {
    Wddm,
    Tcc,
}

/// Static inventory for one GPU.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuInfo {
    pub index: u32,
    pub name: String,
    pub uuid: String,
    /// e.g. (8, 6) for Ampere, (6, 0) for Pascal P100. (0, 0) = unknown
    /// (nvidia-smi XML does not report compute capability; it is mapped
    /// from known product names where possible).
    pub compute_capability: (u32, u32),
    pub total_vram_mib: u64,
    /// PCI bus id, e.g. "00000000:65:00.0".
    pub pci_bus_id: String,
    /// Driver version string, e.g. "581.57".
    pub driver_version: String,
    /// Windows driver model (WDDM/TCC) when reported; `None` when the
    /// source did not say (older payloads, non-Windows).
    #[serde(default)]
    pub driver_model: Option<DriverModel>,
    pub source: TelemetrySourceKind,
}

/// One telemetry sample for one GPU.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuSample {
    pub index: u32,
    pub name: String,
    pub ts: DateTime<Utc>,
    pub utilization_pct: f32,
    pub vram_used_mib: u64,
    pub vram_total_mib: u64,
    pub temp_c: f32,
    /// Instant power draw W, when the card reports it. Some cards
    /// answer `N/A` for power readings in nvidia-smi XML, and
    /// nvidia-smi v570+ renamed the power elements entirely (see
    /// [`smi`]); both cases must degrade to None, never fail the
    /// sample or the source.
    pub power_w: Option<f32>,
    /// True when no successful poll within [`STALE_AFTER_SECS`].
    pub stale: bool,
    /// True when this GPU was absent from the latest successful poll
    /// after having been reported before — the store retains it,
    /// flagged down with its last-known readings, instead of dropping
    /// it from the snapshot. This is the "GPU is lost" case (fallen
    /// off the bus): the card must read as down in the UI, not
    /// silently vanish (operator requirement, 2026-10-16). Sources
    /// never set it; only the store does, and the first poll that
    /// reports the GPU again clears it.
    #[serde(default)]
    pub down: bool,
    pub source: TelemetrySourceKind,
    /// Memory-controller utilization %, when reported.
    pub mem_util_pct: Option<f32>,
    /// Fan speed %, when reported (P100s have no fan: None).
    pub fan_pct: Option<f32>,
    /// Power limit W, when exposed.
    pub power_limit_w: Option<f32>,
    /// Graphics clock MHz, when reported.
    pub clocks_graphics_mhz: Option<u32>,
    /// Memory clock MHz, when reported.
    pub clocks_mem_mhz: Option<u32>,
    /// Windows driver model (WDDM/TCC) when the source reports it;
    /// decides whether this GPU's per-process rows are trustworthy
    /// (see [`pdh`]). `None` when not reported.
    #[serde(default)]
    pub driver_model: Option<DriverModel>,
}

impl GpuSample {
    /// Derived free VRAM (drives the model fit-check). Never negative:
    /// clamps at 0 if a driver ever reports used > total.
    pub fn free_vram_mib(&self) -> u64 {
        self.vram_total_mib.saturating_sub(self.vram_used_mib)
    }

    /// Fresh (non-stale) sample stamped now.
    ///
    /// Eight parameters mirrors the struct's required fields; this is a
    /// test/setup convenience constructor, not a general API (production
    /// code builds samples from source-specific parsers).
    #[allow(clippy::too_many_arguments)]
    pub fn fresh(
        index: u32,
        name: &str,
        utilization_pct: f32,
        vram_used_mib: u64,
        vram_total_mib: u64,
        temp_c: f32,
        power_w: f32,
        source: TelemetrySourceKind,
    ) -> Self {
        Self {
            index,
            name: name.to_string(),
            ts: Utc::now(),
            utilization_pct,
            vram_used_mib,
            vram_total_mib,
            temp_c,
            power_w: Some(power_w),
            stale: false,
            down: false,
            source,
            mem_util_pct: None,
            fan_pct: None,
            power_limit_w: None,
            clocks_graphics_mhz: None,
            clocks_mem_mhz: None,
            driver_model: None,
        }
    }
}

/// Per-process VRAM attribution (NVML accounting; best-effort on fallback).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessSample {
    pub pid: u32,
    pub name: String,
    /// VRAM this process holds on `gpu_index`, in MiB — or `None` when
    /// the figure was **not measured**. Unknown is a real state, not a
    /// zero: under the WDDM driver model NVML/nvidia-smi per-process
    /// accounting reports nothing usable (the XML says `N/A`), and on
    /// Windows the value is filled in from the GPU Process Memory
    /// performance counters instead (see [`pdh`]). When even those are
    /// unavailable the row stays `None`, and consumers must render it
    /// as unmeasured — never as 0 MiB, never as "context only", and
    /// never as measured residency.
    pub vram_mib: Option<u64>,
    /// Index of the GPU this process was collected under — the same index
    /// assigned to that GPU's [`GpuSample`]. Processes are reported per GPU
    /// by both sources (nested per-GPU in the nvidia-smi XML), and one pid
    /// legitimately appears once per GPU it holds memory on (tensor split).
    pub gpu_index: u32,
    /// Backend id this process belongs to, when known.
    pub backend_id: Option<String>,
}

/// A pollable telemetry source.
#[async_trait]
pub trait TelemetrySource: Send + Sync {
    fn kind(&self) -> TelemetrySourceKind;
    async fn sample(&self) -> Result<Vec<GpuSample>, TelemetryError>;
    async fn processes(&self) -> Result<Vec<ProcessSample>, TelemetryError>;

    /// One poll producing both result sets. The default implementation
    /// makes two calls (kept for sources where a combined query is
    /// impossible); a failed process query degrades to empty attribution
    /// rather than failing the tick. Sources that can do it in one shot —
    /// nvidia-smi is one `nvidia-smi --query --xml-format` spawn — override
    /// this so a tick costs a single acquisition instead of two.
    async fn sample_with_processes(
        &self,
    ) -> Result<(Vec<GpuSample>, Vec<ProcessSample>), TelemetryError> {
        let samples = self.sample().await?;
        let processes = self.processes().await.unwrap_or_default();
        Ok((samples, processes))
    }
}

/// The production telemetry stack: NVML primary, `nvidia-smi` fallback,
/// with the WDDM per-process fixup (see [`pdh`]) applied on top.
///
/// The fallback is load-bearing, not decorative: on machines where NVML is
/// unavailable at all — or where per-GPU handles fail, the documented
/// Pascal/P100 case — every poll transparently comes from `nvidia-smi` and
/// `FallbackSource::active_kind()` reports `NvidiaSmi` so the UI can show
/// the per-GPU "nvidia-smi fallback" label (spec §5). This is the default
/// wiring the app boots with; NVML being unwired on non-Windows builds
/// (see [`nvml`]) is precisely the case the fallback exists for.
pub fn default_telemetry_stack(
) -> ProcessMemoryFixupSource<FallbackSource<NvmlSource, NvidiaSmiSource>> {
    ProcessMemoryFixupSource::new(
        FallbackSource::new(NvmlSource::new(), NvidiaSmiSource::default()),
        SystemProcessMemorySource,
    )
}

/// Telemetry errors.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    #[error("NVML unavailable: {0}")]
    NvmlUnavailable(String),
    #[error("nvidia-smi failed: {0}")]
    SmiFailed(String),
    #[error("XML parse error: {0}")]
    ParseError(String),
    #[error("per-process GPU memory counters unavailable: {0}")]
    ProcessMemoryUnavailable(String),
}
