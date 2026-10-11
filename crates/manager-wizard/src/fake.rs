//! Hermetic [`SystemOps`](crate::SystemOps) test harness.
//!
//! [`FakeSystemOps`] records every effect (commands, spawns, downloads,
//! HTTP, file writes) and serves scripted responses, so step tests run
//! without GPUs, networks, or installers. Other crates may reuse it for
//! their own wizard-adjacent tests.

use crate::{CommandOutput, GpuDescriptor, HttpResponse, PlannedCommand, SystemOps, WizardError};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Scripted behavior for one command invocation.
#[derive(Debug, Clone)]
pub enum FakeCommandBehavior {
    Output(CommandOutput),
    Fail { code: i32, stderr: String },
}

impl FakeCommandBehavior {
    fn into_result(self, argv: &[String]) -> Result<CommandOutput, WizardError> {
        match self {
            FakeCommandBehavior::Output(o) => Ok(o),
            FakeCommandBehavior::Fail { code, stderr } => Err(WizardError::CommandFailed {
                argv: argv.to_vec(),
                code,
                stderr,
            }),
        }
    }
}

#[derive(Debug)]
struct FakeState {
    commands: HashMap<String, FakeCommandBehavior>,
    executed: Vec<Vec<String>>,
    spawns: Vec<Vec<String>>,
    files: HashMap<PathBuf, Vec<u8>>,
    downloads: HashMap<String, Vec<u8>>,
    progress_calls: Vec<(u64, u64)>,
    downloaded_to: Vec<(String, PathBuf)>,
    extracted: Vec<(PathBuf, PathBuf)>,
    http_get: HashMap<String, HttpResponse>,
    http_post: HashMap<String, HttpResponse>,
    posts: Vec<(String, String)>,
    gets: Vec<String>,
    gpus: Vec<GpuDescriptor>,
    gpu_error: Option<String>,
    credentials: HashSet<String>,
    home: Option<PathBuf>,
    free_space: u64,
    head_lengths: HashMap<String, u64>,
}

impl Default for FakeState {
    fn default() -> Self {
        Self {
            commands: HashMap::new(),
            executed: Vec::new(),
            spawns: Vec::new(),
            files: HashMap::new(),
            downloads: HashMap::new(),
            progress_calls: Vec::new(),
            downloaded_to: Vec::new(),
            extracted: Vec::new(),
            http_get: HashMap::new(),
            http_post: HashMap::new(),
            posts: Vec::new(),
            gets: Vec::new(),
            gpus: Vec::new(),
            gpu_error: None,
            credentials: HashSet::new(),
            home: None,
            // Default: effectively unlimited, so existing tests never trip
            // the disk-space pre-check.
            free_space: u64::MAX,
            head_lengths: HashMap::new(),
        }
    }
}

fn key(argv: &[String]) -> String {
    argv.join("\u{1f}")
}

/// Hermetic test double for [`SystemOps`].
///
/// Unregistered commands succeed with empty output; unregistered downloads
/// and HTTP calls fail. Use the `with_*` builders to script behavior and
/// the accessors to assert on recorded effects.
#[derive(Debug, Default)]
pub struct FakeSystemOps {
    state: Mutex<FakeState>,
}

impl FakeSystemOps {
    pub fn new() -> Self {
        Self::default()
    }

    /// Script a command's response, matched on exact argv.
    pub fn with_command(&self, argv: &[&str], output: CommandOutput) -> &Self {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        self.state
            .lock()
            .unwrap()
            .commands
            .insert(key(&argv), FakeCommandBehavior::Output(output));
        self
    }

    /// Script a command failure, matched on exact argv.
    pub fn with_command_failure(&self, argv: &[&str], code: i32, stderr: &str) -> &Self {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        self.state.lock().unwrap().commands.insert(
            key(&argv),
            FakeCommandBehavior::Fail {
                code,
                stderr: stderr.to_string(),
            },
        );
        self
    }

    /// Convenience: script `code --list-extensions` output.
    pub fn with_listed_extensions(&self, extensions: &[&str]) -> &Self {
        self.with_command(
            &["code", "--list-extensions"],
            CommandOutput {
                exit_code: 0,
                stdout: extensions.join("\n"),
                stderr: String::new(),
            },
        )
    }

    pub fn with_file(&self, path: PathBuf, bytes: Vec<u8>) -> &Self {
        self.state.lock().unwrap().files.insert(path, bytes);
        self
    }

    pub fn with_download(&self, url: &str, bytes: Vec<u8>) -> &Self {
        self.state
            .lock()
            .unwrap()
            .downloads
            .insert(url.to_string(), bytes);
        self
    }

    pub fn with_http_get(&self, url: &str, resp: HttpResponse) -> &Self {
        self.state
            .lock()
            .unwrap()
            .http_get
            .insert(url.to_string(), resp);
        self
    }

    pub fn with_http_post(&self, url: &str, resp: HttpResponse) -> &Self {
        self.state
            .lock()
            .unwrap()
            .http_post
            .insert(url.to_string(), resp);
        self
    }

    pub fn with_gpus(&self, gpus: Vec<GpuDescriptor>) -> &Self {
        self.state.lock().unwrap().gpus = gpus;
        self
    }

    pub fn with_gpu_error(&self, msg: &str) -> &Self {
        self.state.lock().unwrap().gpu_error = Some(msg.to_string());
        self
    }

    pub fn with_credential(&self, target: &str) -> &Self {
        self.state
            .lock()
            .unwrap()
            .credentials
            .insert(target.to_string());
        self
    }

    pub fn with_home(&self, home: PathBuf) -> &Self {
        self.state.lock().unwrap().home = Some(home);
        self
    }

    /// Script the free space reported by `free_space_bytes`.
    pub fn with_free_space(&self, bytes: u64) -> &Self {
        self.state.lock().unwrap().free_space = bytes;
        self
    }

    /// Script the Content-Length reported by `http_head_content_length`.
    pub fn with_head_length(&self, url: &str, bytes: u64) -> &Self {
        self.state
            .lock()
            .unwrap()
            .head_lengths
            .insert(url.to_string(), bytes);
        self
    }

    /// Every argv passed to `run_command`, in order.
    pub fn executed_commands(&self) -> Vec<Vec<String>> {
        self.state.lock().unwrap().executed.clone()
    }

    /// Every argv passed to `spawn_detached`, in order.
    pub fn spawns(&self) -> Vec<Vec<String>> {
        self.state.lock().unwrap().spawns.clone()
    }

    /// (url, dest) download calls, in order.
    pub fn downloads_made(&self) -> Vec<(String, PathBuf)> {
        self.state.lock().unwrap().downloaded_to.clone()
    }

    /// (archive, dest) extraction calls, in order.
    pub fn extractions(&self) -> Vec<(PathBuf, PathBuf)> {
        self.state.lock().unwrap().extracted.clone()
    }

    /// (url, body) POST calls, in order.
    pub fn posts(&self) -> Vec<(String, String)> {
        self.state.lock().unwrap().posts.clone()
    }

    /// GET urls requested, in order.
    pub fn gets(&self) -> Vec<String> {
        self.state.lock().unwrap().gets.clone()
    }

    /// Current fake file contents.
    pub fn file_bytes(&self, path: &PathBuf) -> Option<Vec<u8>> {
        self.state.lock().unwrap().files.get(path).cloned()
    }

    /// Every (url, dest) passed to `download`, in order.
    pub fn downloaded_to(&self) -> Vec<(String, PathBuf)> {
        self.state.lock().unwrap().downloaded_to.clone()
    }

    pub fn ok_output(stdout: &str) -> CommandOutput {
        CommandOutput {
            exit_code: 0,
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    pub fn http_ok(body: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: body.as_bytes().to_vec(),
            elapsed_ms: 12,
        }
    }
}

#[async_trait]
impl SystemOps for FakeSystemOps {
    async fn run_command(&self, cmd: &PlannedCommand) -> Result<CommandOutput, WizardError> {
        let mut st = self.state.lock().unwrap();
        st.executed.push(cmd.argv.clone());
        match st.commands.get(&key(&cmd.argv)).cloned() {
            Some(behavior) => behavior.into_result(&cmd.argv),
            // Default: succeed quietly so tests only script what they assert on.
            None => Ok(CommandOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }),
        }
    }

    async fn spawn_detached(&self, cmd: &PlannedCommand) -> Result<u32, WizardError> {
        self.state.lock().unwrap().spawns.push(cmd.argv.clone());
        Ok(4242)
    }

    async fn download(
        &self,
        url: &str,
        dest: &Path,
        on_progress: &(dyn Fn(u64, u64) + Sync),
    ) -> Result<u64, WizardError> {
        let mut st = self.state.lock().unwrap();
        let bytes = st
            .downloads
            .get(url)
            .cloned()
            .ok_or_else(|| WizardError::DownloadFailed {
                url: url.to_string(),
                reason: "no scripted download for this URL".to_string(),
            })?;
        let total = bytes.len() as u64;
        on_progress(total / 2, total);
        on_progress(total, total);
        st.progress_calls.push((total / 2, total));
        st.progress_calls.push((total, total));
        st.downloaded_to.push((url.to_string(), dest.to_path_buf()));
        st.files.insert(dest.to_path_buf(), bytes);
        Ok(total)
    }

    async fn extract_zip(&self, archive: &Path, dest: &Path) -> Result<(), WizardError> {
        self.state
            .lock()
            .unwrap()
            .extracted
            .push((archive.to_path_buf(), dest.to_path_buf()));
        Ok(())
    }

    async fn http_get(&self, url: &str) -> Result<HttpResponse, WizardError> {
        let mut st = self.state.lock().unwrap();
        st.gets.push(url.to_string());
        st.http_get
            .get(url)
            .cloned()
            .ok_or_else(|| WizardError::HttpError {
                url: url.to_string(),
                reason: "no scripted GET response for this URL".to_string(),
            })
    }

    async fn http_post(&self, url: &str, body: &str) -> Result<HttpResponse, WizardError> {
        let mut st = self.state.lock().unwrap();
        st.posts.push((url.to_string(), body.to_string()));
        st.http_post
            .get(url)
            .cloned()
            .ok_or_else(|| WizardError::HttpError {
                url: url.to_string(),
                reason: "no scripted POST response for this URL".to_string(),
            })
    }

    async fn http_head_content_length(&self, url: &str) -> Result<Option<u64>, WizardError> {
        Ok(self.state.lock().unwrap().head_lengths.get(url).copied())
    }

    async fn free_space_bytes(&self, _path: &Path) -> Result<u64, WizardError> {
        Ok(self.state.lock().unwrap().free_space)
    }

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>, WizardError> {
        self.state
            .lock()
            .unwrap()
            .files
            .get(path)
            .cloned()
            .ok_or_else(|| WizardError::IoError {
                path: path.to_path_buf(),
                reason: "file not found in fake fs".to_string(),
            })
    }

    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), WizardError> {
        self.state
            .lock()
            .unwrap()
            .files
            .insert(path.to_path_buf(), data.to_vec());
        Ok(())
    }

    async fn file_exists(&self, path: &Path) -> bool {
        self.state.lock().unwrap().files.contains_key(path)
    }

    async fn sha256_file(&self, path: &Path) -> Result<String, WizardError> {
        let data = self.read_file(path).await?;
        let mut hasher = Sha256::new();
        hasher.update(&data);
        Ok(format!("{:x}", hasher.finalize()))
    }

    async fn gpu_inventory(&self) -> Result<Vec<GpuDescriptor>, WizardError> {
        let st = self.state.lock().unwrap();
        if let Some(msg) = &st.gpu_error {
            return Err(WizardError::GpuProbeFailed(msg.clone()));
        }
        Ok(st.gpus.clone())
    }

    async fn credential_exists(&self, target: &str) -> Result<bool, WizardError> {
        Ok(self.state.lock().unwrap().credentials.contains(target))
    }

    fn home_dir(&self) -> Option<PathBuf> {
        self.state.lock().unwrap().home.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unscripted_command_succeeds_quietly() {
        let ops = FakeSystemOps::new();
        let out = ops
            .run_command(&PlannedCommand::new(vec!["whatever".to_string()]))
            .await
            .unwrap();
        assert!(out.success());
        assert_eq!(ops.executed_commands().len(), 1);
    }

    #[tokio::test]
    async fn scripted_command_failure_surfaces() {
        let ops = FakeSystemOps::new();
        ops.with_command_failure(&["uv", "venv"], 1, "uv not found");
        let err = ops
            .run_command(&PlannedCommand::new(vec![
                "uv".to_string(),
                "venv".to_string(),
            ]))
            .await
            .unwrap_err();
        assert!(matches!(err, WizardError::CommandFailed { code: 1, .. }));
    }

    #[tokio::test]
    async fn unscripted_download_fails_closed() {
        let ops = FakeSystemOps::new();
        let err = ops
            .download("https://example.com/x", &PathBuf::from("x"), &|_, _| {})
            .await
            .unwrap_err();
        assert!(matches!(err, WizardError::DownloadFailed { .. }));
    }

    #[tokio::test]
    async fn fake_sha256_matches_real_algorithm() {
        let ops = FakeSystemOps::new();
        let p = PathBuf::from("f.bin");
        ops.with_file(p.clone(), b"hello".to_vec());
        assert_eq!(
            ops.sha256_file(&p).await.unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }
}
