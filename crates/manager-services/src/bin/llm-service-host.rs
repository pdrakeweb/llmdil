//! `llm-service-host` — the Windows service shim and the elevated
//! management helper for manager-services (design note:
//! `docs/windows-services-design.md`).
//!
//! Modes:
//! - `run --definition <path>`: the SCM entry point. The registered
//!   ImagePath of every managed service is this command. The shim
//!   registers its control handler, resolves the definition's env
//!   (credential references read in this process — which runs as the
//!   service account, so the gateway's Credential Manager secrets
//!   resolve), spawns the real child with stdio redirected to the
//!   definition's log files, holds it in a kill-on-close job object
//!   (a stop terminates the whole tree), and reports status to SCM.
//! - `register --definition <path>` / `unregister --name <name>`:
//!   the management operations that need an administrator token. The
//!   app invokes them through its existing elevated-helper pattern
//!   (`runas`). `register` prompts for the account password with the
//!   Windows system credential dialog (CredUI) in this elevated
//!   process and passes it straight to SCM; the password never
//!   crosses the app boundary, is never written to config or logs,
//!   and is zeroed in memory once SCM has it. The durable copy is
//!   the SCM's LSA secret.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("run") => cmd_run(&args[1..]),
        Some("register") => cmd_register(&args[1..]),
        Some("unregister") => cmd_unregister(&args[1..]),
        _ => {
            eprintln!(
                "usage: llm-service-host run --definition <path>\n       llm-service-host register --definition <path>\n       llm-service-host unregister --name <service-name>"
            );
            2
        }
    };
    ExitCode::from(code)
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].clone())
}

/// Append a line to `<data_dir>/logs/service-manage.log` so elevated
/// helper outcomes are diagnosable after the fact (an elevated
/// process's stderr is not visible to the app).
fn manage_log(data_dir: &Path, line: &str) {
    let dir = data_dir.join("logs");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("service-manage.log"))
    {
        use std::io::Write;
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{secs}] {line}");
    }
}

fn cmd_register(args: &[String]) -> u8 {
    let Some(def_path) = arg_value(args, "--definition") else {
        eprintln!("register: --definition <path> is required");
        return 2;
    };
    let def_path = PathBuf::from(def_path);
    // <data_dir>/services/<name>.json → the data dir is two levels up.
    let data_dir = def_path
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf());
    let fail = |msg: String| -> u8 {
        eprintln!("register: {msg}");
        if let Some(dir) = &data_dir {
            manage_log(dir, &format!("register failed: {msg}"));
        }
        1
    };
    let mut def = match manager_services::read_definition(&def_path) {
        Ok(def) => def,
        Err(e) => return fail(format!("{e}")),
    };
    if def.account.is_none() {
        def.account = manager_services::scm::current_user_account();
    }
    if def.account.is_none() {
        return fail("could not resolve the service account".to_string());
    }
    #[cfg(windows)]
    {
        use manager_services::ServiceControl;
        // The password is collected here, in the elevated helper, by
        // the Windows system credential dialog — never by an LLMdil
        // field, never staged through a credential, never logged.
        let password = match prompt_service_account_password(&def) {
            CredentialPrompt::Ok(password) => password,
            CredentialPrompt::Cancelled => {
                return fail("credential prompt cancelled".to_string());
            }
            CredentialPrompt::Failed(msg) => return fail(msg),
        };
        let host_exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => return fail(format!("could not locate llm-service-host.exe: {e}")),
        };
        let image_path = def.image_path(&host_exe, &def_path);
        // `password` drops (zeroed) when this block ends, on every path.
        match manager_services::scm::platform_control().register(&def, &image_path, Some(password.as_str())) {
            Ok(()) => {
                if let Some(dir) = &data_dir {
                    manage_log(dir, &format!("registered {} as {}", def.name, def.account.as_deref().unwrap_or("?")));
                }
                0
            }
            Err(e) => fail(format!("{e}")),
        }
    }
    #[cfg(not(windows))]
    {
        fail("register is only supported on Windows".to_string())
    }
}

fn cmd_unregister(args: &[String]) -> u8 {
    let Some(name) = arg_value(args, "--name") else {
        eprintln!("unregister: --name <service-name> is required");
        return 2;
    };
    #[cfg(windows)]
    {
        use manager_services::ServiceControl;
        match manager_services::scm::platform_control().unregister(&name) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("unregister: {e}");
                1
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        eprintln!("unregister is only supported on Windows");
        2
    }
}

fn cmd_run(args: &[String]) -> u8 {
    let Some(def_path) = arg_value(args, "--definition") else {
        eprintln!("run: --definition <path> is required");
        return 2;
    };
    let def = match manager_services::read_definition(Path::new(&def_path)) {
        Ok(def) => def,
        Err(e) => {
            eprintln!("run: {e}");
            return 1;
        }
    };
    #[cfg(windows)]
    {
        windows_host::run_service(def)
    }
    #[cfg(not(windows))]
    {
        let _ = def;
        eprintln!("run is only supported on Windows");
        2
    }
}

/// Read a generic credential in this process's user context (the
/// service account for `run`, the operator for `register`). Mirrors
/// the blob decoding in manager-wizard's ops: blobs written by the
/// app / cmdkey are UTF-16LE text.
#[cfg(windows)]
fn cred_read(target: &str) -> Option<String> {
    use windows_sys::Win32::Security::Credentials::{
        CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC,
    };
    let wide: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
    let ok = unsafe { CredReadW(wide.as_ptr(), CRED_TYPE_GENERIC, 0, &mut cred) };
    if ok == 0 || cred.is_null() {
        return None;
    }
    let value = unsafe {
        let size = (*cred).CredentialBlobSize as usize;
        let ptr = (*cred).CredentialBlob;
        if ptr.is_null() || size == 0 {
            None
        } else {
            decode_credential_blob(std::slice::from_raw_parts(ptr, size))
        }
    };
    unsafe { CredFree(cred as *const std::ffi::c_void) };
    value.filter(|v| !v.is_empty())
}

/// UTF-16LE first when it decodes to plain ASCII (the app's own
/// writes); UTF-8 otherwise. Same rule as manager-wizard's decoder.
#[cfg(windows)]
fn decode_credential_blob(blob: &[u8]) -> Option<String> {
    if blob.len() >= 2 && blob.len().is_multiple_of(2) {
        let units: Vec<u16> = blob
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| u16::from_le_bytes(*p))
            .collect();
        if let Ok(s) = String::from_utf16(&units) {
            let trimmed = s.trim_end_matches('\0');
            if trimmed.is_ascii() && !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    String::from_utf8(blob.to_vec())
        .ok()
        .map(|s| s.trim_end_matches('\0').to_string())
}

/// A password held as UTF-8 bytes that are overwritten (volatile, so
/// the writes survive optimization) when the value drops. The
/// password for service registration lives only in this buffer
/// between the CredUI dialog and the SCM call.
#[cfg(windows)]
struct SecretString {
    bytes: Vec<u8>,
}

#[cfg(windows)]
impl SecretString {
    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes).unwrap_or("")
    }
}

#[cfg(windows)]
impl Drop for SecretString {
    fn drop(&mut self) {
        for b in self.bytes.iter_mut() {
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

#[cfg(windows)]
enum CredentialPrompt {
    Ok(SecretString),
    Cancelled,
    Failed(String),
}

/// NUL-terminated UTF-16 for a Win32 `W` API.
#[cfg(windows)]
fn wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Read a NUL-terminated UTF-16 buffer as a String (up to the first
/// NUL; empty when the buffer holds nothing).
#[cfg(windows)]
fn utf16_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&u| u == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Prompt for the service account's password with the Windows system
/// credential dialog (`CredUIPromptForCredentialsW`, credui.dll) in
/// this elevated process. The dialog is password-only: the account
/// from the definition is prefilled, and
/// `CREDUI_FLAGS_EXCLUDE_CERTIFICATES` omits the user-name field for
/// a prefilled name, so the user types nothing but the password and
/// cannot register under a different account by editing the name.
/// `DO_NOT_PERSIST` means CredUI stores nothing (no save checkbox);
/// `ALWAYS_SHOW_UI` forces the dialog even if a credential for the
/// target exists. The returned password is moved into a
/// [`SecretString`] and the raw dialog buffer is zeroed immediately;
/// the caller passes it only to SCM and drops it on every path.
#[cfg(windows)]
fn prompt_service_account_password(
    def: &manager_services::ServiceDefinition,
) -> CredentialPrompt {
    use windows_sys::Win32::Foundation::{ERROR_CANCELLED, NO_ERROR};
    use windows_sys::Win32::Security::Credentials::{
        CredUIPromptForCredentialsW, CREDUI_FLAGS_ALWAYS_SHOW_UI, CREDUI_FLAGS_DO_NOT_PERSIST,
        CREDUI_FLAGS_EXCLUDE_CERTIFICATES, CREDUI_FLAGS_GENERIC_CREDENTIALS, CREDUI_INFOW,
        CREDUI_MAX_USERNAME_LENGTH,
    };

    // wincred.h values; windows-sys 0.61 does not export the
    // password-length constant, so it is spelled out here.
    const CREDUI_MAX_PASSWORD_LENGTH: usize = 256;

    let account = def.account.clone().unwrap_or_default();
    let caption = wide_null("LLMdil service registration");
    let message = wide_null(&format!(
        "Enter the Windows password for {account} to register the {} service. \
         LLMdil does not keep this password; Windows stores it as the service's logon credential.",
        def.name
    ));
    let target = wide_null(&def.name);
    let mut info: CREDUI_INFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<CREDUI_INFOW>() as u32;
    // hwndParent stays NULL (no parent window in this helper) and
    // hbmBanner stays NULL: the plain system dialog.
    info.pszMessageText = message.as_ptr();
    info.pszCaptionText = caption.as_ptr();

    let mut username_buf = vec![0u16; CREDUI_MAX_USERNAME_LENGTH as usize + 1];
    for (i, u) in account.encode_utf16().enumerate() {
        if i >= username_buf.len() - 1 {
            break;
        }
        username_buf[i] = u;
    }
    let mut password_buf = vec![0u16; CREDUI_MAX_PASSWORD_LENGTH + 1];
    let mut save: windows_sys::core::BOOL = 0;
    // Win32 argument order: username buffer first, password second.
    let rc = unsafe {
        CredUIPromptForCredentialsW(
            &info,
            target.as_ptr(),
            std::ptr::null(),
            0,
            username_buf.as_mut_ptr(),
            username_buf.len() as u32,
            password_buf.as_mut_ptr(),
            password_buf.len() as u32,
            &mut save,
            CREDUI_FLAGS_GENERIC_CREDENTIALS
                | CREDUI_FLAGS_ALWAYS_SHOW_UI
                | CREDUI_FLAGS_DO_NOT_PERSIST
                | CREDUI_FLAGS_EXCLUDE_CERTIFICATES,
        )
    };
    if rc == ERROR_CANCELLED {
        return CredentialPrompt::Cancelled;
    }
    if rc != NO_ERROR {
        return CredentialPrompt::Failed(format!(
            "credential prompt failed (CredUI error {rc})"
        ));
    }
    let username = utf16_to_string(&username_buf);
    let mut password_units: Vec<u16> = {
        let end = password_buf
            .iter()
            .position(|&u| u == 0)
            .unwrap_or(password_buf.len());
        password_buf[..end].to_vec()
    };
    // Zero the raw dialog buffer before doing anything else with it.
    for u in password_buf.iter_mut() {
        unsafe { std::ptr::write_volatile(u, 0) };
    }
    if username.to_lowercase() != account.to_lowercase() {
        return CredentialPrompt::Failed(format!(
            "credential prompt returned account '{username}', expected '{account}'"
        ));
    }
    let Ok(password) = String::from_utf16(&password_units) else {
        return CredentialPrompt::Failed(
            "credential prompt returned a password that is not valid UTF-16".to_string(),
        );
    };
    // The String now owns the only copy; zero the intermediate units.
    for u in password_units.iter_mut() {
        unsafe { std::ptr::write_volatile(u, 0) };
    }
    if password.is_empty() {
        return CredentialPrompt::Failed(
            "no password was entered in the credential prompt".to_string(),
        );
    }
    CredentialPrompt::Ok(SecretString {
        bytes: password.into_bytes(),
    })
}

#[cfg(windows)]
mod windows_host {
    use super::cred_read;
    use manager_services::{resolve_env, ServiceDefinition};
    use manager_supervisor::lifecycle;
    use std::ffi::c_void;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, HANDLE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_APPEND_DATA, FILE_GENERIC_READ, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_ALWAYS, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Services::{
        RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW,
        SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE,
        SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_START_PENDING, SERVICE_STATUS,
        SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_STOP_PENDING,
        SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
        TerminateJobObject, JobObjectExtendedLimitInformation,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, GetExitCodeProcess, TerminateProcess, WaitForSingleObject, PROCESS_INFORMATION,
        STARTF_USESTDHANDLES, STARTUPINFOW,
    };

    const ERROR_SERVICE_SPECIFIC_ERROR: u32 = 1066;
    const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const INFINITE: u32 = 0xFFFF_FFFF;
    const NO_ERROR: u32 = 0;

    struct HostState {
        def: ServiceDefinition,
        status_handle: SERVICE_STATUS_HANDLE,
        child: HANDLE,
        job: HANDLE,
        state: SERVICE_STATUS_CURRENT_STATE,
        /// Set by the control handler on STOP/SHUTDOWN: the child's exit
        /// that follows is an operator stop (reported as a clean
        /// SERVICE_STOPPED), not a crash.
        stop_requested: bool,
        /// When the child was spawned (uptime in the exit marker).
        child_started: Option<Instant>,
        child_pid: u32,
    }

    /// Append one lifecycle marker for this service to its lifecycle log
    /// (`ServiceDefinition::lifecycle_log`, or the stdout log's
    /// `<stem>.lifecycle.log` sibling for an older definition), in the
    /// shared `[ts] [lifecycle] <key>: <event>: <detail> — by scm` format.
    fn marker(def: &ServiceDefinition, event: &str, detail: Option<&str>) {
        let Some(path) = manager_services::lifecycle_log_path(def) else {
            return;
        };
        let line = lifecycle::marker_line(
            chrono::Utc::now(),
            &manager_services::lifecycle_key(&def.name),
            event,
            detail,
            Some(lifecycle::Origin::Scm),
        );
        lifecycle::append_marker(&path, &line);
    }

    fn def_snapshot() -> Option<ServiceDefinition> {
        HOST.lock().ok()?.as_ref().map(|s| s.def.clone())
    }

    fn uptime_text(d: Duration) -> String {
        let secs = d.as_secs();
        if secs >= 3600 {
            format!("{}h{:02}m{:02}s", secs / 3600, (secs / 60) % 60, secs % 60)
        } else if secs >= 60 {
            format!("{}m{:02}s", secs / 60, secs % 60)
        } else {
            format!("{:.1}s", d.as_secs_f64())
        }
    }

    // The handles are only ever touched under the HOST mutex (or by
    // the SCM on the threads it calls the handler/main on, serialized
    // through the same mutex), so moving the state between threads is
    // sound.
    unsafe impl Send for HostState {}

    static HOST: Mutex<Option<HostState>> = Mutex::new(None);

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn run_service(def: ServiceDefinition) -> u8 {
        let name_w = wide(&def.name);
        {
            let mut guard = HOST.lock().expect("host state");
            *guard = Some(HostState {
                def,
                status_handle: std::ptr::null_mut(),
                child: std::ptr::null_mut(),
                job: std::ptr::null_mut(),
                state: SERVICE_STOPPED,
                stop_requested: false,
                child_started: None,
                child_pid: 0,
            });
        }
        let mut table = [
            SERVICE_TABLE_ENTRYW {
                lpServiceName: name_w.as_ptr() as *mut u16,
                lpServiceProc: Some(service_main),
            },
            SERVICE_TABLE_ENTRYW {
                lpServiceName: std::ptr::null_mut(),
                lpServiceProc: None,
            },
        ];
        let ok = unsafe { StartServiceCtrlDispatcherW(table.as_mut_ptr()) };
        if ok == 0 {
            eprintln!(
                "StartServiceCtrlDispatcherW failed with {} (the host must be started by the SCM)",
                unsafe { GetLastError() }
            );
            return 1;
        }
        0
    }

    extern "system" fn service_main(_argc: u32, _argv: *mut *mut u16) {
        let handle = {
            let guard = HOST.lock().expect("host state");
            let state = guard.as_ref().expect("host state initialized");
            let name_w = wide(&state.def.name);
            unsafe {
                RegisterServiceCtrlHandlerExW(
                    name_w.as_ptr(),
                    Some(control_handler),
                    std::ptr::null_mut(),
                )
            }
        };
        if handle.is_null() {
            return;
        }
        {
            let mut guard = HOST.lock().expect("host state");
            if let Some(state) = guard.as_mut() {
                state.status_handle = handle;
            }
        }
        report(SERVICE_START_PENDING, 0, 0, 30_000);
        if let Some(def) = def_snapshot() {
            marker(
                &def,
                "service start",
                Some(&format!(
                    "llm-service-host v{}, definition {} (format v{}), host pid {}",
                    env!("CARGO_PKG_VERSION"),
                    def.name,
                    def.version,
                    std::process::id()
                )),
            );
        }
        if let Err(code) = start_child() {
            if let Some(def) = def_snapshot() {
                marker(
                    &def,
                    "service stopped",
                    Some(&format!("start failed (service-specific code {code})")),
                );
            }
            report_specific_failure(code);
            return;
        }
        // RUNNING, unless a stop arrived while the child was being
        // spawned (STOP is accepted during START_PENDING): then the job
        // could not be terminated yet — do it now, under the same lock
        // the control handler takes, so the two cannot interleave.
        {
            let mut guard = HOST.lock().expect("host state");
            if let Some(host) = guard.as_mut() {
                if host.stop_requested {
                    terminate_child(host);
                } else {
                    report_locked(
                        host,
                        windows_sys::Win32::System::Services::SERVICE_RUNNING,
                        0,
                        0,
                        0,
                    );
                }
            }
        }
        // Wait for the child; a stop control terminates the job,
        // which ends this wait.
        let child = {
            let guard = HOST.lock().expect("host state");
            guard.as_ref().map(|s| s.child).unwrap_or(std::ptr::null_mut())
        };
        let mut exit_code: u32 = 0;
        let mut exit_known = false;
        if !child.is_null() {
            let wait = unsafe { WaitForSingleObject(child, INFINITE) };
            if wait == WAIT_OBJECT_0 {
                exit_known = unsafe { GetExitCodeProcess(child, &mut exit_code) } != 0;
            }
        }
        let (requested, started, pid, def) = {
            let guard = HOST.lock().expect("host state");
            match guard.as_ref() {
                Some(h) => (h.stop_requested, h.child_started, h.child_pid, Some(h.def.clone())),
                None => (false, None, 0, None),
            }
        };
        if let Some(def) = &def {
            let uptime = started
                .map(|t| uptime_text(t.elapsed()))
                .unwrap_or_else(|| "unknown".to_string());
            let code = if exit_known {
                exit_code.to_string()
            } else {
                "unknown".to_string()
            };
            let why = if requested {
                "stop requested"
            } else {
                "unexpected exit"
            };
            marker(
                def,
                "child exited",
                Some(&format!("pid {pid}, code {code}, uptime {uptime}, {why}")),
            );
        }
        cleanup();
        if requested {
            // An operator stop: a clean stop, whatever code
            // TerminateJobObject left on the child. Never triggers SCM
            // recovery.
            if let Some(def) = &def {
                marker(def, "service stopped", Some("clean stop (exit 0)"));
            }
            report(SERVICE_STOPPED, NO_ERROR, 0, 0);
        } else {
            // The child went away on its own: a failure the SCM (and the
            // app) can tell apart from a stop. With
            // fFailureActionsOnNonCrashFailures set (the log receiver)
            // the SCM runs its restart actions.
            if let Some(def) = &def {
                marker(
                    def,
                    "service stopped",
                    Some(&format!(
                        "child failed (reported service-specific code {exit_code})"
                    )),
                );
            }
            report(SERVICE_STOPPED, ERROR_SERVICE_SPECIFIC_ERROR, exit_code, 0);
        }
    }

    extern "system" fn control_handler(
        control: u32,
        _event_type: u32,
        _event_data: *mut c_void,
        _context: *mut c_void,
    ) -> u32 {
        match control {
            SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
                // Marker first, so it precedes the "child exited" line
                // service_main writes once the kill lands.
                if let Some(def) = def_snapshot() {
                    let detail = if control == SERVICE_CONTROL_SHUTDOWN {
                        "system shutdown"
                    } else {
                        "service control STOP"
                    };
                    marker(&def, "stop requested", Some(detail));
                }
                let mut guard = HOST.lock().expect("host state");
                if let Some(host) = guard.as_mut() {
                    // Before the kill: the exit that follows is a
                    // requested stop, not a crash.
                    host.stop_requested = true;
                    report_locked(host, SERVICE_STOP_PENDING, 0, 0, 15_000);
                    // Under the lock, so cleanup() cannot close the
                    // handles in between. With no child yet (a stop
                    // during START_PENDING) service_main terminates it
                    // once start_child returns.
                    terminate_child(host);
                }
                NO_ERROR
            }
            SERVICE_CONTROL_INTERROGATE => NO_ERROR,
            _ => NO_ERROR,
        }
    }

    /// Kill the child tree: the job object when there is one, else (job
    /// creation failed) the child process itself. Caller holds HOST.
    fn terminate_child(host: &HostState) {
        if !host.job.is_null() {
            unsafe { TerminateJobObject(host.job, 1) };
        } else if !host.child.is_null() {
            unsafe { TerminateProcess(host.child, 1) };
        }
    }

    /// Report a status to the SCM. `specific` is the service-specific
    /// exit code (meaningful with `ERROR_SERVICE_SPECIFIC_ERROR`).
    fn report(state: SERVICE_STATUS_CURRENT_STATE, win32_exit: u32, specific: u32, wait_hint: u32) {
        let mut guard = HOST.lock().expect("host state");
        if let Some(host) = guard.as_mut() {
            report_locked(host, state, win32_exit, specific, wait_hint);
        }
    }

    /// [`report`] with the HOST lock already held. STOP and SHUTDOWN are
    /// accepted while RUNNING and while START_PENDING, so a stop during
    /// a start is honored instead of failing with
    /// ERROR_SERVICE_CANNOT_ACCEPT_CTRL.
    fn report_locked(
        host: &mut HostState,
        state: SERVICE_STATUS_CURRENT_STATE,
        win32_exit: u32,
        specific: u32,
        wait_hint: u32,
    ) {
        host.state = state;
        if host.status_handle.is_null() {
            return;
        }
        let accepted = if state == windows_sys::Win32::System::Services::SERVICE_RUNNING
            || state == SERVICE_START_PENDING
        {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        };
        let status = SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: state,
            dwControlsAccepted: accepted,
            dwWin32ExitCode: win32_exit,
            dwServiceSpecificExitCode: specific,
            dwCheckPoint: 0,
            dwWaitHint: wait_hint,
        };
        unsafe { SetServiceStatus(host.status_handle, &status) };
    }

    /// A failed start: SERVICE_STOPPED with `ERROR_SERVICE_SPECIFIC_ERROR`
    /// and `specific` (1 = env/credential resolution, 2 = spawn).
    fn report_specific_failure(specific: u32) {
        report(SERVICE_STOPPED, ERROR_SERVICE_SPECIFIC_ERROR, specific, 0);
    }

    /// Security attributes marking a handle inheritable. The child's
    /// stdio handles MUST be inheritable: `STARTF_USESTDHANDLES` +
    /// `bInheritHandles = TRUE` only pass handles created with
    /// `bInheritHandle = TRUE`. With a null pointer (as before) the
    /// child received handle values that did not exist in its process,
    /// so every service-mode stdout/stderr log stayed empty, and a later
    /// handle the child opened could alias the dead value and receive
    /// its stdout writes (finding PL-M1, docs/code-review-v0.1.60.md).
    fn inheritable() -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        }
    }

    fn append_handle(path: &Path) -> Option<HANDLE> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let path_w = wide(&path.to_string_lossy());
        let sa = inheritable();
        let handle = unsafe {
            CreateFileW(
                path_w.as_ptr(),
                FILE_APPEND_DATA,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &sa,
                OPEN_ALWAYS,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            None
        } else {
            Some(handle)
        }
    }

    fn nul_handle() -> HANDLE {
        let path_w = wide("NUL");
        let sa = inheritable();
        unsafe {
            CreateFileW(
                path_w.as_ptr(),
                FILE_GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &sa,
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        }
    }

    /// Spawn the definition's child. Returns a service-specific exit
    /// code on failure (1 = env resolution, 2 = spawn).
    fn start_child() -> Result<(), u32> {
        let def = {
            let guard = HOST.lock().expect("host state");
            guard.as_ref().expect("host state").def.clone()
        };
        // One previous run per log, as process mode keeps
        // (`lifecycle::open_rotated_log`): rotate to `.prev` before the
        // handles are opened (OPEN_ALWAYS + append, below). Best-effort:
        // a file someone holds open without delete sharing stays put and
        // this run appends to it.
        let mut rotated: Vec<&Path> = Vec::new();
        for log in [def.stdout_log.as_deref(), def.stderr_log.as_deref()]
            .into_iter()
            .flatten()
        {
            if !rotated.contains(&log) {
                let _ = lifecycle::rotate_log(log);
                rotated.push(log);
            }
        }
        let resolved = resolve_env(&def, cred_read).map_err(|e| {
            marker(&def, "start failed", Some(&format!("environment resolution: {e}")));
            if let Some(stderr_log) = &def.stderr_log {
                let _ = std::fs::create_dir_all(
                    stderr_log.parent().unwrap_or(Path::new(".")),
                );
                let _ = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(stderr_log)
                    .map(|mut f| {
                        use std::io::Write;
                        let _ = writeln!(f, "llm-service-host: {e}");
                    });
            }
            manager_services::HOST_EXIT_ENV_FAILURE
        })?;

        // Environment: the service's own environment overlaid with
        // the definition's entries.
        // `vars_os`, not `vars`: `std::env::vars()` panics on any
        // non-Unicode variable, which would kill the shim before it
        // reports a status (PL-n2). Values are passed through as
        // UTF-16 unchanged.
        use std::os::windows::ffi::OsStrExt;
        let mut env_map: Vec<(std::ffi::OsString, std::ffi::OsString)> =
            std::env::vars_os().collect();
        for (k, v) in resolved {
            if let Some(slot) = env_map
                .iter_mut()
                .find(|(ek, _)| ek.to_string_lossy().eq_ignore_ascii_case(&k))
            {
                slot.1 = v.into();
            } else {
                env_map.push((k.into(), v.into()));
            }
        }
        let mut env_block: Vec<u16> = Vec::new();
        for (k, v) in &env_map {
            env_block.extend(k.encode_wide());
            env_block.push(u16::from(b'='));
            env_block.extend(v.encode_wide());
            env_block.push(0);
        }
        env_block.push(0);

        let stdout = def.stdout_log.as_deref().and_then(append_handle);
        let stderr = def.stderr_log.as_deref().and_then(append_handle);
        let stdin = nul_handle();

        let mut cmdline = manager_services::quote_arg(&def.program.to_string_lossy());
        for arg in &def.args {
            cmdline.push(' ');
            cmdline.push_str(&manager_services::quote_arg(arg));
        }
        let mut cmdline_w = wide(&cmdline);
        let program_w = wide(&def.program.to_string_lossy());
        let workdir_w = def.workdir.as_ref().map(|d| wide(&d.to_string_lossy()));

        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        startup.dwFlags = STARTF_USESTDHANDLES;
        startup.hStdInput = stdin;
        startup.hStdOutput = stdout.unwrap_or(std::ptr::null_mut());
        startup.hStdError = stderr.unwrap_or(std::ptr::null_mut());
        let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            CreateProcessW(
                program_w.as_ptr(),
                cmdline_w.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                1, // inherit the stdio handles
                CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
                env_block.as_ptr() as *const c_void,
                workdir_w
                    .as_ref()
                    .map(|w| w.as_ptr())
                    .unwrap_or(std::ptr::null()),
                &startup,
                &mut process,
            )
        };
        // Read before CloseHandle below can overwrite it.
        let spawn_error = if ok == 0 { unsafe { GetLastError() } } else { 0 };
        // The child holds its own copies of the stdio handles now.
        for h in [stdout, stderr].into_iter().flatten() {
            unsafe { CloseHandle(h) };
        }
        if stdin != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE
            && !stdin.is_null()
        {
            unsafe { CloseHandle(stdin) };
        }
        if ok == 0 {
            let text = std::io::Error::from_raw_os_error(spawn_error as i32);
            let detail = format!(
                "CreateProcessW({}) failed with Windows error {spawn_error}: {text}",
                def.program.display()
            );
            marker(&def, "start failed", Some(&detail));
            if let Some(stderr_log) = &def.stderr_log {
                let _ = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(stderr_log)
                    .map(|mut f| {
                        use std::io::Write;
                        let _ = writeln!(f, "llm-service-host: {detail}");
                    });
            }
            return Err(manager_services::HOST_EXIT_SPAWN_FAILURE);
        }
        unsafe { CloseHandle(process.hThread) };

        // Job object: the child — and anything it spawns — dies with
        // the job, and TerminateJobObject is the stop path.
        let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
        if !job.is_null() {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
                unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *const c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                AssignProcessToJobObject(job, process.hProcess);
            }
        }

        if let Some(pid_file) = &def.pid_file {
            if let Some(parent) = pid_file.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(pid_file, process.dwProcessId.to_string());
        }

        marker(
            &def,
            "child spawned",
            Some(&format!(
                "pid {}, program {}",
                process.dwProcessId,
                def.program.display()
            )),
        );

        let mut guard = HOST.lock().expect("host state");
        if let Some(state) = guard.as_mut() {
            state.child = process.hProcess;
            state.job = job;
            state.child_started = Some(Instant::now());
            state.child_pid = process.dwProcessId;
        }
        Ok(())
    }

    fn cleanup() {
        let (child, job, pid_file) = {
            let mut guard = HOST.lock().expect("host state");
            match guard.as_mut() {
                Some(state) => {
                    let values = (state.child, state.job, state.def.pid_file.clone());
                    state.child = std::ptr::null_mut();
                    state.job = std::ptr::null_mut();
                    values
                }
                None => (std::ptr::null_mut(), std::ptr::null_mut(), None),
            }
        };
        if let Some(pid_file) = pid_file {
            let _ = std::fs::remove_file(pid_file);
        }
        if !child.is_null() {
            unsafe { CloseHandle(child) };
        }
        if !job.is_null() {
            unsafe { CloseHandle(job) };
        }
    }
}
