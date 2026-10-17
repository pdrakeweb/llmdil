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
//!   (`runas`). `register` reads the account password from the
//!   transient credential the app wrote for this one operation and
//!   deletes that credential before exiting; the durable copy is the
//!   SCM's LSA secret.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The transient password-handoff credential target (defined in the
/// library so the app and this helper share one spelling).
#[cfg(windows)]
use manager_services::SERVICE_ACCOUNT_PASSWORD_CRED_TARGET;

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
        let password = cred_read(SERVICE_ACCOUNT_PASSWORD_CRED_TARGET);
        // The transient credential is single-use: remove it whether
        // or not registration succeeds.
        cred_delete(SERVICE_ACCOUNT_PASSWORD_CRED_TARGET);
        if password.is_none() {
            return fail(format!(
                "account password credential '{SERVICE_ACCOUNT_PASSWORD_CRED_TARGET}' not found; \
                 enter the account password in Settings before registering"
            ));
        }
        let host_exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => return fail(format!("could not locate llm-service-host.exe: {e}")),
        };
        let image_path = def.image_path(&host_exe, &def_path);
        match manager_services::scm::platform_control().register(&def, &image_path, password.as_deref()) {
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
            .chunks_exact(2)
            .map(|p| u16::from_le_bytes([p[0], p[1]]))
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

#[cfg(windows)]
fn cred_delete(target: &str) {
    use windows_sys::Win32::Security::Credentials::{CredDeleteW, CRED_TYPE_GENERIC};
    let wide: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { CredDeleteW(wide.as_ptr(), CRED_TYPE_GENERIC, 0) };
}

#[cfg(windows)]
mod windows_host {
    use super::cred_read;
    use manager_services::{resolve_env, ServiceDefinition};
    use std::ffi::c_void;
    use std::path::Path;
    use std::sync::Mutex;
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
        CreateProcessW, GetExitCodeProcess, WaitForSingleObject, PROCESS_INFORMATION,
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
        report(SERVICE_START_PENDING, 0, 30_000);
        if let Err(code) = start_child() {
            report_specific_failure(code);
            return;
        }
        report(
            windows_sys::Win32::System::Services::SERVICE_RUNNING,
            0,
            0,
        );
        // Wait for the child; a stop control terminates the job,
        // which ends this wait.
        let child = {
            let guard = HOST.lock().expect("host state");
            guard.as_ref().map(|s| s.child).unwrap_or(std::ptr::null_mut())
        };
        let mut exit_code: u32 = 0;
        if !child.is_null() {
            let wait = unsafe { WaitForSingleObject(child, INFINITE) };
            if wait == WAIT_OBJECT_0 {
                unsafe { GetExitCodeProcess(child, &mut exit_code) };
            }
        }
        cleanup();
        report(SERVICE_STOPPED, exit_code, 0);
    }

    extern "system" fn control_handler(
        control: u32,
        _event_type: u32,
        _event_data: *mut c_void,
        _context: *mut c_void,
    ) -> u32 {
        match control {
            SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
                report(SERVICE_STOP_PENDING, 0, 15_000);
                let job = {
                    let guard = HOST.lock().expect("host state");
                    guard.as_ref().map(|s| s.job).unwrap_or(std::ptr::null_mut())
                };
                if !job.is_null() {
                    unsafe { TerminateJobObject(job, 1) };
                }
                NO_ERROR
            }
            SERVICE_CONTROL_INTERROGATE => NO_ERROR,
            _ => NO_ERROR,
        }
    }

    fn report(state: SERVICE_STATUS_CURRENT_STATE, win32_exit: u32, wait_hint: u32) {
        let (handle, accepted) = {
            let mut guard = HOST.lock().expect("host state");
            let Some(host) = guard.as_mut() else { return };
            host.state = state;
            let accepted = if state
                == windows_sys::Win32::System::Services::SERVICE_RUNNING
            {
                SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
            } else {
                0
            };
            (host.status_handle, accepted)
        };
        if handle.is_null() {
            return;
        }
        let mut status = SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: state,
            dwControlsAccepted: accepted,
            dwWin32ExitCode: win32_exit,
            dwServiceSpecificExitCode: 0,
            dwCheckPoint: 0,
            dwWaitHint: wait_hint,
        };
        unsafe { SetServiceStatus(handle, &mut status) };
    }

    fn report_specific_failure(specific: u32) {
        let handle = {
            let guard = HOST.lock().expect("host state");
            guard.as_ref().map(|s| s.status_handle).unwrap_or(std::ptr::null_mut())
        };
        if handle.is_null() {
            return;
        }
        let mut status = SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: SERVICE_STOPPED,
            dwControlsAccepted: 0,
            dwWin32ExitCode: ERROR_SERVICE_SPECIFIC_ERROR,
            dwServiceSpecificExitCode: specific,
            dwCheckPoint: 0,
            dwWaitHint: 0,
        };
        unsafe { SetServiceStatus(handle, &mut status) };
    }

    fn append_handle(path: &Path) -> Option<HANDLE> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let path_w = wide(&path.to_string_lossy());
        let handle = unsafe {
            CreateFileW(
                path_w.as_ptr(),
                FILE_APPEND_DATA,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null_mut::<SECURITY_ATTRIBUTES>(),
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
        unsafe {
            CreateFileW(
                path_w.as_ptr(),
                FILE_GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null_mut::<SECURITY_ATTRIBUTES>(),
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
        let resolved = resolve_env(&def, cred_read).map_err(|e| {
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
            1u32
        })?;

        // Environment: the service's own environment overlaid with
        // the definition's entries.
        let mut env_map: Vec<(String, String)> = std::env::vars().collect();
        for (k, v) in resolved {
            if let Some(slot) = env_map.iter_mut().find(|(ek, _)| ek.eq_ignore_ascii_case(&k)) {
                slot.1 = v;
            } else {
                env_map.push((k, v));
            }
        }
        let mut env_block: Vec<u16> = Vec::new();
        for (k, v) in &env_map {
            env_block.extend(format!("{k}={v}").encode_utf16());
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
        // The child holds its own copies of the stdio handles now.
        for h in [stdout, stderr] {
            if let Some(h) = h {
                unsafe { CloseHandle(h) };
            }
        }
        if stdin != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE
            && !stdin.is_null()
        {
            unsafe { CloseHandle(stdin) };
        }
        if ok == 0 {
            return Err(2);
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

        let mut guard = HOST.lock().expect("host state");
        if let Some(state) = guard.as_mut() {
            state.child = process.hProcess;
            state.job = job;
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
