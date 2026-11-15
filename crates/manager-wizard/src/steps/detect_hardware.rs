//! Step 1: detect hardware and drivers.

use super::*;

// ---------------------------------------------------------------------------
// 1. Detect hardware and drivers
// ---------------------------------------------------------------------------

/// Step 1: GPU inventory + driver check (§3.3 verification matrix).
pub struct DetectHardware;

#[async_trait]
impl WizardStep for DetectHardware {
    fn id(&self) -> &str {
        STEP_DETECT_HARDWARE
    }
    fn name(&self) -> &str {
        "Detect hardware and drivers"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, _ctx: &WizardContext) -> Vec<PlannedCommand> {
        // Exactly what RealSystemOps::gpu_inventory runs, in order: the
        // per-GPU query, then the plain nvidia-smi header for the driver
        // version. The program is manager-telemetry's System32-first
        // resolution (same as the real probe), not a bare PATH lookup.
        let smi = manager_telemetry::default_smi_path()
            .to_string_lossy()
            .into_owned();
        vec![
            PlannedCommand::new(vec![
                smi.clone(),
                "--query-gpu=index,name,uuid,memory.total,compute_cap".to_string(),
                "--format=csv,noheader".to_string(),
            ]),
            PlannedCommand::new(vec![smi]),
        ]
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("install a current NVIDIA driver (R550+, i.e. 551.61+) and re-run the hardware check — download: https://www.nvidia.com/Download/index.aspx")
    }
    fn help_url(&self) -> Option<&str> {
        Some(crate::NVIDIA_DRIVER_DOWNLOAD_URL)
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let gpus = match ctx.ops.gpu_inventory().await {
            Ok(g) => g,
            Err(e) => {
                return Ok(StepOutcome::failed(
                    format!("GPU probe failed: {e}"),
                    vec![format!("{e:?}")],
                ))
            }
        };
        if gpus.is_empty() {
            return Ok(StepOutcome::failed(
                "no NVIDIA GPUs detected",
                vec!["GPU inventory probe returned zero devices".to_string()],
            ));
        }
        let mut log = Vec::new();
        for g in &gpus {
            log.push(format!(
                "gpu{}: {} ({}) — {} VRAM, sm_{}{}, driver {}",
                g.index,
                g.name,
                g.uuid,
                manager_present::mib(g.total_vram_mib),
                g.compute_capability.0,
                g.compute_capability.1,
                g.driver_version
            ));
        }
        let driver = &gpus[0].driver_version;
        let major: u32 = driver.split('.').next().unwrap_or("0").parse().unwrap_or(0);
        if major < crate::MIN_DRIVER_MAJOR {
            return Ok(StepOutcome::failed(
                format!(
                    "driver {driver} too old: need R{}+ for the pinned CUDA 12.x runtime (spec §3.4)",
                    crate::MIN_DRIVER_MAJOR
                ),
                log,
            ));
        }
        // PSEC/ingress probe result is recorded by the MXC step; here we just
        // note the driver/CUDA line for the report.
        Ok(StepOutcome::done(
            format!(
                "{} GPU(s) detected, driver {driver} (CUDA 12.x runtime compatible)",
                gpus.len()
            ),
            log,
        ))
    }
}
