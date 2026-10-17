//! Per-process GPU memory on Windows: the PDH fixup source.
//!
//! ## Why this exists
//!
//! NVML's per-process accounting (and the `nvidia-smi` XML built on it)
//! reports real per-process VRAM only under the **TCC** driver model.
//! Under **WDDM** — the display driver model, e.g. an RTX A4000 driving
//! a monitor — the per-process figure does not exist: the XML carries
//! `N/A`, which this crate used to flatten to 0. Every process on a
//! WDDM GPU then read as holding 0 MiB, and the UI labeled a fully
//! resident 11 GiB model "context only" (field report, 2026-10-16:
//! A4000 all-zero, P100/TCC correct).
//!
//! Windows itself does have the figure under WDDM: the **GPU Process
//! Memory** performance counters (PDH) — the same counters Task
//! Manager's per-process GPU column reads. Their `Dedicated Usage`
//! instances are keyed `pid_<pid>_luid_<hi>_<lo>_phys_<n>`, i.e. by
//! process and by adapter LUID. On Windows, this module's decorator
//! ([`ProcessMemoryFixupSource`]) rewrites the process rows of every
//! WDDM GPU from those counters after each poll:
//!
//! - TCC GPUs (and non-Windows): rows pass through untouched — the
//!   NVML/nvidia-smi figures are real there.
//! - WDDM GPU + counter reading for (pid, that GPU's LUID): the row
//!   carries the measured value.
//! - WDDM GPU + counters unavailable, adapter unmapped, or no reading
//!   for the pid: the row's `vram_mib` becomes `None` — **unknown**,
//!   never 0. Consumers render unknown as unmeasured and must not
//!   treat it as residency (see [`crate::ProcessSample`]).
//!
//! The LUID → GPU-index mapping enumerates DXGI adapters (the only
//! user-mode source of adapter LUIDs) and matches them to the poll's
//! GPUs by name (normalized; DXGI prefixes "NVIDIA " where nvidia-smi
//! strips it on some cards). A name pairs only when it is unique on
//! both sides: two identical cards carry no identifying information
//! in a name (or in the PCI device id — identical twins share it),
//! and a crossed pairing would report one GPU's memory on the other
//! as a confident measurement, which is worse than the `None` this
//! module otherwise prefers. Leftovers pair only when exactly one
//! GPU and one adapter remain unmatched, the one case where order
//! cannot cross anything. That matching and every other decision
//! here is pure and unit-tested; only the two acquisitions (PDH
//! query, DXGI enumeration) sit behind the [`ProcessMemorySource`]
//! seam, the same pattern [`crate::nvml`] uses for its runtime
//! load. Off Windows the system source reports unavailable and the
//! fixup never engages (no GPU reports WDDM there).

use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::{
    DriverModel, GpuSample, ProcessSample, TelemetryError, TelemetrySource, TelemetrySourceKind,
};

/// One DXGI adapter: its LUID (as a comparable key, see [`luid_key`]),
/// description, and PCI vendor/device ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterLuid {
    pub luid: u64,
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
}

/// One `GPU Process Memory\Dedicated Usage` reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessMemoryReading {
    pub pid: u32,
    pub luid: u64,
    pub dedicated_mib: u64,
}

/// The acquisition seam: adapter LUIDs and per-process dedicated-usage
/// readings. Production uses [`SystemProcessMemorySource`]; tests use
/// fakes, so the selection/merge logic stays testable anywhere.
pub trait ProcessMemorySource: Send + Sync {
    /// All display adapters with their LUIDs (DXGI enumeration).
    fn adapters(&self) -> Result<Vec<AdapterLuid>, TelemetryError>;
    /// Current per-(pid, adapter) dedicated GPU memory (PDH query).
    fn readings(&self) -> Result<Vec<ProcessMemoryReading>, TelemetryError>;
}

/// NVIDIA's PCI vendor id; DXGI adapters of other vendors (an iGPU,
/// a remote-display adapter) never correspond to a telemetry GPU.
pub const NVIDIA_VENDOR_ID: u32 = 0x10DE;

/// Combine an LUID's two halves into one comparable key. Both halves
/// are taken as raw bit patterns, so the key from a PDH instance name
/// (hex text) and the key from a DXGI `LUID` struct agree.
pub fn luid_key(high_bits: u32, low_bits: u32) -> u64 {
    ((high_bits as u64) << 32) | low_bits as u64
}

/// Parse a `GPU Process Memory` counter instance name —
/// `pid_4321_luid_0x00000000_0x0000abcd_phys_0` — into (pid, luid key).
/// Returns `None` for anything that does not match the shape (other
/// counter sets' instances, the `_Total` instance).
pub fn parse_instance_name(name: &str) -> Option<(u32, u64)> {
    let rest = name.strip_prefix("pid_")?;
    let (pid_s, rest) = rest.split_once('_')?;
    let pid: u32 = pid_s.parse().ok()?;
    let rest = rest.strip_prefix("luid_")?;
    let (hi_s, rest) = rest.split_once('_')?;
    let (lo_s, _phys) = rest.split_once('_')?;
    let parse_hex = |s: &str| {
        let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
        u32::from_str_radix(s, 16).ok()
    };
    Some((pid, luid_key(parse_hex(hi_s)?, parse_hex(lo_s)?)))
}

/// Normalize a GPU name for adapter↔GPU matching: case-insensitive,
/// whitespace-collapsed, with one leading vendor word removed from
/// either side (DXGI says "NVIDIA RTX A4000" / "NVIDIA Tesla
/// P100-PCIE-16GB"; nvidia-smi's product_name keeps the prefix on the
/// A4000 but drops it on the P100).
fn normalize_name(name: &str) -> String {
    let lower = name.trim().to_lowercase();
    let collapsed = lower.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .strip_prefix("nvidia ")
        .unwrap_or(&collapsed)
        .to_string()
}

/// Map DXGI adapter LUIDs to telemetry GPU indices.
///
/// Primary rule: NVIDIA adapters matched to GPUs by normalized name,
/// but only when the name is unique on both sides. Identical twin
/// cards share their name *and* their PCI device id, so nothing in
/// the available data says which adapter is which GPU — pairing
/// them by enumeration order can cross the two, reporting each
/// backend's memory on the other GPU as a confident `Some(...)`.
/// Unmatched is the honest result there: the fixup renders those
/// rows unknown. Leftover GPUs and adapters pair only when exactly
/// one of each remains — the single case a positional pairing
/// cannot cross. If no adapter reports the NVIDIA vendor at all (a
/// source that cannot see vendor ids), all adapters participate
/// rather than none.
pub fn map_adapters_to_gpus(adapters: &[AdapterLuid], gpus: &[(u32, &str)]) -> HashMap<u64, u32> {
    let nvidia: Vec<&AdapterLuid> = adapters
        .iter()
        .filter(|a| a.vendor_id == NVIDIA_VENDOR_ID)
        .collect();
    let pool: Vec<&AdapterLuid> = if nvidia.is_empty() {
        adapters.iter().collect()
    } else {
        nvidia
    };

    let mut out: HashMap<u64, u32> = HashMap::new();
    let mut used: HashSet<u64> = HashSet::new();
    let mut matched: HashSet<u32> = HashSet::new();

    // Occurrences of each normalized name on the GPU side: a name
    // carried by two GPUs identifies neither.
    let mut gpu_name_counts: HashMap<String, usize> = HashMap::new();
    for (_, name) in gpus {
        *gpu_name_counts.entry(normalize_name(name)).or_insert(0) += 1;
    }

    let mut sorted_gpus: Vec<(u32, &str)> = gpus.to_vec();
    sorted_gpus.sort_by_key(|(index, _)| *index);
    for (index, name) in &sorted_gpus {
        let want = normalize_name(name);
        if gpu_name_counts.get(&want).copied().unwrap_or(0) != 1 {
            continue; // twin GPUs: this name cannot identify one
        }
        let mut candidates = pool
            .iter()
            .filter(|a| !used.contains(&a.luid) && normalize_name(&a.name) == want);
        let (Some(adapter), None) = (candidates.next(), candidates.next()) else {
            continue; // zero or twin adapters under this name
        };
        used.insert(adapter.luid);
        matched.insert(*index);
        out.insert(adapter.luid, *index);
    }
    // Leftover pairing, only when it cannot cross anything: exactly
    // one unmatched GPU and exactly one unused adapter.
    let unmatched: Vec<&(u32, &str)> = sorted_gpus
        .iter()
        .filter(|(index, _)| !matched.contains(index))
        .collect();
    let remaining: Vec<&&AdapterLuid> = pool.iter().filter(|a| !used.contains(&a.luid)).collect();
    if unmatched.len() == 1 && remaining.len() == 1 {
        out.insert(remaining[0].luid, unmatched[0].0);
    }
    out
}

/// Rewrite the process rows of WDDM GPUs from the counter source.
///
/// Rows on TCC/unknown-model GPUs pass through untouched. Rows on
/// WDDM GPUs get the counter value for (pid, the GPU's adapter LUID);
/// anything the counters cannot speak for — source error, unmapped
/// adapter, exited pid — becomes `None` (unknown), never the incoming
/// value, because on a WDDM GPU the incoming per-process figure is the
/// NVML `N/A` and trusting it is the bug being fixed.
pub fn fixup_wddm_processes(
    samples: &[GpuSample],
    mut processes: Vec<ProcessSample>,
    source: &dyn ProcessMemorySource,
) -> Vec<ProcessSample> {
    let wddm: HashSet<u32> = samples
        .iter()
        .filter(|s| s.driver_model == Some(DriverModel::Wddm))
        .map(|s| s.index)
        .collect();
    if wddm.is_empty() || !processes.iter().any(|p| wddm.contains(&p.gpu_index)) {
        return processes;
    }

    let resolved: Option<HashMap<(u32, u32), u64>> = (|| {
        let adapters = source.adapters().ok()?;
        let identities: Vec<(u32, &str)> =
            samples.iter().map(|s| (s.index, s.name.as_str())).collect();
        let luid_by_gpu = map_adapters_to_gpus(&adapters, &identities);
        let mut by_pid_gpu: HashMap<(u32, u32), u64> = HashMap::new();
        for reading in source.readings().ok()? {
            if let Some(gpu_index) = luid_by_gpu.get(&reading.luid) {
                // A pid can hold several counter instances for one
                // adapter (`..._phys_0`, `..._phys_1` on linked /
                // multi-adapter setups); parse_instance_name drops
                // the suffix, so the readings for one (pid, GPU)
                // SUM — a later instance must not overwrite an
                // earlier one.
                *by_pid_gpu.entry((reading.pid, *gpu_index)).or_insert(0) += reading.dedicated_mib;
            }
        }
        Some(by_pid_gpu)
    })();

    for p in processes.iter_mut() {
        if wddm.contains(&p.gpu_index) {
            p.vram_mib = resolved
                .as_ref()
                .and_then(|m| m.get(&(p.pid, p.gpu_index)).copied());
        }
    }
    processes
}

/// A [`TelemetrySource`] decorator applying [`fixup_wddm_processes`]
/// to its inner source's process rows on every poll.
pub struct ProcessMemoryFixupSource<S> {
    inner: S,
    counters: Arc<dyn ProcessMemorySource>,
}

impl<S> ProcessMemoryFixupSource<S> {
    pub fn new(inner: S, counters: impl ProcessMemorySource + 'static) -> Self {
        Self {
            inner,
            counters: Arc::new(counters),
        }
    }

    /// Run the fixup off the async runtime: the production counter
    /// source is synchronous FFI (DXGI enumeration plus a full PDH
    /// open/collect/close) that can block for hundreds of
    /// milliseconds on first load — at the 1 s poll cadence that
    /// would stall the Tokio worker running the poller every tick.
    async fn fixup_blocking(
        &self,
        samples: Vec<GpuSample>,
        processes: Vec<ProcessSample>,
    ) -> Result<Vec<ProcessSample>, TelemetryError> {
        let counters = self.counters.clone();
        tokio::task::spawn_blocking(move || fixup_wddm_processes(&samples, processes, &*counters))
            .await
            .map_err(|e| {
                TelemetryError::ProcessMemoryUnavailable(format!("PDH fixup task failed: {e}"))
            })
    }
}

#[async_trait]
impl<S: TelemetrySource> TelemetrySource for ProcessMemoryFixupSource<S> {
    fn kind(&self) -> TelemetrySourceKind {
        self.inner.kind()
    }

    async fn sample(&self) -> Result<Vec<GpuSample>, TelemetryError> {
        self.inner.sample().await
    }

    async fn processes(&self) -> Result<Vec<ProcessSample>, TelemetryError> {
        // Driver models live on the samples; a process-only poll has
        // to fetch them to know which GPUs are WDDM. (The poll loop
        // uses sample_with_processes, so this costs one extra query
        // only for direct processes() callers.)
        let processes = self.inner.processes().await?;
        if processes.is_empty() {
            return Ok(processes);
        }
        let samples = match self.inner.sample().await {
            Ok(samples) => samples,
            Err(_) => {
                // Without the samples the driver models are
                // unknown, so no row's provenance can be verified:
                // the decorator's invariant is that an unverifiable
                // per-process figure is never trusted, so every row
                // degrades to unknown for this poll rather than
                // passing an un-fixed WDDM figure through.
                return Ok(processes
                    .into_iter()
                    .map(|mut p| {
                        p.vram_mib = None;
                        p
                    })
                    .collect());
            }
        };
        self.fixup_blocking(samples, processes).await
    }

    async fn sample_with_processes(
        &self,
    ) -> Result<(Vec<GpuSample>, Vec<ProcessSample>), TelemetryError> {
        let (samples, processes) = self.inner.sample_with_processes().await?;
        let processes = self.fixup_blocking(samples.clone(), processes).await?;
        Ok((samples, processes))
    }
}

/// The production counter source.
///
/// On Windows this performs the real acquisitions (PDH query of
/// `\GPU Process Memory(*)\Dedicated Usage`, DXGI adapter enumeration
/// for LUIDs). Off Windows both acquisitions report unavailable —
/// the fixup then never engages, which is correct: nothing reports
/// the WDDM driver model off Windows.
pub struct SystemProcessMemorySource;

#[cfg(not(windows))]
impl ProcessMemorySource for SystemProcessMemorySource {
    fn adapters(&self) -> Result<Vec<AdapterLuid>, TelemetryError> {
        Err(TelemetryError::ProcessMemoryUnavailable(
            "GPU adapter LUID enumeration is Windows-only".to_string(),
        ))
    }

    fn readings(&self) -> Result<Vec<ProcessMemoryReading>, TelemetryError> {
        Err(TelemetryError::ProcessMemoryUnavailable(
            "GPU Process Memory counters are Windows-only".to_string(),
        ))
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::LUID;
    use windows_sys::Win32::System::Performance as perf;

    impl ProcessMemorySource for SystemProcessMemorySource {
        fn adapters(&self) -> Result<Vec<AdapterLuid>, TelemetryError> {
            dxgi::enumerate_adapters()
        }

        fn readings(&self) -> Result<Vec<ProcessMemoryReading>, TelemetryError> {
            pdh_query::dedicated_usage()
        }
    }

    fn wide_z(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Minimal DXGI surface, declared locally: the windows-sys build
    /// in this workspace carries no Dxgi module. The COM vtable layout
    /// below is the published DXGI ABI (IUnknown, then IDXGIObject,
    /// then the factory/adapter methods in declaration order) and is
    /// stable across Windows versions.
    mod dxgi {
        use super::*;

        // IID_IDXGIFactory1 {770aae78-f26f-4dba-a829-253c83d1b387}
        const IID_IDXGI_FACTORY1: windows_sys::core::GUID =
            windows_sys::core::GUID::from_u128(0x770aae78_f26f_4dba_a829_253c83d1b387);
        const DXGI_ERROR_NOT_FOUND: i32 = 0x887A_0002u32 as i32;

        #[repr(C)]
        struct DxgiAdapterDesc1 {
            description: [u16; 128],
            vendor_id: u32,
            device_id: u32,
            subsys_id: u32,
            revision: u32,
            dedicated_video_memory: usize,
            dedicated_system_memory: usize,
            shared_system_memory: usize,
            adapter_luid: LUID,
            // DXGI_ADAPTER_DESC1's trailing field — the one DESC1
            // added over DESC. Omitting it makes this struct 304
            // bytes against the real 312, and GetDesc1 writes the
            // full descriptor through the pointer: a stack buffer
            // overflow on every enumeration.
            flags: u32,
        }

        // The real DXGI_ADAPTER_DESC1 is 312 bytes on x86_64
        // (256 description + 16 ids + 24 memory + 8 LUID + 4 flags
        // + 4 tail padding). Pin the layout so a field edit can
        // never silently shrink it again.
        const _: () = assert!(std::mem::size_of::<DxgiAdapterDesc1>() == 312);

        /// One COM interface pointer plus the vtable slots this
        /// module calls. Unused slots are opaque addresses — only
        /// their position matters.
        struct ComPtr(*mut c_void);

        impl ComPtr {
            fn slot(&self, index: usize) -> *const c_void {
                unsafe {
                    let vtbl = *(self.0 as *const *const *const c_void);
                    *vtbl.add(index)
                }
            }

            unsafe fn release(&self) {
                // IUnknown::Release is slot 2 on every interface.
                let f: unsafe extern "system" fn(*mut c_void) -> u32 =
                    std::mem::transmute(self.slot(2));
                f(self.0);
            }
        }

        #[link(name = "dxgi")]
        extern "system" {
            fn CreateDXGIFactory1(
                riid: *const windows_sys::core::GUID,
                pp_factory: *mut *mut c_void,
            ) -> i32;
        }

        pub fn enumerate_adapters() -> Result<Vec<AdapterLuid>, TelemetryError> {
            let unavailable = |what: &str, hr: i32| {
                TelemetryError::ProcessMemoryUnavailable(format!(
                    "DXGI {what} failed: HRESULT {hr:#010x}"
                ))
            };
            unsafe {
                let mut factory_raw: *mut c_void = std::ptr::null_mut();
                let hr = CreateDXGIFactory1(&IID_IDXGI_FACTORY1, &mut factory_raw);
                if hr < 0 || factory_raw.is_null() {
                    return Err(unavailable("CreateDXGIFactory1", hr));
                }
                let factory = ComPtr(factory_raw);
                // IDXGIFactory1::EnumAdapters1 is vtable slot 12
                // (IUnknown 0-2, IDXGIObject 3-6, IDXGIFactory 7-11,
                // then IDXGIFactory1::EnumAdapters1).
                let enum_adapters1: unsafe extern "system" fn(
                    *mut c_void,
                    u32,
                    *mut *mut c_void,
                ) -> i32 = std::mem::transmute(factory.slot(12));

                let mut out = Vec::new();
                for i in 0.. {
                    let mut adapter_raw: *mut c_void = std::ptr::null_mut();
                    let hr = enum_adapters1(factory.0, i, &mut adapter_raw);
                    if hr == DXGI_ERROR_NOT_FOUND {
                        break;
                    }
                    if hr < 0 || adapter_raw.is_null() {
                        factory.release();
                        return Err(unavailable("EnumAdapters1", hr));
                    }
                    let adapter = ComPtr(adapter_raw);
                    // IDXGIAdapter1::GetDesc1 is vtable slot 10
                    // (IUnknown 0-2, IDXGIObject 3-6, IDXGIAdapter
                    // 7-9, then IDXGIAdapter1::GetDesc1).
                    let get_desc1: unsafe extern "system" fn(
                        *mut c_void,
                        *mut DxgiAdapterDesc1,
                    ) -> i32 = std::mem::transmute(adapter.slot(10));
                    let mut desc = DxgiAdapterDesc1 {
                        description: [0; 128],
                        vendor_id: 0,
                        device_id: 0,
                        subsys_id: 0,
                        revision: 0,
                        dedicated_video_memory: 0,
                        dedicated_system_memory: 0,
                        shared_system_memory: 0,
                        adapter_luid: LUID {
                            LowPart: 0,
                            HighPart: 0,
                        },
                        flags: 0,
                    };
                    let hr = get_desc1(adapter.0, &mut desc);
                    adapter.release();
                    if hr < 0 {
                        factory.release();
                        return Err(unavailable("GetDesc1", hr));
                    }
                    let name_len = desc
                        .description
                        .iter()
                        .position(|c| *c == 0)
                        .unwrap_or(desc.description.len());
                    out.push(AdapterLuid {
                        luid: luid_key(
                            desc.adapter_luid.HighPart as u32,
                            desc.adapter_luid.LowPart,
                        ),
                        name: String::from_utf16_lossy(&desc.description[..name_len]),
                        vendor_id: desc.vendor_id,
                        device_id: desc.device_id,
                    });
                }
                factory.release();
                Ok(out)
            }
        }
    }

    /// PDH query of `\GPU Process Memory(*)\Dedicated Usage`.
    mod pdh_query {
        use super::*;

        struct QueryGuard(perf::PDH_HQUERY);

        impl Drop for QueryGuard {
            fn drop(&mut self) {
                unsafe {
                    perf::PdhCloseQuery(self.0);
                }
            }
        }

        fn status_err(what: &str, status: u32) -> TelemetryError {
            TelemetryError::ProcessMemoryUnavailable(format!(
                "PDH {what} failed: status {status:#010x}"
            ))
        }

        pub fn dedicated_usage() -> Result<Vec<ProcessMemoryReading>, TelemetryError> {
            unsafe {
                let mut query: perf::PDH_HQUERY = std::ptr::null_mut();
                let status = perf::PdhOpenQueryW(std::ptr::null(), 0, &mut query);
                if status != 0 {
                    return Err(status_err("PdhOpenQueryW", status));
                }
                let _guard = QueryGuard(query);

                let path = wide_z("\\GPU Process Memory(*)\\Dedicated Usage");
                let mut counter: perf::PDH_HCOUNTER = std::ptr::null_mut();
                let status = perf::PdhAddEnglishCounterW(query, path.as_ptr(), 0, &mut counter);
                if status != 0 {
                    return Err(status_err("PdhAddEnglishCounterW", status));
                }
                // Dedicated Usage is an instantaneous (raw) counter:
                // one collect is enough, unlike rate counters.
                let status = perf::PdhCollectQueryData(query);
                if status != 0 {
                    return Err(status_err("PdhCollectQueryData", status));
                }

                // Size, then fill — with one re-size retry: a
                // process can spawn between the two calls, growing
                // the array and making the fill answer PDH_MORE_DATA;
                // that must re-size, not fail the whole poll.
                // The backing store is u64 words, not bytes: the item
                // array holds pointers and needs 8-byte alignment a
                // Vec<u8> does not guarantee.
                let mut buffer: Vec<u64> = Vec::new();
                let mut size: u32 = 0;
                let mut count: u32 = 0;
                for attempt in 0..2 {
                    size = 0;
                    count = 0;
                    let status = perf::PdhGetFormattedCounterArrayW(
                        counter,
                        perf::PDH_FMT_LARGE,
                        &mut size,
                        &mut count,
                        std::ptr::null_mut(),
                    );
                    if status != perf::PDH_MORE_DATA || size == 0 {
                        return Err(status_err("PdhGetFormattedCounterArrayW (sizing)", status));
                    }
                    buffer = vec![0u64; (size as usize).div_ceil(8)];
                    let mut fill_size = size;
                    let status = perf::PdhGetFormattedCounterArrayW(
                        counter,
                        perf::PDH_FMT_LARGE,
                        &mut fill_size,
                        &mut count,
                        buffer.as_mut_ptr() as *mut perf::PDH_FMT_COUNTERVALUE_ITEM_W,
                    );
                    if status == 0 {
                        size = fill_size;
                        break;
                    }
                    if status != perf::PDH_MORE_DATA || attempt == 1 {
                        return Err(status_err("PdhGetFormattedCounterArrayW", status));
                    }
                }
                // Never trust count past the buffer actually filled.
                let item_size = std::mem::size_of::<perf::PDH_FMT_COUNTERVALUE_ITEM_W>();
                let capacity = (buffer.len() * 8) / item_size;
                if (count as usize) > capacity {
                    return Err(status_err(
                        "PdhGetFormattedCounterArrayW (count)",
                        perf::PDH_MORE_DATA,
                    ));
                }

                let items = std::slice::from_raw_parts(
                    buffer.as_ptr() as *const perf::PDH_FMT_COUNTERVALUE_ITEM_W,
                    count as usize,
                );
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    // VALID_DATA ("unchanged since last read") and
                    // NEW_DATA ("changed since last read") are both
                    // success statuses. This query is opened fresh
                    // on every call, so there is no meaningful last
                    // read: an actively-changing allocation (model
                    // loading, KV growth) reports NEW_DATA, and
                    // accepting only VALID_DATA discards exactly
                    // those readings.
                    if item.FmtValue.CStatus != perf::PDH_CSTATUS_VALID_DATA
                        && item.FmtValue.CStatus != perf::PDH_CSTATUS_NEW_DATA
                    {
                        continue;
                    }
                    if item.szName.is_null() {
                        continue;
                    }
                    let name = {
                        // Bounded scan: instance names are short
                        // (`pid_.._luid_.._phys_..`); never walk the
                        // buffer looking for a NUL without a limit.
                        let mut len = 0;
                        while len < 1024 && *item.szName.add(len) != 0 {
                            len += 1;
                        }
                        String::from_utf16_lossy(std::slice::from_raw_parts(item.szName, len))
                    };
                    let Some((pid, luid)) = parse_instance_name(&name) else {
                        continue;
                    };
                    let bytes = item.FmtValue.Anonymous.largeValue.max(0) as u64;
                    // Round to nearest MiB: the sub-1 GiB range
                    // carries the context-only distinction, so a
                    // 256 MiB context must not floor to 0. A real
                    // non-zero allocation below the rounding floor
                    // still reports 1, never a measured-looking
                    // Some(0) — 0 is reserved for a genuinely
                    // measured zero.
                    let rounded = (bytes + 512 * 1024) / (1024 * 1024);
                    out.push(ProcessMemoryReading {
                        pid,
                        luid,
                        dedicated_mib: if bytes > 0 { rounded.max(1) } else { 0 },
                    });
                }
                Ok(out)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GpuSample, TelemetrySourceKind};

    fn sample(index: u32, name: &str, model: Option<DriverModel>) -> GpuSample {
        let mut s = GpuSample::fresh(
            index,
            name,
            10.0,
            1024,
            16384,
            50.0,
            60.0,
            TelemetrySourceKind::NvidiaSmi,
        );
        s.driver_model = model;
        s
    }

    fn proc(pid: u32, gpu_index: u32, vram_mib: Option<u64>) -> ProcessSample {
        ProcessSample {
            pid,
            name: "llama-server".to_string(),
            vram_mib,
            gpu_index,
            backend_id: None,
        }
    }

    struct FakeCounters {
        adapters: Result<Vec<AdapterLuid>, String>,
        readings: Result<Vec<ProcessMemoryReading>, String>,
    }

    impl ProcessMemorySource for FakeCounters {
        fn adapters(&self) -> Result<Vec<AdapterLuid>, TelemetryError> {
            self.adapters
                .clone()
                .map_err(TelemetryError::ProcessMemoryUnavailable)
        }
        fn readings(&self) -> Result<Vec<ProcessMemoryReading>, TelemetryError> {
            self.readings
                .clone()
                .map_err(TelemetryError::ProcessMemoryUnavailable)
        }
    }

    fn a4000_p100_adapters() -> Vec<AdapterLuid> {
        vec![
            AdapterLuid {
                luid: 0x1111,
                name: "NVIDIA RTX A4000".to_string(),
                vendor_id: NVIDIA_VENDOR_ID,
                device_id: 0x2486,
            },
            AdapterLuid {
                luid: 0x2222,
                name: "NVIDIA Tesla P100-PCIE-16GB".to_string(),
                vendor_id: NVIDIA_VENDOR_ID,
                device_id: 0x15F9,
            },
        ]
    }

    #[test]
    fn instance_name_parsing() {
        assert_eq!(
            parse_instance_name("pid_4321_luid_0x00000000_0x0000abcd_phys_0"),
            Some((4321, 0xabcd))
        );
        assert_eq!(
            parse_instance_name("pid_7_luid_0x00010002_0x00030004_phys_1"),
            Some((7, (0x10002u64 << 32) | 0x30004))
        );
        assert_eq!(parse_instance_name("_Total"), None);
        assert_eq!(parse_instance_name("pid_x_luid_0x0_0x0_phys_0"), None);
        assert_eq!(parse_instance_name(""), None);
        assert_eq!(parse_instance_name("pid_1_luid_zz_0x0_phys_0"), None);
    }

    #[test]
    fn adapter_mapping_matches_by_normalized_name() {
        // DXGI names carry the "NVIDIA " vendor prefix on both cards;
        // nvidia-smi drops it on the P100. Mapping must still pair them.
        let gpus = vec![(0, "NVIDIA RTX A4000"), (1, "Tesla P100-PCIE-16GB")];
        let map = map_adapters_to_gpus(&a4000_p100_adapters(), &gpus);
        assert_eq!(map.get(&0x1111), Some(&0));
        assert_eq!(map.get(&0x2222), Some(&1));
    }

    #[test]
    fn adapter_mapping_ignores_non_nvidia_and_never_pairs_ambiguously() {
        let mut adapters = a4000_p100_adapters();
        adapters.insert(
            0,
            AdapterLuid {
                luid: 0x9999,
                name: "Microsoft Basic Render Driver".to_string(),
                vendor_id: 0x1414,
                device_id: 0,
            },
        );
        // Names unmatchable on purpose, with two GPUs and two
        // NVIDIA adapters left over: a positional pairing could
        // cross them, so nothing pairs. The non-NVIDIA renderer is
        // never in the pool.
        let gpus = vec![(0, "Card A"), (1, "Card B")];
        let map = map_adapters_to_gpus(&adapters, &gpus);
        assert!(map.is_empty());
        assert!(!map.contains_key(&0x9999));
        // One unmatched GPU and one unused NVIDIA adapter is the
        // only leftover case that cannot cross: it pairs.
        let gpus = vec![(0, "NVIDIA RTX A4000"), (1, "Card B")];
        let map = map_adapters_to_gpus(&adapters, &gpus);
        assert_eq!(map.get(&0x1111), Some(&0));
        assert_eq!(map.get(&0x2222), Some(&1));
    }

    #[test]
    fn adapter_mapping_leaves_identical_twins_unmapped() {
        // Two RTX A4000s: the name (and the PCI device id) is the
        // same for both, so no pairing is identifiable. Enumeration
        // order must NOT decide — a crossed pairing reports each
        // backend's memory on the other GPU as fact.
        let adapters = vec![
            AdapterLuid {
                luid: 0x1111,
                name: "NVIDIA RTX A4000".to_string(),
                vendor_id: NVIDIA_VENDOR_ID,
                device_id: 0x2486,
            },
            AdapterLuid {
                luid: 0x3333,
                name: "NVIDIA RTX A4000".to_string(),
                vendor_id: NVIDIA_VENDOR_ID,
                device_id: 0x2486,
            },
        ];
        let gpus = vec![(0, "NVIDIA RTX A4000"), (1, "NVIDIA RTX A4000")];
        let map = map_adapters_to_gpus(&adapters, &gpus);
        assert!(map.is_empty(), "twins stay unmapped, got {map:?}");
    }

    #[test]
    fn adapter_mapping_partial_match_does_not_cross_leftovers() {
        // GPU 1 matches by name; GPUs 0 and 2 fail name matching
        // (suffix variants). DXGI enumerates the leftovers in the
        // opposite order — the old positional zip assigned
        // adapter-of-2 to GPU 0 and adapter-of-0 to GPU 2.
        let adapters = vec![
            AdapterLuid {
                luid: 0xAAAA,
                name: "NVIDIA RTX A4000".to_string(),
                vendor_id: NVIDIA_VENDOR_ID,
                device_id: 0x2486,
            },
            AdapterLuid {
                luid: 0xBBBB,
                name: "NVIDIA GeForce RTX 4070".to_string(),
                vendor_id: NVIDIA_VENDOR_ID,
                device_id: 0x2786,
            },
            AdapterLuid {
                luid: 0xCCCC,
                name: "NVIDIA RTX A4000 GA104".to_string(),
                vendor_id: NVIDIA_VENDOR_ID,
                device_id: 0x2486,
            },
        ];
        let gpus = vec![
            (0, "RTX A4000 (OEM)"),
            (1, "NVIDIA GeForce RTX 4070"),
            (2, "RTX A4000 rev2"),
        ];
        let map = map_adapters_to_gpus(&adapters, &gpus);
        assert_eq!(map.get(&0xBBBB), Some(&1), "the one name match pairs");
        assert_eq!(map.len(), 1, "the two leftovers stay unmapped: {map:?}");
    }

    #[test]
    fn instance_name_high_bit_luid_and_uppercase_prefix() {
        // A LUID whose high half has the sign bit set must survive
        // as raw bits (the DXGI side builds the key the same way).
        assert_eq!(
            parse_instance_name("pid_9_luid_0x80000000_0x00000001_phys_0"),
            Some((9, 0x8000_0000_0000_0001))
        );
        assert_eq!(
            parse_instance_name("pid_9_luid_0X00000001_0X00000002_phys_0"),
            Some((9, (1u64 << 32) | 2))
        );
    }

    #[test]
    fn fixup_sums_phys_instances_for_one_pid_and_gpu() {
        // Two counter instances for one (pid, adapter) — the phys_0
        // and phys_1 of a linked adapter — sum into one reading.
        let samples = vec![sample(0, "NVIDIA RTX A4000", Some(DriverModel::Wddm))];
        let processes = vec![proc(4321, 0, None)];
        let counters = FakeCounters {
            adapters: Ok(a4000_p100_adapters()),
            readings: Ok(vec![
                ProcessMemoryReading {
                    pid: 4321,
                    luid: 0x1111,
                    dedicated_mib: 3000,
                },
                ProcessMemoryReading {
                    pid: 4321,
                    luid: 0x1111,
                    dedicated_mib: 2500,
                },
            ]),
        };
        let out = fixup_wddm_processes(&samples, processes, &counters);
        assert_eq!(out[0].vram_mib, Some(5500));
    }

    #[test]
    fn pre_0132_process_sample_payload_still_deserializes() {
        // Pre-0.1.32 payloads carried vram_mib as a bare number
        // (the u64 -> Option<u64> change); the shape must keep
        // deserializing.
        let p: ProcessSample = serde_json::from_str(
            r#"{"pid":4321,"name":"llama-server","vram_mib":11264,"gpu_index":1,"backend_id":null}"#,
        )
        .unwrap();
        assert_eq!(p.vram_mib, Some(11264));
    }

    #[test]
    fn fixup_replaces_wddm_rows_from_counters() {
        let samples = vec![
            sample(0, "NVIDIA RTX A4000", Some(DriverModel::Wddm)),
            sample(1, "Tesla P100-PCIE-16GB", Some(DriverModel::Tcc)),
        ];
        // As parsed from nvidia-smi XML: the WDDM row is N/A -> None,
        // the TCC row carries its real figure.
        let processes = vec![proc(4321, 0, None), proc(4321, 1, Some(11264))];
        let counters = FakeCounters {
            adapters: Ok(a4000_p100_adapters()),
            readings: Ok(vec![ProcessMemoryReading {
                pid: 4321,
                luid: 0x1111,
                dedicated_mib: 6144,
            }]),
        };
        let out = fixup_wddm_processes(&samples, processes, &counters);
        assert_eq!(out[0].vram_mib, Some(6144), "WDDM row takes the PDH value");
        assert_eq!(out[1].vram_mib, Some(11264), "TCC row passes through");
    }

    #[test]
    fn fixup_counter_failure_yields_unknown_never_zero() {
        let samples = vec![sample(0, "NVIDIA RTX A4000", Some(DriverModel::Wddm))];
        let processes = vec![proc(4321, 0, None)];
        let counters = FakeCounters {
            adapters: Err("pdh down".to_string()),
            readings: Err("pdh down".to_string()),
        };
        let out = fixup_wddm_processes(&samples, processes, &counters);
        assert_eq!(out[0].vram_mib, None, "unavailable counters -> unknown");
    }

    #[test]
    fn fixup_missing_reading_for_pid_is_unknown() {
        let samples = vec![sample(0, "NVIDIA RTX A4000", Some(DriverModel::Wddm))];
        let processes = vec![proc(4321, 0, None), proc(9876, 0, None)];
        let counters = FakeCounters {
            adapters: Ok(a4000_p100_adapters()),
            readings: Ok(vec![ProcessMemoryReading {
                pid: 4321,
                luid: 0x1111,
                dedicated_mib: 256,
            }]),
        };
        let out = fixup_wddm_processes(&samples, processes, &counters);
        assert_eq!(out[0].vram_mib, Some(256), "the context row measures 256");
        assert_eq!(out[1].vram_mib, None, "no reading for this pid -> unknown");
    }

    #[test]
    fn fixup_no_wddm_gpus_passes_rows_through() {
        let samples = vec![sample(1, "Tesla P100-PCIE-16GB", Some(DriverModel::Tcc))];
        let processes = vec![proc(4321, 1, Some(11264))];
        // A counters fake that would fail if consulted.
        let counters = FakeCounters {
            adapters: Err("must not be called".to_string()),
            readings: Err("must not be called".to_string()),
        };
        let out = fixup_wddm_processes(&samples, processes, &counters);
        assert_eq!(out[0].vram_mib, Some(11264));
    }

    struct StubInner {
        samples: Vec<GpuSample>,
        processes: Vec<ProcessSample>,
    }

    #[async_trait]
    impl TelemetrySource for StubInner {
        fn kind(&self) -> TelemetrySourceKind {
            TelemetrySourceKind::NvidiaSmi
        }
        async fn sample(&self) -> Result<Vec<GpuSample>, TelemetryError> {
            Ok(self.samples.clone())
        }
        async fn processes(&self) -> Result<Vec<ProcessSample>, TelemetryError> {
            Ok(self.processes.clone())
        }
        async fn sample_with_processes(
            &self,
        ) -> Result<(Vec<GpuSample>, Vec<ProcessSample>), TelemetryError> {
            Ok((self.samples.clone(), self.processes.clone()))
        }
    }

    #[tokio::test]
    async fn decorator_applies_fixup_on_combined_poll() {
        let inner = StubInner {
            samples: vec![sample(0, "NVIDIA RTX A4000", Some(DriverModel::Wddm))],
            processes: vec![proc(4321, 0, None)],
        };
        let counters = FakeCounters {
            adapters: Ok(a4000_p100_adapters()),
            readings: Ok(vec![ProcessMemoryReading {
                pid: 4321,
                luid: 0x1111,
                dedicated_mib: 11008,
            }]),
        };
        let src = ProcessMemoryFixupSource::new(inner, counters);
        let (samples, processes) = src.sample_with_processes().await.expect("poll ok");
        assert_eq!(samples.len(), 1);
        assert_eq!(processes[0].vram_mib, Some(11008));
        assert_eq!(src.kind(), TelemetrySourceKind::NvidiaSmi);
    }

    #[tokio::test]
    async fn system_source_unavailable_off_windows() {
        #[cfg(not(windows))]
        {
            let src = SystemProcessMemorySource;
            assert!(matches!(
                src.adapters(),
                Err(TelemetryError::ProcessMemoryUnavailable(_))
            ));
            assert!(matches!(
                src.readings(),
                Err(TelemetryError::ProcessMemoryUnavailable(_))
            ));
        }
    }
}
