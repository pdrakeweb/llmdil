//! CPU / system-RAM inventory probe (design §2.1).
//!
//! The app knows GPU memory from telemetry but has never needed
//! total system RAM or a CPU inventory. Values that cannot be
//! read are `None` — RAM is never estimated from the GGUF or the
//! page file, and never derived from `sys_weights_mib` figures.

/// What the probe could read on this machine.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HardwareProbe {
    pub ram_total_mib: Option<u64>,
    pub cpu_model: Option<String>,
    pub cpu_cores: Option<usize>,
}

/// Read the machine's CPU model, logical core count, and total
/// physical RAM. Cheap and synchronous; safe on a command's
/// critical path.
pub fn probe_hardware() -> HardwareProbe {
    let mut probe = HardwareProbe {
        cpu_cores: std::thread::available_parallelism()
            .ok()
            .map(|n| n.get()),
        ..Default::default()
    };
    #[cfg(windows)]
    windows_fill(&mut probe);
    #[cfg(target_os = "linux")]
    linux_fill(&mut probe);
    probe
}

#[cfg(windows)]
#[repr(C)]
struct MemoryStatusEx {
    dw_length: u32,
    dw_memory_load: u32,
    ull_total_phys: u64,
    ull_avail_phys: u64,
    ull_total_page_file: u64,
    ull_avail_page_file: u64,
    ull_total_virtual: u64,
    ull_avail_virtual: u64,
    ull_avail_extended_virtual: u64,
}

#[cfg(windows)]
extern "system" {
    fn GlobalMemoryStatusEx(lp_buffer: *mut MemoryStatusEx) -> i32;
}

#[cfg(windows)]
fn windows_fill(probe: &mut HardwareProbe) {
    let mut status = MemoryStatusEx {
        dw_length: std::mem::size_of::<MemoryStatusEx>() as u32,
        dw_memory_load: 0,
        ull_total_phys: 0,
        ull_avail_phys: 0,
        ull_total_page_file: 0,
        ull_avail_page_file: 0,
        ull_total_virtual: 0,
        ull_avail_virtual: 0,
        ull_avail_extended_virtual: 0,
    };
    // SAFETY: the struct is fully initialized with its length
    // field set, as GlobalMemoryStatusEx requires; the call
    // writes only into it.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    if ok != 0 && status.ull_total_phys > 0 {
        probe.ram_total_mib = Some(status.ull_total_phys / (1024 * 1024));
    }
    if let Ok(model) = std::env::var("PROCESSOR_IDENTIFIER") {
        let model = model.trim().to_string();
        if !model.is_empty() {
            probe.cpu_model = Some(model);
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_fill(probe: &mut HardwareProbe) {
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        for line in meminfo.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                if let Ok(kb) = rest.trim().trim_end_matches("kB").trim().parse::<u64>() {
                    probe.ram_total_mib = Some(kb / 1024);
                }
                break;
            }
        }
    }
    if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
        for line in cpuinfo.lines() {
            if let Some(rest) = line.strip_prefix("model name") {
                if let Some((_, model)) = rest.split_once(':') {
                    let model = model.trim().to_string();
                    if !model.is_empty() {
                        probe.cpu_model = Some(model);
                    }
                }
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_returns_plausible_values_on_this_platform() {
        let probe = probe_hardware();
        // Every dev/CI machine this runs on has cores; RAM and CPU
        // model are best-effort but on Linux/Windows both read.
        assert!(probe.cpu_cores.unwrap_or(0) >= 1);
        if cfg!(any(windows, target_os = "linux")) {
            assert!(probe.ram_total_mib.unwrap_or(0) > 0);
        }
    }
}
