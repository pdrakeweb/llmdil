//! App-supervised (Local exposure) MCP server processes, derived from
//! the persisted registry `AppConfig.mcp` (review v0.1.60 PS-5).
//!
//! The pure rules — which registry entries can run locally, command
//! resolution, the spawn-time allowlist / banned-arg re-check, and
//! credential injection — live in [`manager_mcp::local`]. This module
//! adds the app's side: the credential store (read only at spawn
//! time, the value handed only to the child environment and never
//! kept in the supervisor's registered spec afterwards), the shared
//! supervisor, and the lifecycle rule that changing or deleting a
//! running Local server stops the old process first.
//!
//! Crash handling: a Local server that exits on its own is reaped and
//! reported as `failed` (`process_state` on `mcp_list`). It is not
//! restarted automatically — the app has no crash-restart loop for
//! any supervised process (review PL-m5, a pending decision), so a
//! restart limit has nothing to attach to yet.

use crate::{ApiError, AppState};
use manager_config::{McpExposure, McpServerConfig};
use manager_mcp::local::{self, CommandContext, LocalSpecError};
use manager_supervisor::{ProcessSpec, ProcessState, Supervisor};
use serde_json::{json, Value};

/// Why a Local server was not started.
#[derive(Debug)]
pub(crate) enum StartError {
    /// The registry entry cannot run locally (explicit, see
    /// [`LocalSpecError`]); nothing was spawned.
    Spec(LocalSpecError),
    /// The supervisor failed to stop the previous process or to spawn.
    Spawn(ApiError),
}

impl From<LocalSpecError> for StartError {
    fn from(e: LocalSpecError) -> Self {
        StartError::Spec(e)
    }
}

/// The credential store. Presence and reads go to Windows Credential
/// Manager through the wizard's ops layer.
#[cfg(not(test))]
mod creds {
    use manager_wizard::{ops::RealSystemOps, SystemOps};

    pub(super) async fn exists(target: &str) -> bool {
        RealSystemOps::default()
            .credential_exists(target)
            .await
            .unwrap_or(false)
    }

    pub(super) async fn read(target: &str) -> Option<String> {
        RealSystemOps::default().credential_read(target).await
    }
}

/// Unit tests never touch the machine's Credential Manager (a stored
/// credential there would make an op test spawn a real server): a
/// per-thread fake store, empty unless a test fills it.
#[cfg(test)]
pub(crate) mod creds {
    use std::cell::RefCell;
    use std::collections::HashMap;

    thread_local! {
        static STORE: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    }

    pub(crate) fn set(target: &str, value: &str) {
        STORE.with(|s| s.borrow_mut().insert(target.to_string(), value.to_string()));
    }

    pub(super) async fn exists(target: &str) -> bool {
        STORE.with(|s| s.borrow().contains_key(target))
    }

    pub(super) async fn read(target: &str) -> Option<String> {
        STORE.with(|s| s.borrow().get(target).cloned())
    }
}

fn command_context(state: &AppState) -> CommandContext {
    CommandContext::from_env(std::slice::from_ref(&state.data_dir))
}

/// True when the supervisor has a live (Running) process for `id`.
fn is_live(sup: &Supervisor, process_id: &str) -> bool {
    sup.status()
        .iter()
        .any(|s| s.id == process_id && s.state == ProcessState::Running)
}

/// The supervisor state of a server's process: `Stopped` when it was
/// never started this session, `Failed` once a crash was reaped.
pub(crate) fn process_state(sup: &Supervisor, server_id: &str) -> ProcessState {
    let pid = manager_mcp::process_id(server_id);
    sup.status()
        .into_iter()
        .find(|s| s.id == pid)
        .map(|s| s.state)
        .unwrap_or(ProcessState::Stopped)
}

/// Everything a start checks short of spawning: the static rules,
/// command resolution, and credential PRESENCE (no value is read).
/// Used where a Local definition is saved without being started.
pub(crate) async fn check_runnable(
    state: &AppState,
    cfg: &McpServerConfig,
) -> Result<(), LocalSpecError> {
    let spec = local::local_launch(cfg)?;
    local::resolve_command(&spec, &command_context(state))?;
    let mut present: Option<String> = None;
    for target in local::required_credential_targets(&spec)? {
        if creds::exists(&target).await {
            present = Some(target);
            break;
        }
    }
    local::check_credential_presence(&spec, |t| present.as_deref() == Some(t))
}

/// Why a start of this Local server would fail right now, in plain
/// words (`None`: it can start). The same checks as [`check_runnable`].
async fn start_problem(state: &AppState, cfg: &McpServerConfig) -> Option<String> {
    Some(match check_runnable(state, cfg).await.err()? {
        LocalSpecError::CredentialMissing { .. } => {
            "credential missing: store it to start this server".to_string()
        }
        LocalSpecError::CommandNotFound { command, .. } => format!("{command} not found on PATH"),
        e => e.to_string(),
    })
}

/// Why an enabled server is not usable right now, in plain words, or
/// `None`. `state_now` is its process state; `credential_stored` is
/// whether its own `cred_ref` is in the credential store (the target
/// the gateway reads).
///
/// The app starts a Local server when its exposure is set to Local, and
/// never at app launch: an stdio server serves the program that
/// launched it, and the app has no MCP client of its own. Gateway
/// servers are started by the gateway, which leaves out one whose
/// credential is missing.
pub(crate) async fn blocked_reason(
    state: &AppState,
    cfg: &McpServerConfig,
    state_now: ProcessState,
    credential_stored: bool,
) -> Option<String> {
    match cfg.exposure {
        McpExposure::Disabled => None,
        McpExposure::Gateway => {
            if cfg.env_keys.is_empty() || credential_stored {
                return None;
            }
            let named = cfg
                .cred_ref
                .as_deref()
                .is_some_and(|r| !r.trim().is_empty());
            Some(if named {
                "credential missing: the gateway leaves this server out until it is stored"
                    .to_string()
            } else {
                "no credential target is set, so the gateway leaves this server out".to_string()
            })
        }
        McpExposure::Local => {
            if state_now == ProcessState::Running {
                return None;
            }
            if let Some(problem) = start_problem(state, cfg).await {
                return Some(problem);
            }
            Some(if state_now == ProcessState::Failed {
                "the process exited on its own; see its log. Turn it off and on to start it again"
                    .to_string()
            } else {
                "not started: LLMdil does not start local servers when it opens. Turn it off and on to start it"
                    .to_string()
            })
        }
    }
}

/// Start a Local server from its registry entry. Every check runs
/// again here (a config edited on disk never went through save-time
/// validation). A live process under the same id is stopped first so
/// the new definition always wins. After the spawn the supervisor's
/// registered spec is replaced with one that carries no env values:
/// the credential exists only in the child's environment.
pub(crate) async fn start(state: &AppState, cfg: &McpServerConfig) -> Result<(), StartError> {
    let ctx = command_context(state);
    // Static rules and command resolution first: nothing is read
    // from the credential store for a server that cannot spawn.
    let spec = local::local_launch(cfg)?;
    local::resolve_command(&spec, &ctx)?;
    let mut found: Option<(String, String)> = None;
    for target in local::required_credential_targets(&spec)? {
        if let Some(value) = creds::read(&target).await.filter(|v| !v.is_empty()) {
            found = Some((target, value));
            break;
        }
    }
    let process = local::prepare_spawn(cfg, &ctx, |t| {
        found
            .as_ref()
            .filter(|(target, _)| target == t)
            .map(|(_, value)| value.clone())
    })?;
    drop(found);
    let id = process.id.clone();
    let scrubbed = ProcessSpec {
        env: Vec::new(),
        ..process.clone()
    };
    let mut sup = state.supervisor().await.lock().await;
    sup.reap_exited();
    if is_live(&sup, &id) {
        sup.stop(&id, true)
            .await
            .map_err(|e| StartError::Spawn(ApiError::Mcp(format!("supervisor error: {e}"))))?;
    }
    sup.register_spec(process);
    let spawned = sup.spawn_backend(&id).await;
    sup.register_spec(scrubbed);
    spawned.map_err(|e| StartError::Spawn(ApiError::Mcp(format!("supervisor error: {e}"))))
}

/// Stop a server's process if one is live. Returns whether a process
/// was stopped. Works for every registry entry (the process id is
/// derived from the server id alone).
pub(crate) async fn stop(state: &AppState, server_id: &str) -> Result<bool, ApiError> {
    let id = manager_mcp::process_id(server_id);
    let mut sup = state.supervisor().await.lock().await;
    sup.reap_exited();
    if !is_live(&sup, &id) {
        return Ok(false);
    }
    sup.stop(&id, true)
        .await
        .map_err(|e| ApiError::Mcp(format!("supervisor error: {e}")))?;
    Ok(true)
}

/// Stop a deleted server's process and drop its supervisor spec and
/// history, so an id re-added later starts clean.
pub(crate) async fn forget(state: &AppState, server_id: &str) -> Result<(), ApiError> {
    stop(state, server_id).await?;
    let id = manager_mcp::process_id(server_id);
    state.supervisor().await.lock().await.unregister_spec(&id);
    Ok(())
}

/// True when a change between two definitions of the same server
/// affects what (or whether) the app spawns.
fn spawn_definition_changed(a: &McpServerConfig, b: &McpServerConfig) -> bool {
    a.exposure != b.exposure
        || a.transport != b.transport
        || a.command != b.command
        || a.args != b.args
        || a.env_keys != b.env_keys
        || a.cred_ref != b.cred_ref
}

/// The process side of `mcp_save_server`, after the save persisted.
/// A running process whose spawn definition changed is stopped
/// first (no orphan keeps running the old command); when the server
/// is still Local it is started again with the new definition. A
/// Local server that is not (re)started is checked as a start would
/// check it, and the result is reported in-band: `local_ready`, plus
/// `local_error` naming the problem (a missing credential is a
/// runtime state the operator fixes after saving, MCP design §3.2).
pub(crate) async fn reconcile_after_save(
    state: &AppState,
    previous: Option<&McpServerConfig>,
    saved: &McpServerConfig,
) -> Result<Value, ApiError> {
    let id = &saved.id;
    let changed = previous.is_none_or(|p| spawn_definition_changed(p, saved));
    let mut restarted = false;
    if changed && stop(state, id).await? && saved.exposure == McpExposure::Local {
        start(state, saved).await.map_err(|e| {
            let reason = match e {
                StartError::Spec(e) => e.to_string(),
                StartError::Spawn(e) => e.to_string(),
            };
            ApiError::Mcp(format!(
                "mcp server {id} saved and its previous process stopped, but the new \
                 definition was not started: {reason}"
            ))
        })?;
        restarted = true;
    }
    let mut out = json!({ "id": id, "persisted": true, "restarted": restarted });
    if saved.exposure == McpExposure::Local {
        let problem = if restarted {
            None
        } else {
            check_runnable(state, saved).await.err()
        };
        out["local_ready"] = json!(problem.is_none());
        out["local_error"] = json!(problem.map(|e| e.to_string()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        op_mcp_delete_server, op_mcp_list, op_mcp_save_server, op_mcp_set_exposure,
        op_mcp_test_connection, AppConfig, EventSink, NoopSink,
    };
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    async fn row(state: &Arc<AppState>, id: &str) -> Value {
        op_mcp_list(state, &sink())
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .unwrap()
            .clone()
    }

    /// B19: a stopped Local server, and a gateway server without its
    /// credential, say why; a server that needs no credential is not
    /// missing one.
    #[tokio::test]
    async fn rows_say_why_a_server_is_not_running() {
        let (state, dir) = test_state("blocked");
        let (command, args) = fake_node(&dir, 60);
        let mut local = operator_server("blocked_local", &command, args);
        local.exposure = McpExposure::Local;
        let mut free = operator_server("blocked_free", "node", Vec::new());
        free.env_keys = Vec::new();
        free.cred_ref = None;
        let mut gateway = operator_server("blocked_gateway", "npx", Vec::new());
        gateway.exposure = McpExposure::Gateway;
        gateway.metered = true;
        gateway.allowed_tools = Some(vec!["t".into()]);
        gateway.rpm_limit = Some(20);
        for server in [&local, &free, &gateway] {
            op_mcp_save_server(&state, &sink(), server.clone())
                .await
                .unwrap();
        }

        // The save left the Local server without a credential.
        let r = row(&state, "blocked_local").await;
        assert_eq!(r["credential_required"], true);
        assert_eq!(r["credential_status"], "missing");
        assert_eq!(r["starts_with_app"], false);
        let why = r["blocked_reason"].as_str().unwrap();
        assert!(why.starts_with("credential missing"), "{why}");
        let t = op_mcp_test_connection(&state, &sink(), "blocked_local".into())
            .await
            .unwrap();
        assert_eq!(t["ok"], false);
        assert!(t["detail"].as_str().unwrap().contains(why), "{t}");

        // Startable, but nothing started it.
        creds::set("test/blocked_local", "v");
        let why = row(&state, "blocked_local").await["blocked_reason"].clone();
        assert!(why.as_str().unwrap().starts_with("not started"), "{why}");

        // The command is gone.
        let mut gone = local.clone();
        gone.command = Some(dir.join("npx").to_string_lossy().into_owned());
        let why = blocked_reason(&state, &gone, ProcessState::Stopped, true)
            .await
            .unwrap();
        assert!(why.contains("not found"), "{why}");

        // No credential needed: not missing; disabled: not blocked.
        let r = row(&state, "blocked_free").await;
        assert_eq!(r["credential_required"], false);
        assert_eq!(r["credential_status"], "not_required");
        assert!(r["blocked_reason"].is_null(), "{r}");

        // The gateway leaves it out.
        let why = row(&state, "blocked_gateway").await["blocked_reason"].clone();
        assert!(
            why.as_str().unwrap().starts_with("credential missing"),
            "{why}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_state(tag: &str) -> (Arc<AppState>, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "llm-mgr-mcp-local-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = AppConfig::default_config();
        cfg.data_dir = dir.clone();
        let state = Arc::new(AppState::new(cfg, dir.join("config.json"), dir.clone()));
        (state, dir)
    }

    fn sink() -> Arc<dyn EventSink> {
        Arc::new(NoopSink)
    }

    /// A real program copied into `dir` under the allowlisted name
    /// `node`, with args that make it wait `secs` seconds (0 = exit
    /// at once). Windows: ping.exe; elsewhere: sleep.
    fn fake_node(dir: &std::path::Path, secs: u32) -> (String, Vec<String>) {
        #[cfg(windows)]
        let (src, dst) = {
            let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
            (
                PathBuf::from(root).join("System32").join("PING.EXE"),
                dir.join("node.exe"),
            )
        };
        #[cfg(windows)]
        std::fs::copy(src, &dst).unwrap();
        // Elsewhere a wrapper script, not a copy: multi-call coreutils
        // builds (e.g. uutils on newer Ubuntu runners) pick the utility
        // from argv[0], so a `sleep` binary copied as `node` exits 1.
        #[cfg(not(windows))]
        let dst = {
            use std::os::unix::fs::PermissionsExt;
            let sleep = ["/bin/sleep", "/usr/bin/sleep"]
                .into_iter()
                .map(PathBuf::from)
                .find(|p| p.exists())
                .unwrap();
            let dst = dir.join("node");
            std::fs::write(
                &dst,
                format!("#!/bin/sh\nexec {} \"$@\"\n", sleep.display()),
            )
            .unwrap();
            std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755)).unwrap();
            dst
        };
        (dst.to_string_lossy().into_owned(), node_args(secs))
    }

    fn node_args(secs: u32) -> Vec<String> {
        if cfg!(windows) {
            vec!["-n".into(), (secs + 1).to_string(), "127.0.0.1".into()]
        } else {
            vec![secs.to_string()]
        }
    }

    fn operator_server(id: &str, command: &str, args: Vec<String>) -> McpServerConfig {
        McpServerConfig {
            id: id.to_string(),
            display_name: "Operator".to_string(),
            transport: manager_config::McpTransport::Stdio,
            exposure: McpExposure::Disabled,
            command: Some(command.to_string()),
            args,
            url: None,
            env_keys: vec!["OPERATOR_TOKEN".to_string()],
            cred_ref: Some(format!("test/{id}")),
            signup_url: None,
            credential_hint: None,
            allowed_tools: None,
            metered: false,
            rpm_limit: None,
        }
    }

    async fn pid_of(state: &AppState, id: &str) -> Option<u32> {
        let pid = manager_mcp::process_id(id);
        let mut sup = state.supervisor().await.lock().await;
        sup.reap_exited();
        sup.status()
            .into_iter()
            .find(|s| s.id == pid && s.state == ProcessState::Running)
            .and_then(|s| s.pid)
    }

    /// PS-5: an operator-added stdio server runs locally with its
    /// credential injected at spawn and no value kept by the
    /// supervisor; editing its args stops the old process before the
    /// new definition starts; deleting it stops it and forgets it.
    #[tokio::test]
    async fn operator_server_runs_locally_and_edit_or_delete_stops_it() {
        let (state, dir) = test_state("lifecycle");
        let (command, args) = fake_node(&dir, 60);
        let server = operator_server("operator_tool", &command, args);
        op_mcp_save_server(&state, &sink(), server.clone())
            .await
            .unwrap();
        creds::set("test/operator_tool", "s3cr3t-operator-value");

        op_mcp_set_exposure(&state, &sink(), server.id.clone(), "local".into())
            .await
            .unwrap();
        let first = pid_of(&state, &server.id).await.expect("spawned");
        {
            let sup = state.supervisor().await.lock().await;
            let spec = sup.spec("mcp-operator_tool").unwrap();
            assert!(spec.env.is_empty(), "no credential value kept after spawn");
            assert!(spec.program.is_absolute());
        }
        let tested = op_mcp_test_connection(&state, &sink(), server.id.clone())
            .await
            .unwrap();
        assert_eq!(tested["ok"], true, "{tested}");

        // A display-name-only change does not restart.
        let mut edited = state
            .mcp_registry()
            .await
            .into_iter()
            .find(|s| s.id == server.id)
            .unwrap();
        edited.display_name = "Renamed".into();
        let v = op_mcp_save_server(&state, &sink(), edited.clone())
            .await
            .unwrap();
        assert_eq!(v["restarted"], false, "{v}");
        assert_eq!(pid_of(&state, &server.id).await, Some(first));

        // Editing the args of the running Local server.
        edited.args = node_args(59);
        let v = op_mcp_save_server(&state, &sink(), edited.clone())
            .await
            .unwrap();
        assert_eq!(v["restarted"], true, "{v}");
        let second = pid_of(&state, &server.id).await.expect("restarted");
        assert_ne!(first, second, "old process stopped, new one spawned");
        assert!(!manager_supervisor::pid_alive(first), "no orphan");

        op_mcp_delete_server(&state, &sink(), server.id.clone())
            .await
            .unwrap();
        assert_eq!(pid_of(&state, &server.id).await, None);
        assert!(!manager_supervisor::pid_alive(second), "no orphan");
        assert!(state
            .supervisor()
            .await
            .lock()
            .await
            .spec("mcp-operator_tool")
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// PS-5: an http server cannot become a local process — refused
    /// at save (nothing stored) and at set-exposure (explicit error).
    #[tokio::test]
    async fn http_server_local_is_an_explicit_error() {
        let (state, dir) = test_state("http");
        let mut http = operator_server("remote_tool", "npx", Vec::new());
        http.transport = manager_config::McpTransport::Http;
        http.command = None;
        http.url = Some("https://mcp.example.com/mcp".into());
        http.exposure = McpExposure::Local;
        let err = op_mcp_save_server(&state, &sink(), http.clone())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("http transport"), "{err}");
        assert!(!state
            .mcp_registry()
            .await
            .iter()
            .any(|s| s.id == "remote_tool"));

        http.exposure = McpExposure::Disabled;
        op_mcp_save_server(&state, &sink(), http.clone())
            .await
            .unwrap();
        let err = op_mcp_set_exposure(&state, &sink(), http.id.clone(), "local".into())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("http transport"), "{err}");
        assert_eq!(pid_of(&state, &http.id).await, None);
        let t = op_mcp_test_connection(&state, &sink(), http.id.clone())
            .await
            .unwrap();
        assert_eq!(t["ok"], false);
        assert!(
            t["detail"].as_str().unwrap().contains("http transport"),
            "{t}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// PS-5: a missing credential is reported by save and is an
    /// explicit error at start (set exposure); nothing is spawned and
    /// the error names the target, never a value.
    #[tokio::test]
    async fn missing_credential_is_explicit_at_save_and_start() {
        let (state, dir) = test_state("cred");
        let (command, args) = fake_node(&dir, 60);
        let mut server = operator_server("needs_cred", &command, args);
        server.exposure = McpExposure::Local;
        let v = op_mcp_save_server(&state, &sink(), server.clone())
            .await
            .unwrap();
        assert_eq!(v["local_ready"], false, "{v}");
        assert!(
            v["local_error"]
                .as_str()
                .unwrap()
                .contains("credential missing"),
            "{v}"
        );

        server.exposure = McpExposure::Disabled;
        op_mcp_save_server(&state, &sink(), server.clone())
            .await
            .unwrap();
        let err = op_mcp_set_exposure(&state, &sink(), server.id.clone(), "local".into())
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("credential missing"), "{msg}");
        assert!(msg.contains("test/needs_cred"), "{msg}");
        assert_eq!(pid_of(&state, &server.id).await, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// PS-5: an absolute command outside every known install
    /// directory is refused at start even though save-time
    /// validation (the basename allowlist) accepts it.
    #[tokio::test]
    async fn command_outside_install_dirs_is_refused() {
        let (state, dir) = test_state("outside");
        let (_, elsewhere) = test_state("outside-elsewhere");
        let (command, args) = fake_node(&elsewhere, 60);
        let server = operator_server("outside_tool", &command, args);
        op_mcp_save_server(&state, &sink(), server.clone())
            .await
            .unwrap();
        creds::set("test/outside_tool", "value");
        let err = op_mcp_set_exposure(&state, &sink(), server.id.clone(), "local".into())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("install directory"), "{err}");
        assert_eq!(pid_of(&state, &server.id).await, None);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    /// PS-5: a Local server that exits on its own shows as failed in
    /// `mcp_list`, which lists every registry entry.
    #[tokio::test]
    async fn crashed_local_server_is_visible_as_failed() {
        let (state, dir) = test_state("crash");
        let (command, args) = fake_node(&dir, 0);
        let mut server = operator_server("short_lived", &command, args);
        server.env_keys.clear();
        server.cred_ref = None;
        op_mcp_save_server(&state, &sink(), server.clone())
            .await
            .unwrap();
        op_mcp_set_exposure(&state, &sink(), server.id.clone(), "local".into())
            .await
            .unwrap();
        let mut seen = String::new();
        for _ in 0..100 {
            let list = op_mcp_list(&state, &sink()).await.unwrap();
            let entry = list
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["id"] == "short_lived")
                .unwrap()
                .clone();
            seen = entry["process_state"].as_str().unwrap().to_string();
            if seen == "failed" {
                assert_eq!(entry["running"], false);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(seen, "failed");
        let list = op_mcp_list(&state, &sink()).await.unwrap();
        assert_eq!(
            list.as_array().unwrap().len(),
            state.mcp_registry().await.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// PS-5: delete and the test op work for curated servers, which
    /// used to return NotFound (they had no built-in spec).
    #[tokio::test]
    async fn delete_and_test_work_for_derived_servers() {
        let (state, dir) = test_state("derived");
        let t = op_mcp_test_connection(&state, &sink(), "web_fetch".into())
            .await
            .unwrap();
        assert_eq!(t["ok"], false);
        assert_eq!(t["detail"], "server disabled");
        let v = op_mcp_delete_server(&state, &sink(), "web_fetch".into())
            .await
            .unwrap();
        assert_eq!(v["deleted"], true);
        assert!(!state
            .mcp_registry()
            .await
            .iter()
            .any(|s| s.id == "web_fetch"));
        let err = op_mcp_delete_server(&state, &sink(), "web_fetch".into())
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::NotFound(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
