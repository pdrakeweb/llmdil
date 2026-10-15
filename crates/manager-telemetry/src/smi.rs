//! `nvidia-smi --query --xml-format` source (load-bearing fallback).
//!
//! Runs `nvidia-smi --query --xml-format` (equivalent to `-q -x`) and parses
//! the XML with quick-xml. Tolerant of real-world quirks:
//!
//! - `N/A` values (fan speed on fanless Teslas, power readings on some
//!   configs, clock fields) → `None` for optional metrics, never zero.
//! - Missing optional elements (e.g. no `<clocks>` block, empty
//!   `<processes>`) → `None` / empty, no error.
//! - Unknown extra elements are ignored, so new driver versions that add
//!   fields do not break the parser.
//! - Required fields (`product_name`, `uuid`, memory totals, `gpu_temp`,
//!   `gpu_util`) missing or `N/A` → [`TelemetryError::ParseError`]
//!   naming the GPU. The poller treats a failed poll as a missed tick and the
//!   store marks the GPU STALE; we never synthesize zeros.
//! - `power_draw` is NOT required: some cards (an RTX A4000 under
//!   current drivers) report `N/A` for power readings while every
//!   other field is valid. It degrades to `None` like the other
//!   optional metrics — requiring it took the whole source down on
//!   real hardware (2026-10-15).

use async_trait::async_trait;
use chrono::Utc;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::{
    GpuInfo, GpuSample, ProcessSample, TelemetryError, TelemetrySource, TelemetrySourceKind,
};

/// `nvidia-smi --query --xml-format` source (load-bearing fallback).
pub struct NvidiaSmiSource {
    pub nvidia_smi_path: PathBuf,
}

impl NvidiaSmiSource {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            nvidia_smi_path: path.into(),
        }
    }

    /// Spawn `nvidia-smi --query --xml-format` and capture stdout.
    pub async fn run_query(&self) -> Result<String, TelemetryError> {
        let mut cmd = tokio::process::Command::new(&self.nvidia_smi_path);
        cmd.args(["--query", "--xml-format"]);
        // The poller spawns this from a GUI process every tick; on
        // Windows a console child without CREATE_NO_WINDOW opens a
        // visible console window each time (focus-stealing flash).
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW
            cmd.creation_flags(0x08000000);
        }
        let output = cmd.output().await.map_err(|e| {
            TelemetryError::SmiFailed(format!(
                "failed to spawn {}: {e}",
                self.nvidia_smi_path.display()
            ))
        })?;
        if !output.status.success() {
            return Err(TelemetryError::SmiFailed(format!(
                "exit {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// One spawn, both result sets. Preferred by the poll loop so a tick
    /// costs one process launch instead of two. (Also exposed via the
    /// `TelemetrySource::sample_with_processes` trait method; this inherent
    /// method is kept so existing callers do not need the trait in scope.)
    pub async fn sample_with_processes(
        &self,
    ) -> Result<(Vec<GpuSample>, Vec<ProcessSample>), TelemetryError> {
        let xml = self.run_query().await?;
        let doc = parse_document(&xml)?;
        let ts = Utc::now();
        let mut samples = Vec::with_capacity(doc.gpus.len());
        let mut processes = Vec::new();
        for gpu in &doc.gpus {
            samples.push(gpu.to_sample(ts, &doc.driver_version));
            processes.extend(gpu.processes.iter().cloned());
        }
        Ok((samples, processes))
    }

    /// Parse `nvidia-smi -q -x` XML output into samples.
    pub fn parse_xml(xml: &str) -> Result<Vec<GpuSample>, TelemetryError> {
        let (samples, _) = Self::parse_xml_with_processes(xml)?;
        Ok(samples)
    }

    /// Parse `nvidia-smi -q -x` XML output into (samples, processes).
    pub fn parse_xml_with_processes(
        xml: &str,
    ) -> Result<(Vec<GpuSample>, Vec<ProcessSample>), TelemetryError> {
        let doc = parse_document(xml)?;
        let ts = Utc::now();
        let mut samples = Vec::with_capacity(doc.gpus.len());
        let mut processes = Vec::new();
        for gpu in &doc.gpus {
            samples.push(gpu.to_sample(ts, &doc.driver_version));
            processes.extend(gpu.processes.iter().cloned());
        }
        Ok((samples, processes))
    }

    /// Parse the process list only.
    pub fn parse_processes(xml: &str) -> Result<Vec<ProcessSample>, TelemetryError> {
        let (_, processes) = Self::parse_xml_with_processes(xml)?;
        Ok(processes)
    }

    /// Static inventory (name, UUID, PCI, driver, VRAM) from the same XML.
    pub fn parse_inventory(xml: &str) -> Result<Vec<GpuInfo>, TelemetryError> {
        let doc = parse_document(xml)?;
        Ok(doc
            .gpus
            .iter()
            .map(|g| GpuInfo {
                index: g.index,
                name: g.name.clone(),
                uuid: g.uuid.clone(),
                compute_capability: compute_capability_for(&g.name),
                total_vram_mib: g.mem_total_mib,
                pci_bus_id: g.pci_bus_id.clone(),
                driver_version: doc.driver_version.clone(),
                source: TelemetrySourceKind::NvidiaSmi,
            })
            .collect())
    }

    pub fn binary_path(&self) -> &Path {
        &self.nvidia_smi_path
    }
}

impl Default for NvidiaSmiSource {
    fn default() -> Self {
        Self::new(default_smi_path())
    }
}

/// Program path for the default source.
///
/// On Windows the bare name `nvidia-smi` resolves through PATH (and the
/// current directory), so a planted executable could be picked up by
/// every poll tick. Prefer the canonical System32 location when it
/// exists; fall back to the bare name otherwise. Non-Windows builds are
/// unchanged.
///
/// Public so every spawn site (telemetry poller, wizard probes) resolves
/// the binary the same way instead of re-implementing the rule.
#[cfg(windows)]
pub fn default_smi_path() -> PathBuf {
    let system = PathBuf::from(r"C:\Windows\System32\nvidia-smi.exe");
    if system.exists() {
        system
    } else {
        PathBuf::from("nvidia-smi")
    }
}

#[cfg(not(windows))]
pub fn default_smi_path() -> PathBuf {
    PathBuf::from("nvidia-smi")
}

#[async_trait]
impl TelemetrySource for NvidiaSmiSource {
    fn kind(&self) -> TelemetrySourceKind {
        TelemetrySourceKind::NvidiaSmi
    }

    async fn sample(&self) -> Result<Vec<GpuSample>, TelemetryError> {
        let (samples, _) = self.sample_with_processes().await?;
        Ok(samples)
    }

    async fn processes(&self) -> Result<Vec<ProcessSample>, TelemetryError> {
        let (_, processes) = self.sample_with_processes().await?;
        Ok(processes)
    }

    async fn sample_with_processes(
        &self,
    ) -> Result<(Vec<GpuSample>, Vec<ProcessSample>), TelemetryError> {
        // One nvidia-smi spawn serves both; the inherent method above keeps
        // the concrete-type call path stable.
        NvidiaSmiSource::sample_with_processes(self).await
    }
}

// ---------------------------------------------------------------------------
// XML parsing
// ---------------------------------------------------------------------------

/// One fully parsed `<gpu>` element.
struct ParsedGpu {
    index: u32,
    name: String,
    uuid: String,
    pci_bus_id: String,
    mem_total_mib: u64,
    mem_used_mib: u64,
    gpu_util_pct: f32,
    mem_util_pct: Option<f32>,
    temp_c: f32,
    power_w: Option<f32>,
    power_limit_w: Option<f32>,
    fan_pct: Option<f32>,
    clocks_graphics_mhz: Option<u32>,
    clocks_mem_mhz: Option<u32>,
    processes: Vec<ProcessSample>,
}

impl ParsedGpu {
    fn to_sample(&self, ts: chrono::DateTime<Utc>, _driver_version: &str) -> GpuSample {
        GpuSample {
            index: self.index,
            name: self.name.clone(),
            ts,
            utilization_pct: self.gpu_util_pct,
            vram_used_mib: self.mem_used_mib,
            vram_total_mib: self.mem_total_mib,
            temp_c: self.temp_c,
            power_w: self.power_w,
            stale: false,
            source: TelemetrySourceKind::NvidiaSmi,
            mem_util_pct: self.mem_util_pct,
            fan_pct: self.fan_pct,
            power_limit_w: self.power_limit_w,
            clocks_graphics_mhz: self.clocks_graphics_mhz,
            clocks_mem_mhz: self.clocks_mem_mhz,
        }
    }
}

struct ParsedDoc {
    driver_version: String,
    gpus: Vec<ParsedGpu>,
}

/// Map a product name to its CUDA compute capability. nvidia-smi XML does
/// not report compute capability, so this is a best-effort table;
/// unknown cards yield (0, 0) = unknown (see [`GpuInfo`]).
fn compute_capability_for(product_name: &str) -> (u32, u32) {
    let n = product_name.to_ascii_lowercase();
    if n.contains("p100") {
        (6, 0)
    } else if n.contains("p40") || n.contains("titan xp") {
        (6, 1)
    } else if n.contains("v100") {
        (7, 0)
    } else if n.contains("t4") {
        (7, 5)
    } else if n.contains("a100") {
        (8, 0)
    } else if n.contains("a4000") || n.contains("a5000") || n.contains("a6000") {
        (8, 6)
    } else if n.contains("4090") || n.contains("4080") || n.contains("4070") {
        (8, 9)
    } else {
        (0, 0)
    }
}

// --- value helpers: "N/A" -> None, unit suffixes stripped ---

fn is_na(s: &str) -> bool {
    s.trim().eq_ignore_ascii_case("n/a")
}

fn strip_unit<'a>(s: &'a str, unit: &str) -> Option<&'a str> {
    let s = s.trim();
    if is_na(s) || s.is_empty() {
        return None;
    }
    Some(s.strip_suffix(unit).unwrap_or(s).trim())
}

/// "62 %" -> Some(62.0); "N/A" -> None.
fn parse_percent(s: &str) -> Option<f32> {
    strip_unit(s, "%")?.parse().ok()
}

/// "16384 MiB" -> Some(16384); "N/A" -> None.
fn parse_mib(s: &str) -> Option<u64> {
    strip_unit(s, "MiB")?.parse().ok()
}

/// "67 C" -> Some(67.0); "N/A" -> None.
fn parse_celsius(s: &str) -> Option<f32> {
    strip_unit(s, "C")?.parse().ok()
}

/// "95.30 W" -> Some(95.3); "N/A" -> None.
fn parse_watts(s: &str) -> Option<f32> {
    strip_unit(s, "W")?.parse().ok()
}

/// "1560 MHz" -> Some(1560); "N/A" -> None.
fn parse_mhz(s: &str) -> Option<u32> {
    // Clocks sometimes report as "N/A" or as floats in odd drivers.
    let raw = strip_unit(s, "MHz")?;
    raw.parse::<u32>()
        .ok()
        .or_else(|| raw.parse::<f32>().ok().map(|f| f as u32))
}

fn required<T>(
    label: &str,
    gpu_idx: u32,
    gpu_name: &str,
    value: Option<T>,
) -> Result<T, TelemetryError> {
    value.ok_or_else(|| {
        TelemetryError::ParseError(format!(
            "gpu {gpu_idx} ({gpu_name}): missing or N/A required field '{label}'"
        ))
    })
}

fn parse_document(xml: &str) -> Result<ParsedDoc, TelemetryError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut stack: Vec<String> = Vec::new();
    let mut driver_version = String::from("unknown");
    let mut gpus: Vec<ParsedGpu> = Vec::new();

    // In-progress accumulators.
    let mut gpu_fields: Option<HashMap<String, String>> = None;
    let mut gpu_id_attr = String::new();
    let mut proc_fields: Option<HashMap<String, String>> = None;
    let mut proc_list: Vec<ProcessSample> = Vec::new();
    let mut order: u32 = 0;

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = name_of(e.name())?;
                if name == "gpu" {
                    gpu_id_attr = attr_of(&e, "id").unwrap_or_default();
                    gpu_fields = Some(HashMap::new());
                    proc_list = Vec::new();
                } else if name == "process_info" && gpu_fields.is_some() {
                    proc_fields = Some(HashMap::new());
                }
                stack.push(name);
            }
            Ok(Event::Empty(e)) => {
                // Self-closing element: no text; nothing to record.
                let _ = name_of(e.name())?;
            }
            Ok(Event::Text(e)) => {
                let text = e.xml_content().map(|c| c.into_owned()).unwrap_or_default();
                if stack.len() >= 2 && stack[0] == "nvidia_smi_log" && stack[1] == "driver_version"
                {
                    driver_version = text.trim().to_string();
                } else if stack.len() >= 2 && stack[0] == "nvidia_smi_log" && stack[1] == "gpu" {
                    if stack.iter().any(|s| s == "process_info") {
                        if let Some(pf) = proc_fields.as_mut() {
                            if let Some(leaf) = stack.last() {
                                pf.insert(leaf.clone(), text);
                            }
                        }
                    } else if let Some(gf) = gpu_fields.as_mut() {
                        // Key is the path relative to <gpu>, e.g. "pci/pci_bus".
                        let rel = stack[2..].join("/");
                        gf.insert(rel, text);
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = name_of(e.name())?;
                if name == "process_info" {
                    if let Some(pf) = proc_fields.take() {
                        let label = format!("process on gpu id '{gpu_id_attr}'");
                        let pid: u32 = pf
                            .get("pid")
                            .and_then(|s| s.trim().parse().ok())
                            .ok_or_else(|| {
                                TelemetryError::ParseError(format!("{label}: missing pid"))
                            })?;
                        let vram_mib = pf
                            .get("used_memory")
                            .and_then(|s| parse_mib(s))
                            .unwrap_or(0);
                        proc_list.push(ProcessSample {
                            pid,
                            name: pf
                                .get("process_name")
                                .map(|s| s.trim().to_string())
                                .unwrap_or_default(),
                            vram_mib,
                            // Stamped with the enclosing GPU's index in
                            // finalize_gpu, which is where the index
                            // (minor_number, else document order) is known.
                            gpu_index: 0,
                            backend_id: None,
                        });
                    }
                } else if name == "gpu" {
                    let fields = gpu_fields.take().ok_or_else(|| {
                        TelemetryError::ParseError("unbalanced <gpu> element".to_string())
                    })?;
                    gpus.push(finalize_gpu(order, &gpu_id_attr, fields, proc_list)?);
                    proc_list = Vec::new();
                    order += 1;
                }
                stack.pop();
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(TelemetryError::ParseError(format!("malformed XML: {e}")));
            }
            _ => {}
        }
        buf.clear();
    }

    if gpus.is_empty() {
        return Err(TelemetryError::ParseError(
            "no <gpu> elements found in nvidia-smi output".to_string(),
        ));
    }
    Ok(ParsedDoc {
        driver_version,
        gpus,
    })
}

fn name_of(name: quick_xml::name::QName<'_>) -> Result<String, TelemetryError> {
    std::str::from_utf8(name.as_ref())
        .map(|s| s.to_string())
        .map_err(|e| TelemetryError::ParseError(format!("bad element name: {e}")))
}

fn attr_of(e: &quick_xml::events::BytesStart<'_>, key: &str) -> Option<String> {
    e.attributes().find_map(|a| {
        a.ok().and_then(|attr| {
            if attr.key.as_ref() == key.as_bytes() {
                std::str::from_utf8(&attr.value).ok().map(|s| s.to_string())
            } else {
                None
            }
        })
    })
}

fn finalize_gpu(
    order: u32,
    id_attr: &str,
    f: HashMap<String, String>,
    processes: Vec<ProcessSample>,
) -> Result<ParsedGpu, TelemetryError> {
    let get = |k: &str| {
        f.get(k)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let name = get("product_name").unwrap_or_else(|| format!("gpu-{order}"));
    let uuid = get("uuid").unwrap_or_default();

    // Index: <minor_number> when present, else document order.
    let index = get("minor_number")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(order);

    // Processes were parsed out of this <gpu> element; stamp each with the
    // index just resolved for it — the same index the GpuSample carries.
    let processes = processes
        .into_iter()
        .map(|mut p| {
            p.gpu_index = index;
            p
        })
        .collect();

    let mem_total_mib = required(
        "fb_memory_usage/total",
        index,
        &name,
        get("fb_memory_usage/total").and_then(|s| parse_mib(&s)),
    )?;
    let mem_used_mib = required(
        "fb_memory_usage/used",
        index,
        &name,
        get("fb_memory_usage/used").and_then(|s| parse_mib(&s)),
    )?;
    let gpu_util_pct = required(
        "utilization/gpu_util",
        index,
        &name,
        get("utilization/gpu_util").and_then(|s| parse_percent(&s)),
    )?;
    let temp_c = required(
        "temperature/gpu_temp",
        index,
        &name,
        get("temperature/gpu_temp").and_then(|s| parse_celsius(&s)),
    )?;
    // Optional: N/A or missing on cards whose power sensor the driver
    // does not expose in XML (see module docs).
    let power_w = get("gpu_power_readings/power_draw").and_then(|s| parse_watts(&s));

    let pci_bus_id = get("pci/pci_bus").unwrap_or_else(|| id_attr.to_string());

    Ok(ParsedGpu {
        index,
        name,
        uuid,
        pci_bus_id,
        mem_total_mib,
        mem_used_mib,
        gpu_util_pct,
        mem_util_pct: get("utilization/memory_util").and_then(|s| parse_percent(&s)),
        temp_c,
        power_w,
        power_limit_w: get("gpu_power_readings/power_limit").and_then(|s| parse_watts(&s)),
        fan_pct: get("fan_speed").and_then(|s| parse_percent(&s)),
        clocks_graphics_mhz: get("clocks/graphics_clock").and_then(|s| parse_mhz(&s)),
        clocks_mem_mhz: get("clocks/mem_clock").and_then(|s| parse_mhz(&s)),
        processes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/nvidia-smi-a4000-2xp100.xml");

    #[test]
    fn parses_fixture_three_gpus() {
        let samples = NvidiaSmiSource::parse_xml(FIXTURE).expect("fixture must parse");
        assert_eq!(samples.len(), 3);

        let a4000 = &samples[0];
        assert_eq!(a4000.index, 0);
        assert_eq!(a4000.name, "NVIDIA RTX A4000");
        assert!((a4000.utilization_pct - 62.0).abs() < f32::EPSILON);
        assert_eq!(a4000.vram_used_mib, 9216);
        assert_eq!(a4000.vram_total_mib, 16384);
        assert!((a4000.temp_c - 67.0).abs() < f32::EPSILON);
        assert!((a4000.power_w.expect("power reported") - 95.30).abs() < 0.01);
        assert_eq!(a4000.fan_pct, Some(32.0));
        assert_eq!(a4000.power_limit_w, Some(140.0));
        assert_eq!(a4000.clocks_graphics_mhz, Some(1560));
        assert_eq!(a4000.clocks_mem_mhz, Some(7000));
        assert_eq!(a4000.mem_util_pct, Some(48.0));
        assert!(!a4000.stale);
        assert_eq!(a4000.source, TelemetrySourceKind::NvidiaSmi);

        let p100_0 = &samples[1];
        assert_eq!(p100_0.index, 1);
        assert_eq!(p100_0.name, "Tesla P100-PCIE-16GB");
        assert!((p100_0.utilization_pct - 45.0).abs() < f32::EPSILON);
        assert_eq!(p100_0.vram_used_mib, 12288);
        assert!((p100_0.temp_c - 61.0).abs() < f32::EPSILON);
        assert!((p100_0.power_w.expect("power reported") - 118.75).abs() < 0.01);
        assert_eq!(
            p100_0.fan_pct, None,
            "P100 has no fan: N/A must map to None"
        );
        assert_eq!(p100_0.power_limit_w, Some(250.0));

        let p100_1 = &samples[2];
        assert_eq!(p100_1.index, 2);
        assert!((p100_1.utilization_pct - 12.0).abs() < f32::EPSILON);
        assert_eq!(p100_1.vram_used_mib, 2048);
        assert_eq!(p100_1.fan_pct, None);
    }

    #[test]
    fn parses_fixture_processes() {
        let procs = NvidiaSmiSource::parse_processes(FIXTURE).expect("fixture must parse");
        assert_eq!(procs.len(), 3, "2 on A4000 + 1 on P100-0 + 0 on P100-1");
        assert_eq!(procs[0].pid, 4321);
        assert_eq!(procs[0].vram_mib, 6144);
        assert!(procs[0].name.contains("llama-server"));
        assert_eq!(procs[1].pid, 9876);
        assert_eq!(procs[1].vram_mib, 2048);
        assert_eq!(procs[2].pid, 4321);
        assert_eq!(procs[2].vram_mib, 11264);
        // Each process carries the index of its enclosing <gpu> — the same
        // index that GPU's sample carries. pid 4321 appears on both GPU 0
        // and GPU 1 (a tensor split); the two rows must not be conflated.
        assert_eq!(procs[0].gpu_index, 0, "first process lives under GPU 0");
        assert_eq!(procs[1].gpu_index, 0, "second process lives under GPU 0");
        assert_eq!(procs[2].gpu_index, 1, "third process lives under GPU 1");
    }

    #[test]
    fn default_path_is_bare_name_off_windows() {
        // The System32 hardening is Windows-only; other builds keep the
        // bare program name.
        #[cfg(not(windows))]
        assert_eq!(
            NvidiaSmiSource::default().binary_path(),
            Path::new("nvidia-smi")
        );
    }

    #[test]
    fn parses_fixture_inventory() {
        let infos = NvidiaSmiSource::parse_inventory(FIXTURE).expect("fixture must parse");
        assert_eq!(infos.len(), 3);
        assert_eq!(infos[0].pci_bus_id, "00000000:01:00.0");
        assert_eq!(infos[0].driver_version, "581.57");
        assert_eq!(infos[0].compute_capability, (8, 6));
        assert_eq!(infos[0].total_vram_mib, 16384);
        assert_eq!(infos[1].pci_bus_id, "00000000:65:00.0");
        assert_eq!(infos[1].compute_capability, (6, 0));
        assert_eq!(infos[1].uuid, "GPU-22222222-3333-4444-5555-666666666666");
        assert_eq!(infos[2].compute_capability, (6, 0));
        assert!(infos
            .iter()
            .all(|i| i.source == TelemetrySourceKind::NvidiaSmi));
    }

    #[test]
    fn na_fields_map_to_none_without_error() {
        let xml = r#"<?xml version="1.0" ?><nvidia_smi_log>
<driver_version>581.57</driver_version><attached_gpus>1</attached_gpus>
<gpu id="00000000:65:00.0">
<product_name>Tesla P100-PCIE-16GB</product_name>
<uuid>GPU-aaaa</uuid><minor_number>0</minor_number>
<pci><pci_bus>00000000:65:00.0</pci_bus></pci>
<fan_speed>N/A</fan_speed>
<fb_memory_usage><total>16384 MiB</total><used>0 MiB</used></fb_memory_usage>
<utilization><gpu_util>0 %</gpu_util></utilization>
<temperature><gpu_temp>34 C</gpu_temp></temperature>
<gpu_power_readings><power_draw>38.20 W</power_draw><power_limit>N/A</power_limit></gpu_power_readings>
<processes></processes>
</gpu></nvidia_smi_log>"#;
        let samples = NvidiaSmiSource::parse_xml(xml).expect("N/A fields must parse");
        assert_eq!(samples.len(), 1);
        let s = &samples[0];
        assert_eq!(s.fan_pct, None);
        assert_eq!(s.power_limit_w, None);
        assert_eq!(
            s.clocks_graphics_mhz, None,
            "missing <clocks> must not error"
        );
        assert_eq!(s.mem_util_pct, None);
        assert!((s.power_w.expect("power reported") - 38.20).abs() < 0.01);
    }

    #[test]
    fn na_power_draw_maps_to_none_without_error() {
        // An RTX A4000 under current drivers reports N/A here while
        // every other field is valid; the sample must survive with
        // power_w = None (real hardware, 2026-10-15 — requiring the
        // field failed the entire telemetry source).
        let xml = r#"<?xml version="1.0" ?><nvidia_smi_log>
<driver_version>582.78</driver_version><attached_gpus>1</attached_gpus>
<gpu id="00000000:65:00.0">
<product_name>NVIDIA RTX A4000</product_name>
<uuid>GPU-bbbb</uuid><minor_number>0</minor_number>
<pci><pci_bus>00000000:65:00.0</pci_bus></pci>
<fan_speed>30 %</fan_speed>
<fb_memory_usage><total>16384 MiB</total><used>1024 MiB</used></fb_memory_usage>
<utilization><gpu_util>12 %</gpu_util></utilization>
<temperature><gpu_temp>41 C</gpu_temp></temperature>
<gpu_power_readings><power_draw>N/A</power_draw><power_limit>140.00 W</power_limit></gpu_power_readings>
<processes></processes>
</gpu></nvidia_smi_log>"#;
        let samples = NvidiaSmiSource::parse_xml(xml).expect("N/A power_draw must parse");
        assert_eq!(samples.len(), 1);
        let s = &samples[0];
        assert_eq!(s.power_w, None);
        assert_eq!(s.power_limit_w, Some(140.0));
        assert_eq!(s.vram_total_mib, 16384);
        assert!((s.temp_c - 41.0).abs() < f32::EPSILON);
    }

    #[test]
    fn missing_required_field_is_parse_error() {
        let xml = r#"<?xml version="1.0" ?><nvidia_smi_log>
<gpu id="00000000:01:00.0">
<product_name>NVIDIA RTX A4000</product_name>
<uuid>GPU-aaaa</uuid>
<utilization><gpu_util>10 %</gpu_util></utilization>
<temperature><gpu_temp>40 C</gpu_temp></temperature>
<gpu_power_readings><power_draw>50.00 W</power_draw></gpu_power_readings>
</gpu></nvidia_smi_log>"#;
        let err = NvidiaSmiSource::parse_xml(xml).expect_err("missing fb_memory_usage must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("fb_memory_usage/total"),
            "error must name the field: {msg}"
        );
        assert!(
            msg.contains("NVIDIA RTX A4000"),
            "error must name the GPU: {msg}"
        );
    }

    #[test]
    fn required_field_na_is_parse_error_not_zero() {
        let xml = r#"<?xml version="1.0" ?><nvidia_smi_log>
<gpu id="00000000:01:00.0">
<product_name>NVIDIA RTX A4000</product_name>
<uuid>GPU-aaaa</uuid>
<fb_memory_usage><total>16384 MiB</total><used>100 MiB</used></fb_memory_usage>
<utilization><gpu_util>N/A</gpu_util></utilization>
<temperature><gpu_temp>40 C</gpu_temp></temperature>
<gpu_power_readings><power_draw>50.00 W</power_draw></gpu_power_readings>
</gpu></nvidia_smi_log>"#;
        let err = NvidiaSmiSource::parse_xml(xml).expect_err("N/A gpu_util must not become 0");
        assert!(err.to_string().contains("gpu_util"));
    }

    #[test]
    fn malformed_xml_is_parse_error() {
        let err = NvidiaSmiSource::parse_xml("<nvidia_smi_log><gpu>")
            .expect_err("truncated XML must fail");
        assert!(matches!(err, TelemetryError::ParseError(_)));
    }

    #[test]
    fn empty_gpu_list_is_parse_error() {
        let err = NvidiaSmiSource::parse_xml(
            r#"<?xml version="1.0" ?><nvidia_smi_log><attached_gpus>0</attached_gpus></nvidia_smi_log>"#,
        )
        .expect_err("no GPUs must fail");
        assert!(err.to_string().contains("no <gpu> elements"));
    }

    #[test]
    fn value_parsers() {
        assert_eq!(parse_percent("62 %"), Some(62.0));
        assert_eq!(parse_percent("N/A"), None);
        assert_eq!(parse_percent(""), None);
        assert_eq!(parse_mib("16384 MiB"), Some(16384));
        assert_eq!(parse_mib("N/A"), None);
        assert_eq!(parse_celsius("67 C"), Some(67.0));
        assert_eq!(parse_watts("95.30 W"), Some(95.30));
        assert_eq!(parse_watts("N/A"), None);
        assert_eq!(parse_mhz("1560 MHz"), Some(1560));
        assert_eq!(parse_mhz("N/A"), None);
        assert_eq!(parse_percent("bogus"), None);
    }

    #[test]
    fn compute_capability_mapping() {
        assert_eq!(compute_capability_for("NVIDIA RTX A4000"), (8, 6));
        assert_eq!(compute_capability_for("Tesla P100-PCIE-16GB"), (6, 0));
        assert_eq!(compute_capability_for("Tesla V100-SXM2-32GB"), (7, 0));
        assert_eq!(compute_capability_for("Some Future Card X9000"), (0, 0));
    }

    #[test]
    fn index_falls_back_to_document_order() {
        let xml = r#"<?xml version="1.0" ?><nvidia_smi_log>
<gpu id="00000000:01:00.0">
<product_name>Card A</product_name><uuid>GPU-a</uuid>
<fb_memory_usage><total>8192 MiB</total><used>0 MiB</used></fb_memory_usage>
<utilization><gpu_util>0 %</gpu_util></utilization>
<temperature><gpu_temp>30 C</gpu_temp></temperature>
<gpu_power_readings><power_draw>20.00 W</power_draw></gpu_power_readings>
</gpu>
<gpu id="00000000:02:00.0">
<product_name>Card B</product_name><uuid>GPU-b</uuid>
<fb_memory_usage><total>8192 MiB</total><used>0 MiB</used></fb_memory_usage>
<utilization><gpu_util>0 %</gpu_util></utilization>
<temperature><gpu_temp>30 C</gpu_temp></temperature>
<gpu_power_readings><power_draw>20.00 W</power_draw></gpu_power_readings>
</gpu></nvidia_smi_log>"#;
        let samples = NvidiaSmiSource::parse_xml(xml).expect("must parse");
        assert_eq!(samples[0].index, 0);
        assert_eq!(samples[1].index, 1);
        // pci falls back to the id attribute when <pci><pci_bus> is absent
        let infos = NvidiaSmiSource::parse_inventory(xml).expect("must parse");
        assert_eq!(infos[0].pci_bus_id, "00000000:01:00.0");
    }

    #[test]
    fn process_gpu_index_follows_minor_number() {
        // minor_number (5) disagrees with document order (0): the process
        // must be stamped with the same index the GpuSample gets.
        let xml = r#"<?xml version="1.0" ?><nvidia_smi_log>
<gpu id="00000000:01:00.0">
<product_name>Card A</product_name><uuid>GPU-a</uuid><minor_number>5</minor_number>
<fb_memory_usage><total>8192 MiB</total><used>0 MiB</used></fb_memory_usage>
<utilization><gpu_util>0 %</gpu_util></utilization>
<temperature><gpu_temp>30 C</gpu_temp></temperature>
<gpu_power_readings><power_draw>20.00 W</power_draw></gpu_power_readings>
<processes><process_info><pid>777</pid><process_name>llama-server</process_name><used_memory>1024 MiB</used_memory></process_info></processes>
</gpu></nvidia_smi_log>"#;
        let (samples, procs) = NvidiaSmiSource::parse_xml_with_processes(xml).expect("must parse");
        assert_eq!(samples[0].index, 5);
        assert_eq!(procs.len(), 1);
        assert_eq!(procs[0].gpu_index, samples[0].index);
    }

    #[tokio::test]
    async fn run_query_reports_missing_binary() {
        let src = NvidiaSmiSource::new("/nonexistent/nvidia-smi-xyz");
        let err = src.run_query().await.expect_err("missing binary must fail");
        assert!(matches!(err, TelemetryError::SmiFailed(_)));
        assert!(err.to_string().contains("failed to spawn"));
    }
}
