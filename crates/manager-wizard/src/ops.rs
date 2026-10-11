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
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Fail fast when the server cannot be reached at all.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Health probes must answer quickly; the retry loop handles slowness.
const HTTP_GET_TIMEOUT: Duration = Duration::from_secs(15);
/// Above `SMOKE_MAX_LATENCY_MS` (120 s) so the smoke step — not the client —
/// decides what "too slow" means.
const HTTP_POST_TIMEOUT: Duration = Duration::from_secs(180);

/// Production system effects.
pub struct RealSystemOps;

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

#[async_trait]
impl SystemOps for RealSystemOps {
    async fn run_command(&self, cmd: &PlannedCommand) -> Result<CommandOutput, WizardError> {
        let (prog, args) = cmd
            .argv
            .split_first()
            .ok_or_else(|| WizardError::CommandFailed {
                argv: cmd.argv.clone(),
                code: -1,
                stderr: "empty argv".to_string(),
            })?;
        let mut c = tokio::process::Command::new(prog);
        c.args(args);
        if let Some(cwd) = &cmd.cwd {
            c.current_dir(cwd);
        }
        for (k, v) in &cmd.env {
            c.env(k, v);
        }
        let out = c.output().await.map_err(|e| WizardError::CommandFailed {
            argv: cmd.argv.clone(),
            code: -1,
            stderr: format!("spawn failed: {e}"),
        })?;
        Ok(CommandOutput {
            exit_code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    async fn spawn_detached(&self, cmd: &PlannedCommand) -> Result<u32, WizardError> {
        let (prog, args) = cmd
            .argv
            .split_first()
            .ok_or_else(|| WizardError::CommandFailed {
                argv: cmd.argv.clone(),
                code: -1,
                stderr: "empty argv".to_string(),
            })?;
        let mut c = tokio::process::Command::new(prog);
        c.args(args);
        // Detach from this process group so the child outlives the wizard.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW | DETACHED_PROCESS
            c.creation_flags(0x08000000 | 0x00000008);
        }
        #[cfg(not(windows))]
        {
            // New session: the child is not in our process group.
            unsafe {
                c.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }
        c.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(false);
        let child = c.spawn().map_err(|e| WizardError::CommandFailed {
            argv: cmd.argv.clone(),
            code: -1,
            stderr: format!("spawn failed: {e}"),
        })?;
        Ok(child.id().unwrap_or(0))
    }

    async fn download(
        &self,
        url: &str,
        dest: &Path,
        on_progress: &(dyn Fn(u64, u64) + Sync),
    ) -> Result<u64, WizardError> {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| WizardError::DownloadFailed {
                url: url.to_string(),
                reason: format!("client build failed: {e}"),
            })?;
        let mut resp = client
            .get(url)
            .send()
            .await
            .map_err(|e| WizardError::DownloadFailed {
                url: url.to_string(),
                reason: e.to_string(),
            })?;
        if !resp.status().is_success() {
            return Err(WizardError::DownloadFailed {
                url: url.to_string(),
                reason: format!("HTTP {}", resp.status()),
            });
        }
        let total = resp.content_length().unwrap_or(0);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| Self::io_err(dest, e))?;
        }
        let mut file = tokio::fs::File::create(dest)
            .await
            .map_err(|e| Self::io_err(dest, e))?;
        let mut downloaded: u64 = 0;
        loop {
            let chunk = resp
                .chunk()
                .await
                .map_err(|e| WizardError::DownloadFailed {
                    url: url.to_string(),
                    reason: e.to_string(),
                })?;
            let Some(bytes) = chunk else { break };
            tokio::io::AsyncWriteExt::write_all(&mut file, &bytes)
                .await
                .map_err(|e| Self::io_err(dest, e))?;
            downloaded += bytes.len() as u64;
            on_progress(downloaded, total);
        }
        Ok(downloaded)
    }

    async fn extract_zip(&self, archive: &Path, dest: &Path) -> Result<(), WizardError> {
        let archive_c = archive.to_path_buf();
        let dest_c = dest.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(&archive_c).map_err(|e| Self::io_err(&archive_c, e))?;
            let mut zip = zip::ZipArchive::new(file).map_err(|e| Self::io_err(&archive_c, e))?;
            zip.extract(&dest_c).map_err(|e| Self::io_err(&dest_c, e))
        })
        .await
        .map_err(|e| WizardError::IoError {
            path: archive.to_path_buf(),
            reason: format!("extraction task failed: {e}"),
        })?
    }

    async fn http_get(&self, url: &str) -> Result<HttpResponse, WizardError> {
        let start = std::time::Instant::now();
        let client = reqwest::Client::builder()
            .timeout(HTTP_GET_TIMEOUT)
            .build()
            .map_err(|e| WizardError::HttpError {
                url: url.to_string(),
                reason: format!("client build failed: {e}"),
            })?;
        let resp = client
            .get(url)
            .send()
            .await
            .map_err(|e| WizardError::HttpError {
                url: url.to_string(),
                reason: e.to_string(),
            })?;
        let status = resp.status().as_u16();
        let body = resp.bytes().await.map_err(|e| WizardError::HttpError {
            url: url.to_string(),
            reason: e.to_string(),
        })?;
        Ok(HttpResponse {
            status,
            body: body.to_vec(),
            elapsed_ms: start.elapsed().as_millis() as u64,
        })
    }

    async fn http_post(&self, url: &str, body: &str) -> Result<HttpResponse, WizardError> {
        let start = std::time::Instant::now();
        let client = reqwest::Client::builder()
            .timeout(HTTP_POST_TIMEOUT)
            .build()
            .map_err(|e| WizardError::HttpError {
                url: url.to_string(),
                reason: format!("client build failed: {e}"),
            })?;
        let resp = client
            .post(url)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| WizardError::HttpError {
                url: url.to_string(),
                reason: e.to_string(),
            })?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.map_err(|e| WizardError::HttpError {
            url: url.to_string(),
            reason: e.to_string(),
        })?;
        Ok(HttpResponse {
            status,
            body: bytes.to_vec(),
            elapsed_ms: start.elapsed().as_millis() as u64,
        })
    }

    async fn http_head_content_length(&self, url: &str) -> Result<Option<u64>, WizardError> {
        let client = reqwest::Client::builder()
            .timeout(HTTP_GET_TIMEOUT)
            .build()
            .map_err(|e| WizardError::HttpError {
                url: url.to_string(),
                reason: format!("client build failed: {e}"),
            })?;
        let resp = client
            .head(url)
            .send()
            .await
            .map_err(|e| WizardError::HttpError {
                url: url.to_string(),
                reason: e.to_string(),
            })?;
        Ok(resp.content_length())
    }

    async fn free_space_bytes(&self, path: &Path) -> Result<u64, WizardError> {
        // Shell out rather than linking winapi: the wizard already spawns
        // subprocesses for every other probe, and this keeps the dep tree
        // unchanged.
        #[cfg(target_os = "windows")]
        let probe = PlannedCommand::new(vec![
            "powershell".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            format!(
                "(Get-Item '{}').PSDrive.Free",
                path.display().to_string().replace('\'', "''")
            ),
        ]);
        #[cfg(not(target_os = "windows"))]
        let probe = PlannedCommand::new(vec![
            "df".to_string(),
            "-k".to_string(),
            "--output=avail".to_string(),
            path.display().to_string(),
        ]);
        let out = self.run_command(&probe).await?;
        if !out.success() {
            return Err(WizardError::CommandFailed {
                argv: probe.argv.clone(),
                code: out.exit_code,
                stderr: out.stderr.clone(),
            });
        }
        #[cfg(target_os = "windows")]
        let bytes = out
            .stdout
            .trim()
            .parse::<u64>()
            .map_err(|e| WizardError::CommandFailed {
                argv: probe.argv.clone(),
                code: out.exit_code,
                stderr: format!("could not parse free-space output {:?}: {e}", out.stdout),
            })?;
        #[cfg(not(target_os = "windows"))]
        let bytes = out
            .stdout
            .lines()
            .last()
            .and_then(|l| l.trim().parse::<u64>().ok())
            .map(|kb| kb * 1024)
            .ok_or_else(|| WizardError::CommandFailed {
                argv: probe.argv.clone(),
                code: out.exit_code,
                stderr: format!("could not parse df output {:?}", out.stdout),
            })?;
        Ok(bytes)
    }

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>, WizardError> {
        tokio::fs::read(path)
            .await
            .map_err(|e| Self::io_err(path, e))
    }

    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), WizardError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| Self::io_err(path, e))?;
            }
        }
        tokio::fs::write(path, data)
            .await
            .map_err(|e| Self::io_err(path, e))
    }

    async fn file_exists(&self, path: &Path) -> bool {
        tokio::fs::metadata(path).await.is_ok()
    }

    async fn sha256_file(&self, path: &Path) -> Result<String, WizardError> {
        let data = self.read_file(path).await?;
        let mut hasher = Sha256::new();
        hasher.update(&data);
        Ok(format!("{:x}", hasher.finalize()))
    }

    async fn gpu_inventory(&self) -> Result<Vec<GpuDescriptor>, WizardError> {
        let rows = PlannedCommand::new(vec![
            "nvidia-smi".to_string(),
            "--query-gpu=index,name,uuid,memory.total,compute_cap".to_string(),
            "--format=csv,noheader".to_string(),
        ]);
        let out = self
            .run_command(&rows)
            .await
            .map_err(|e| WizardError::GpuProbeFailed(format!("nvidia-smi query failed: {e}")))?;
        if !out.success() {
            return Err(WizardError::GpuProbeFailed(format!(
                "nvidia-smi exited {}: {}",
                out.exit_code, out.stderr
            )));
        }
        let header = self
            .run_command(&PlannedCommand::new(vec!["nvidia-smi".to_string()]))
            .await
            .map_err(|e| WizardError::GpuProbeFailed(format!("nvidia-smi failed: {e}")))?;
        let driver = Self::parse_driver_version(&header.stdout);
        Ok(Self::parse_gpu_rows(&out.stdout, &driver))
    }

    async fn credential_exists(&self, target: &str) -> Result<bool, WizardError> {
        #[cfg(windows)]
        {
            let out = tokio::process::Command::new("cmdkey")
                .arg(format!("/list:{target}"))
                .output()
                .await
                .map_err(|e| WizardError::IoError {
                    path: PathBuf::from("cmdkey"),
                    reason: e.to_string(),
                })?;
            Ok(out.status.success() && String::from_utf8_lossy(&out.stdout).contains(target))
        }
        #[cfg(not(windows))]
        {
            let _ = target;
            // No Credential Manager off Windows; presence checks fail closed.
            Ok(false)
        }
    }

    fn home_dir(&self) -> Option<PathBuf> {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gpu_rows_handles_realistic_output() {
        let text = "0, NVIDIA RTX A4000, GPU-aaaa, 16384 MiB, 8.6\n\
                    1, Tesla P100-PCIE-16GB, GPU-bbbb, 16384 MiB, 6.0\n";
        let gpus = RealSystemOps::parse_gpu_rows(text, "581.57");
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].name, "NVIDIA RTX A4000");
        assert_eq!(gpus[0].compute_capability, (8, 6));
        assert!(!gpus[0].is_pascal_or_older());
        assert_eq!(gpus[1].compute_capability, (6, 0));
        assert!(gpus[1].is_pascal_or_older());
        assert_eq!(gpus[1].total_vram_mib, 16384);
        assert_eq!(gpus[1].driver_version, "581.57");
    }

    #[test]
    fn parse_driver_version_from_header() {
        let header = "| NVIDIA-SMI 581.57                 Driver Version: 581.57         CUDA Version: 12.9     |";
        assert_eq!(RealSystemOps::parse_driver_version(header), "581.57");
        assert_eq!(RealSystemOps::parse_driver_version("nope"), "0.0");
    }

    #[tokio::test]
    async fn run_command_empty_argv_errors() {
        let ops = RealSystemOps;
        let err = ops
            .run_command(&PlannedCommand::new(vec![]))
            .await
            .unwrap_err();
        assert!(matches!(err, WizardError::CommandFailed { .. }));
    }

    #[tokio::test]
    async fn sha256_roundtrip_on_temp_file() {
        let ops = RealSystemOps;
        let path = std::env::temp_dir().join("wizard-sha256-test.bin");
        ops.write_file(&path, b"hello").await.unwrap();
        let hex = ops.sha256_file(&path).await.unwrap();
        assert_eq!(
            hex,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert!(ops.file_exists(&path).await);
        std::fs::remove_file(&path).ok();
    }
}
