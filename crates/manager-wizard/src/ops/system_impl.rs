//! The `SystemOps` implementation for [`RealSystemOps`].

use super::*;

#[async_trait]
impl SystemOps for RealSystemOps {
    fn note_step_started(&self) {
        self.step_cancel.store(false, Ordering::SeqCst);
    }

    fn request_cancellation(&self) {
        self.step_cancel.store(true, Ordering::SeqCst);
    }

    async fn run_command(&self, cmd: &PlannedCommand) -> Result<CommandOutput, WizardError> {
        self.run_child(cmd, COMMAND_TIMEOUT).await
    }

    async fn run_command_with_timeout(
        &self,
        cmd: &PlannedCommand,
        timeout: Duration,
    ) -> Result<CommandOutput, WizardError> {
        // The caller's budget is honored exactly — no inner default
        // cap underneath it (code-review 2.1).
        self.run_child(cmd, timeout).await
    }

    async fn service_start_gateway(
        &self,
        def: &manager_services::ServiceDefinition,
    ) -> Result<u32, WizardError> {
        #[cfg(windows)]
        {
            use manager_services::ServiceControl;
            let control = manager_services::scm::platform_control();
            let name = def.name.clone();
            let pid_file = def.pid_file.clone();
            // SCM calls are synchronous; keep them off the async runtime.
            tokio::task::spawn_blocking(move || {
                if !control.is_registered(&name).map_err(|e| e.to_string())? {
                    return Err(format!(
                        "the gateway service '{name}' is not registered — register it in Settings → Supervision before starting it in service mode"
                    ));
                }
                // The shared start-and-wait: a start that falls back
                // to Stopped reports the shim's own reason (credential
                // resolution, spawn failure, the child's exit code)
                // instead of a pid-file timeout.
                manager_services::start_and_wait(
                    &control,
                    &name,
                    manager_services::ServiceTimeouts::start(&name),
                )
                .map_err(|e| e.to_string())?;
                if let Some(pid_file) = &pid_file {
                    // The service host writes the child's pid once it
                    // is spawned; wait for a live pid to appear.
                    for _ in 0..150 {
                        if let Ok(text) = std::fs::read_to_string(pid_file) {
                            if let Ok(pid) = text.trim().parse::<u32>() {
                                if process_alive(pid) {
                                    return Ok(pid);
                                }
                            }
                        }
                        // A child that died right after the service
                        // reached Running takes the service down with
                        // it: report the shim's reason, not a timeout.
                        if let Ok(status) = control.query(&name) {
                            if status.state == manager_services::ServiceState::Stopped {
                                return Err(format!(
                                    "gateway service '{name}' stopped right after starting: {}",
                                    status
                                        .failure_reason()
                                        .unwrap_or_else(|| "no exit code recorded".to_string())
                                ));
                            }
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    Err(format!(
                        "gateway service '{name}' started but its process did not appear (no live pid in {})",
                        pid_file.display()
                    ))
                } else {
                    control
                        .query(&name)
                        .map_err(|e| e.to_string())?
                        .pid
                        .ok_or_else(|| {
                            format!("gateway service '{name}' has no process id after start")
                        })
                }
            })
            .await
            .map_err(|e| WizardError::IoError {
                path: PathBuf::from("scm"),
                reason: format!("service start task failed: {e}"),
            })?
            .map_err(|reason| WizardError::IoError {
                path: PathBuf::from("scm"),
                reason,
            })
        }
        #[cfg(not(windows))]
        {
            Err(WizardError::IoError {
                path: PathBuf::from("scm"),
                reason: format!(
                    "service supervision requires Windows (service '{}')",
                    def.name
                ),
            })
        }
    }

    async fn service_stop(&self, name: &str) -> Result<manager_services::StopOutcome, WizardError> {
        #[cfg(windows)]
        {
            let control = manager_services::scm::platform_control();
            let name = name.to_string();
            tokio::task::spawn_blocking(move || {
                manager_services::stop_and_wait(
                    &control,
                    &name,
                    manager_services::ServiceTimeouts::stop(&name),
                )
                .map_err(|e| e.to_string())
            })
            .await
            .map_err(|e| WizardError::IoError {
                path: PathBuf::from("scm"),
                reason: format!("service stop task failed: {e}"),
            })?
            .map_err(|reason| WizardError::IoError {
                path: PathBuf::from("scm"),
                reason,
            })
        }
        #[cfg(not(windows))]
        {
            Err(WizardError::IoError {
                path: PathBuf::from("scm"),
                reason: format!("service supervision requires Windows (service '{name}')"),
            })
        }
    }

    async fn service_registered(&self, name: &str) -> bool {
        #[cfg(windows)]
        {
            use manager_services::ServiceControl;
            let name = name.to_string();
            tokio::task::spawn_blocking(move || {
                manager_services::scm::platform_control()
                    .is_registered(&name)
                    .unwrap_or(false)
            })
            .await
            .unwrap_or(false)
        }
        #[cfg(not(windows))]
        {
            let _ = name;
            false
        }
    }

    async fn service_state(&self, name: &str) -> Option<manager_services::ServiceState> {
        #[cfg(windows)]
        {
            use manager_services::ServiceControl;
            let name = name.to_string();
            tokio::task::spawn_blocking(move || {
                manager_services::scm::platform_control()
                    .query(&name)
                    .ok()
                    .map(|s| s.state)
            })
            .await
            .ok()
            .flatten()
        }
        #[cfg(not(windows))]
        {
            let _ = name;
            None
        }
    }

    fn port_owner_pid(&self, port: u16) -> Option<u32> {
        manager_supervisor::port_owner_pid(port)
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
        // The planned environment must reach the child: the gateway's
        // OpenRouter key travels exclusively this way (the generated
        // YAML references it as os.environ/OPENROUTER_API_KEY). Dropping
        // it here silently disarms the fallback in production.
        for (k, v) in &cmd.env {
            c.env(k, v);
        }
        // The child must outlive the wizard, but that is a property
        // of process lifetime, not of DETACHED_PROCESS: no job
        // object is used, so the child survives the parent's exit
        // either way, and the GUI app has no console from which a
        // console-control event could reach it. CREATE_NO_WINDOW
        // alone is what keeps the child — and, critically, the
        // child's own console children — windowless: it gives the
        // child a console that exists but has no window, which its
        // children inherit, while combining it with DETACHED_PROCESS
        // makes Windows ignore CREATE_NO_WINDOW entirely (and a
        // detached child has no console to hand down, so every
        // console grandchild gets a fresh visible window — the
        // Prisma query engine window reported 2026-11-02). The
        // service shim already uses CREATE_NO_WINDOW alone; this
        // aligns the process-mode spawn with it.
        #[cfg(windows)]
        {
            // CREATE_NO_WINDOW (tokio Command: creation_flags is
            // inherent on Windows, no std import).
            c.creation_flags(windowless_creation_flags());
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
        // Capture the child's output when the caller named log files:
        // a detached process with null stdio fails invisibly — when
        // the gateway dies at startup, its own error output is the
        // only place the cause exists. A failure to open the log is
        // reported, not silently degraded to discarding output.
        let stdout_stdio = match &cmd.stdout_log {
            Some(path) => std::process::Stdio::from(open_rotated_log(path).map_err(|e| {
                WizardError::CommandFailed {
                    argv: cmd.argv.clone(),
                    code: -1,
                    stderr: format!("could not open stdout log {}: {e}", path.display()),
                }
            })?),
            None => std::process::Stdio::null(),
        };
        let stderr_stdio = match &cmd.stderr_log {
            Some(path) => std::process::Stdio::from(open_rotated_log(path).map_err(|e| {
                WizardError::CommandFailed {
                    argv: cmd.argv.clone(),
                    code: -1,
                    stderr: format!("could not open stderr log {}: {e}", path.display()),
                }
            })?),
            None => std::process::Stdio::null(),
        };
        c.stdin(std::process::Stdio::null())
            .stdout(stdout_stdio)
            .stderr(stderr_stdio)
            .kill_on_drop(false);
        let child = c.spawn().map_err(|e| WizardError::CommandFailed {
            argv: cmd.argv.clone(),
            code: -1,
            stderr: format!("spawn failed: {e}"),
        })?;
        Ok(child.id().unwrap_or(0))
    }

    fn process_alive(&self, pid: u32) -> bool {
        process_alive(pid)
    }

    fn process_image_is(&self, pid: u32, image: &str) -> Option<bool> {
        process_image_is(pid, image)
    }

    fn process_is_gateway(&self, pid: u32) -> Option<bool> {
        process_is_gateway(pid)
    }

    fn kill_process(&self, pid: u32) -> std::io::Result<()> {
        kill_process(pid)
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
            // No-data timeout: the client has a connect timeout but a
            // connection that stalls mid-body would otherwise block
            // the step (and the wizard lock) forever with no output —
            // the install-postgres hang Pete hit on 2026-10-15. A
            // slow but progressing download never trips this.
            let chunk = match tokio::time::timeout(CHUNK_IDLE_TIMEOUT, resp.chunk()).await {
                Ok(r) => r.map_err(|e| WizardError::DownloadFailed {
                    url: url.to_string(),
                    reason: e.to_string(),
                })?,
                Err(_) => {
                    return Err(WizardError::DownloadFailed {
                        url: url.to_string(),
                        reason: format!(
                            "no data received for {} s; the connection appears stalled",
                            CHUNK_IDLE_TIMEOUT.as_secs()
                        ),
                    })
                }
            };
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
        let cancel = self.cancel_flag();
        tokio::task::spawn_blocking(move || {
            use std::io::Read as _;
            let file = std::fs::File::open(&archive_c).map_err(|e| Self::io_err(&archive_c, e))?;
            let mut zip = zip::ZipArchive::new(file).map_err(|e| Self::io_err(&archive_c, e))?;
            // Manual per-entry loop (not `zip.extract`) so a step
            // cancellation lands between entries: extraction runs on
            // a blocking thread the step's dropped future cannot
            // cancel, and a timed-out extraction otherwise kept
            // writing into the tree the retry was extracting into.
            // `enclosed_name` gives the same escape protection
            // `extract` applies internally.
            for i in 0..zip.len() {
                if cancel.load(Ordering::SeqCst) {
                    return Err(WizardError::Cancelled);
                }
                let mut entry = zip.by_index(i).map_err(|e| Self::io_err(&archive_c, e))?;
                let Some(rel_path) = entry.enclosed_name() else {
                    return Err(WizardError::IoError {
                        path: archive_c.clone(),
                        reason: format!("zip entry escapes the destination: {}", entry.name()),
                    });
                };
                let target = dest_c.join(&rel_path);
                if entry.is_dir() {
                    std::fs::create_dir_all(&target).map_err(|e| Self::io_err(&target, e))?;
                    continue;
                }
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| Self::io_err(parent, e))?;
                }
                let mut bytes = Vec::with_capacity(entry.size() as usize);
                entry
                    .read_to_end(&mut bytes)
                    .map_err(|e| Self::io_err(&archive_c, e))?;
                std::fs::write(&target, &bytes).map_err(|e| Self::io_err(&target, e))?;
                // `zip.extract` applied the entry's Unix mode; the
                // manual loop must too — the extracted uv binary is
                // not runnable without its executable bit.
                #[cfg(unix)]
                if let Some(mode) = entry.unix_mode() {
                    use std::os::unix::fs::PermissionsExt as _;
                    let _ =
                        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode));
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| WizardError::IoError {
            path: archive.to_path_buf(),
            reason: format!("extraction task failed: {e}"),
        })?
    }

    async fn extract_tar_gz(
        &self,
        archive: &Path,
        dest: &Path,
        prefix: &str,
        strip_prefix: &str,
    ) -> Result<(), WizardError> {
        let archive_c = archive.to_path_buf();
        let dest_c = dest.to_path_buf();
        let prefix_c = prefix.to_string();
        let strip_c = strip_prefix.to_string();
        let cancel = self.cancel_flag();
        tokio::task::spawn_blocking(move || {
            use std::io::Read as _;
            let file = std::fs::File::open(&archive_c).map_err(|e| Self::io_err(&archive_c, e))?;
            let gz = flate2::read::GzDecoder::new(file);
            let mut tar = tar::Archive::new(gz);
            let entries = tar.entries().map_err(|e| Self::io_err(&archive_c, e))?;
            for entry in entries {
                if cancel.load(Ordering::SeqCst) {
                    return Err(WizardError::Cancelled);
                }
                let mut entry = entry.map_err(|e| Self::io_err(&archive_c, e))?;
                let path = entry.path().map_err(|e| Self::io_err(&archive_c, e))?;
                let path_str = path.to_string_lossy().replace('\\', "/");
                if !path_str.starts_with(&prefix_c) {
                    continue;
                }
                let Some(rel) = path_str.strip_prefix(&strip_c) else {
                    continue;
                };
                if rel.is_empty() {
                    continue;
                }
                let rel_path = Path::new(rel);
                // Entries are written manually (not via unpack) so a
                // hostile or malformed archive cannot escape `dest`.
                if rel_path.components().any(|c| {
                    matches!(
                        c,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                }) {
                    return Err(WizardError::IoError {
                        path: archive_c.clone(),
                        reason: format!("tar entry escapes the destination: {path_str}"),
                    });
                }
                let target = dest_c.join(rel_path);
                if entry.header().entry_type().is_dir() {
                    std::fs::create_dir_all(&target).map_err(|e| Self::io_err(&target, e))?;
                    continue;
                }
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| Self::io_err(parent, e))?;
                }
                let mut bytes = Vec::new();
                entry
                    .read_to_end(&mut bytes)
                    .map_err(|e| Self::io_err(&archive_c, e))?;
                std::fs::write(&target, &bytes).map_err(|e| Self::io_err(&target, e))?;
            }
            Ok(())
        })
        .await
        .map_err(|e| WizardError::IoError {
            path: archive.to_path_buf(),
            reason: format!("extraction task failed: {e}"),
        })?
    }

    async fn extract_zip_prefixes(
        &self,
        archive: &Path,
        dest: &Path,
        prefixes: &[&str],
        strip_prefix: &str,
    ) -> Result<(), WizardError> {
        let archive_c = archive.to_path_buf();
        let dest_c = dest.to_path_buf();
        let prefixes_c: Vec<String> = prefixes.iter().map(|p| p.to_string()).collect();
        let strip_c = strip_prefix.to_string();
        let cancel = self.cancel_flag();
        tokio::task::spawn_blocking(move || {
            use std::io::Read as _;
            let file = std::fs::File::open(&archive_c).map_err(|e| Self::io_err(&archive_c, e))?;
            let mut zip = zip::ZipArchive::new(file).map_err(|e| Self::io_err(&archive_c, e))?;
            for i in 0..zip.len() {
                if cancel.load(Ordering::SeqCst) {
                    return Err(WizardError::Cancelled);
                }
                let mut entry = zip.by_index(i).map_err(|e| Self::io_err(&archive_c, e))?;
                let name = entry.name().replace('\\', "/");
                if !prefixes_c.iter().any(|p| name.starts_with(p.as_str())) {
                    continue;
                }
                let Some(rel) = name.strip_prefix(strip_c.as_str()) else {
                    continue;
                };
                if rel.is_empty() {
                    continue;
                }
                let rel_path = Path::new(rel);
                // Entries are written manually (not via extract) so
                // a hostile or malformed archive cannot escape `dest`.
                if rel_path.components().any(|c| {
                    matches!(
                        c,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                }) {
                    return Err(WizardError::IoError {
                        path: archive_c.clone(),
                        reason: format!("zip entry escapes the destination: {name}"),
                    });
                }
                let target = dest_c.join(rel_path);
                if entry.is_dir() {
                    std::fs::create_dir_all(&target).map_err(|e| Self::io_err(&target, e))?;
                    continue;
                }
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| Self::io_err(parent, e))?;
                }
                let mut bytes = Vec::with_capacity(entry.size() as usize);
                entry
                    .read_to_end(&mut bytes)
                    .map_err(|e| Self::io_err(&archive_c, e))?;
                std::fs::write(&target, &bytes).map_err(|e| Self::io_err(&target, e))?;
            }
            Ok(())
        })
        .await
        .map_err(|e| WizardError::IoError {
            path: archive.to_path_buf(),
            reason: format!("extraction task failed: {e}"),
        })?
    }

    async fn run_elevated(&self, cmd: &PlannedCommand) -> Result<CommandOutput, WizardError> {
        #[cfg(windows)]
        {
            run_elevated_windows(cmd)
        }
        #[cfg(not(windows))]
        {
            Err(WizardError::IoError {
                path: PathBuf::from(cmd.argv.first().cloned().unwrap_or_default()),
                reason: "elevated execution is only supported on Windows".to_string(),
            })
        }
    }

    async fn http_get(&self, url: &str) -> Result<HttpResponse, WizardError> {
        http_get_impl(url, &[]).await
    }

    async fn http_get_with_headers(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<HttpResponse, WizardError> {
        http_get_impl(url, headers).await
    }

    async fn http_post(&self, url: &str, body: &str) -> Result<HttpResponse, WizardError> {
        http_post_impl(url, body, &[]).await
    }

    async fn http_post_with_headers(
        &self,
        url: &str,
        body: &str,
        headers: &[(String, String)],
    ) -> Result<HttpResponse, WizardError> {
        http_post_impl(url, body, headers).await
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

    async fn remove_file(&self, path: &Path) -> Result<(), WizardError> {
        match tokio::fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Self::io_err(path, e)),
        }
    }

    async fn remove_dir_all(&self, path: &Path) -> Result<(), WizardError> {
        match tokio::fs::remove_dir_all(path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Self::io_err(path, e)),
        }
    }

    async fn file_exists(&self, path: &Path) -> bool {
        tokio::fs::metadata(path).await.is_ok()
    }

    async fn file_len(&self, path: &Path) -> Option<u64> {
        tokio::fs::metadata(path)
            .await
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.len())
    }

    async fn sha256_file(&self, path: &Path) -> Result<String, WizardError> {
        // Streamed hashing via the shared implementation: the
        // previous read-the-whole-file-then-hash version
        // materialized the entire GGUF in memory, and the failed
        // allocation on a multi-GiB file aborted the whole process
        // (the v0.1.31 crash on the 45.9 GiB Coder-Next file, hit
        // by the launch-time wizard audit).
        let owned = path.to_path_buf();
        match tokio::task::spawn_blocking(move || manager_models::sha256_hex(&owned)).await {
            Ok(Ok(hex)) => Ok(hex),
            Ok(Err(e)) => Err(WizardError::IoError {
                path: path.to_path_buf(),
                reason: e.to_string(),
            }),
            Err(e) => Err(WizardError::IoError {
                path: path.to_path_buf(),
                reason: format!("hash task failed: {e}"),
            }),
        }
    }

    async fn gpu_inventory(&self) -> Result<Vec<GpuDescriptor>, WizardError> {
        // Same System32-first nvidia-smi resolution as telemetry
        // (manager_telemetry::default_smi_path): a bare name resolves
        // through PATH, which a same-named executable could shadow.
        let smi = manager_telemetry::default_smi_path()
            .to_string_lossy()
            .into_owned();
        let rows = PlannedCommand::new(vec![
            smi.clone(),
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
            .run_command(&PlannedCommand::new(vec![smi]))
            .await
            .map_err(|e| WizardError::GpuProbeFailed(format!("nvidia-smi failed: {e}")))?;
        let driver = Self::parse_driver_version(&header.stdout);
        Ok(Self::parse_gpu_rows(&out.stdout, &driver))
    }

    async fn credential_exists(&self, target: &str) -> Result<bool, WizardError> {
        #[cfg(windows)]
        {
            // tokio Command below: creation_flags is inherent on
            // Windows, so no std CommandExt import is needed here.
            let mut cmd = tokio::process::Command::new("cmdkey");
            cmd.arg(format!("/list:{target}"));
            // CREATE_NO_WINDOW: this probe runs from a GUI process.
            cmd.creation_flags(0x08000000);
            let out = cmd.output().await.map_err(|e| WizardError::IoError {
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

    async fn credential_read(&self, target: &str) -> Option<String> {
        #[cfg(windows)]
        {
            cred_read_windows(target)
        }
        #[cfg(not(windows))]
        {
            let _ = target;
            // No Credential Manager off Windows.
            None
        }
    }

    async fn credential_write(&self, target: &str, secret: &str) -> Result<(), WizardError> {
        #[cfg(windows)]
        {
            cred_write_windows(target, secret).ok_or_else(|| WizardError::IoError {
                path: PathBuf::from("Credential Manager"),
                reason: format!("CredWriteW failed for target '{target}'"),
            })
        }
        #[cfg(not(windows))]
        {
            let _ = (target, secret);
            Err(WizardError::IoError {
                path: PathBuf::from("Credential Manager"),
                reason: "credential storage is only supported on Windows".to_string(),
            })
        }
    }

    fn home_dir(&self) -> Option<PathBuf> {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }

    async fn vscode_cli(&self) -> Option<String> {
        #[cfg(windows)]
        {
            find_vscode_cli_windows()
        }
        #[cfg(not(windows))]
        {
            // The PATH probe in the step is the detection mechanism
            // off Windows.
            None
        }
    }
}
