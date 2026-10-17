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
//! strips it on some cards), with enumeration order as the fallback
//! for leftovers. That matching and every other decision here is pure
//! and unit-tested; only the two acquisitions (PDH query, DXGI
//! enumeration) sit behind the [`ProcessMemorySource`] seam, the same
//! pattern [`crate::nvml`] uses for its runtime load. Off Windows the
//! system source reports unavailable and the fixup never engages
//! (no GPU reports WDDM there).

use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::{
    DriverModel, GpuSample, ProcessSample, TelemetryError, TelemetrySource, TelemetrySourceKind,
};

/// One DXGI adapter: its LUID (as a comparable key, see [`luid_key`]),
/// description, and PCI vendor id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterLuid {
    pub luid: u64,
    pub name: String,
    pub vendor_id: u32,
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
/// in GPU-index order (identical names consume adapters in enumeration
/// order). Leftover GPUs and leftover NVIDIA adapters are then paired
/// in order — DXGI and NVML enumerate the same devices in the same
/// order on the single-vendor machines this app runs on. If no adapter
/// reports the NVIDIA vendor at all (a source that cannot see vendor
/// ids), all adapters participate rather than none.
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
    let mut unmatched_gpus: Vec<(u32, &str)> = Vec::new();

    let mut sorted_gpus: Vec<(u32, &str)> = gpus.to_vec();
    sorted_gpus.sort_by_key(|(index, _)| *index);
    for (index, name) in &sorted_gpus {
        let want = normalize_name(name);
        let hit = pool
            .iter()
            .find(|a| !used.contains(&a.luid) && normalize_name(&a.name) == want);
        match hit {
            Some(adapter) => {
                used.insert(adapter.luid);
                out.insert(adapter.luid, *index);
            }
            None => unmatched_gpus.push((*index, name)),
        }
    }
    // Positional fallback for whatever the name pass could not pair.
    let remaining: Vec<&&AdapterLuid> = pool.iter().filter(|a| !used.contains(&a.luid)).collect();
    for ((index, _), adapter) in unmatched_gpus.iter().zip(remaining.iter()) {
        out.insert(adapter.luid, *index);
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
                by_pid_gpu.insert((reading.pid, *gpu_index), reading.dedicated_mib);
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
        let samples = self.inner.sample().await.unwrap_or_default();
        Ok(fixup_wddm_processes(&samples, processes, &*self.counters))
    }

    async fn sample_with_processes(
        &self,
    ) -> Result<(Vec<GpuSample>, Vec<ProcessSample>), TelemetryError> {
        let (samples, processes) = self.inner.sample_with_processes().await?;
        let processes = fixup_wddm_processes(&samples, processes, &*self.counters);
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
        }

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

                let mut size: u32 = 0;
                let mut count: u32 = 0;
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
                let mut buffer = vec![0u8; size as usize];
                let status = perf::PdhGetFormattedCounterArrayW(
                    counter,
                    perf::PDH_FMT_LARGE,
                    &mut size,
                    &mut count,
                    buffer.as_mut_ptr() as *mut perf::PDH_FMT_COUNTERVALUE_ITEM_W,
                );
                if status != 0 {
                    return Err(status_err("PdhGetFormattedCounterArrayW", status));
                }

                let items = std::slice::from_raw_parts(
                    buffer.as_ptr() as *const perf::PDH_FMT_COUNTERVALUE_ITEM_W,
                    count as usize,
                );
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    if item.FmtValue.CStatus != perf::PDH_CSTATUS_VALID_DATA {
                        continue;
                    }
                    if item.szName.is_null() {
                        continue;
                    }
                    let name = {
                        let mut len = 0;
                        while *item.szName.add(len) != 0 {
                            len += 1;
                        }
                        String::from_utf16_lossy(std::slice::from_raw_parts(item.szName, len))
                    };
                    let Some((pid, luid)) = parse_instance_name(&name) else {
                        continue;
                    };
                    let bytes = item.FmtValue.Anonymous.largeValue.max(0) as u64;
                    out.push(ProcessMemoryReading {
                        pid,
                        luid,
                        // Round to nearest MiB: the sub-1 GiB range
                        // carries the context-only distinction, so a
                        // 256 MiB context must not floor to 0.
                        dedicated_mib: (bytes + 512 * 1024) / (1024 * 1024),
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
            },
            AdapterLuid {
                luid: 0x2222,
                name: "NVIDIA Tesla P100-PCIE-16GB".to_string(),
                vendor_id: NVIDIA_VENDOR_ID,
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
    fn adapter_mapping_ignores_non_nvidia_and_falls_back_positionally() {
        let mut adapters = a4000_p100_adapters();
        adapters.insert(
            0,
            AdapterLuid {
                luid: 0x9999,
                name: "Microsoft Basic Render Driver".to_string(),
                vendor_id: 0x1414,
            },
        );
        // Names unmatchable on purpose: positional order must pair the
        // two NVIDIA adapters with GPUs 0 and 1, skipping the renderer.
        let gpus = vec![(0, "Card A"), (1, "Card B")];
        let map = map_adapters_to_gpus(&adapters, &gpus);
        assert_eq!(map.get(&0x1111), Some(&0));
        assert_eq!(map.get(&0x2222), Some(&1));
        assert!(!map.contains_key(&0x9999));
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
