//! Windows-only helpers: elevated launch, registry lookup, VS Code CLI discovery, and Credential Manager access.

use super::*;

/// Shared GET implementation behind the trait's plain and
/// header-carrying variants.
/// Quote one argument for a ShellExecuteExW parameter string: wrap in
/// double quotes when it contains whitespace or a quote, escaping
/// embedded quotes. Good enough for the paths and flags the wizard
/// launches elevated (a .cmd path and subcommand words).
#[cfg(windows)]
fn shell_quote_arg(arg: &str) -> String {
    if arg.is_empty() {
        return "\"\"".to_string();
    }
    if !arg.chars().any(|c| c.is_whitespace() || c == '"') {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
        if c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Launch `cmd` elevated via ShellExecuteExW with the "runas" verb
/// (one UAC prompt), wait for the process to exit, and return its exit
/// code. The user declining the prompt makes ShellExecuteExW fail,
/// which surfaces as an IoError naming the program.
#[cfg(windows)]
pub(crate) fn run_elevated_windows(cmd: &PlannedCommand) -> Result<CommandOutput, WizardError> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let program = cmd.argv.first().cloned().unwrap_or_default();
    let params = cmd
        .argv
        .iter()
        .skip(1)
        .map(|a| shell_quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ");
    let verb: Vec<u16> = "runas\0".encode_utf16().collect();
    let file: Vec<u16> = program.encode_utf16().chain(std::iter::once(0)).collect();
    let params_w: Vec<u16> = params.encode_utf16().chain(std::iter::once(0)).collect();
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params_w.as_ptr();
    info.nShow = SW_HIDE;
    let ok = unsafe { ShellExecuteExW(&mut info) };
    if ok == 0 {
        let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        return Err(WizardError::IoError {
            path: PathBuf::from(&program),
            reason: format!("elevation failed or was declined (ShellExecuteEx error {code})"),
        });
    }
    // Bounded wait (this was INFINITE): a stalled elevated script
    // must not hang the calling step for the rest of the session.
    // WAIT_TIMEOUT (0x102) means the budget expired; the handle is
    // closed and an error returned, and the elevated process is
    // left to finish — the only caller (the gateway firewall rule)
    // treats this failure as warning-only.
    const WAIT_TIMEOUT_CODE: u32 = 0x0000_0102;
    const WAIT_OBJECT_0: u32 = 0;
    let wait_result = unsafe { WaitForSingleObject(info.hProcess, ELEVATED_WAIT_TIMEOUT_MS) };
    if wait_result == WAIT_TIMEOUT_CODE {
        unsafe { CloseHandle(info.hProcess) };
        return Err(WizardError::IoError {
            path: PathBuf::from(&program),
            reason: format!(
                "elevated command did not finish within {} seconds",
                ELEVATED_WAIT_TIMEOUT_MS / 1000
            ),
        });
    }
    // Anything other than "signaled" (WAIT_FAILED, an abandoned
    // mutex, ...) means the wait itself broke down; falling through
    // used to read an uninitialized exit code and report success.
    if wait_result != WAIT_OBJECT_0 {
        unsafe { CloseHandle(info.hProcess) };
        return Err(WizardError::IoError {
            path: PathBuf::from(&program),
            reason: format!(
                "waiting for the elevated command failed (wait result {wait_result:#x})"
            ),
        });
    }
    let mut exit_code: u32 = 0;
    let got = unsafe { GetExitCodeProcess(info.hProcess, &mut exit_code) };
    unsafe { CloseHandle(info.hProcess) };
    if got == 0 {
        return Err(WizardError::IoError {
            path: PathBuf::from(&program),
            reason: "could not read the elevated command's exit code".to_string(),
        });
    }
    Ok(CommandOutput {
        exit_code: exit_code as i32,
        stdout: String::new(),
        stderr: String::new(),
    })
}

/// Read a registry key's (Default) string value; `None` on any
/// failure (missing key/value, non-string value).
#[cfg(windows)]
fn reg_default_string(
    root: windows_sys::Win32::System::Registry::HKEY,
    subkey: &str,
) -> Option<String> {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, KEY_READ, REG_EXPAND_SZ, REG_SZ,
    };
    let subkey_w: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
    let mut key: windows_sys::Win32::System::Registry::HKEY = std::ptr::null_mut();
    let rc = unsafe { RegOpenKeyExW(root, subkey_w.as_ptr(), 0, KEY_READ, &mut key) };
    if rc != 0 {
        return None;
    }
    let mut buf = vec![0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    let mut ty: u32 = 0;
    let rc = unsafe {
        RegQueryValueExW(
            key,
            std::ptr::null(),
            std::ptr::null_mut(),
            &mut ty,
            buf.as_mut_ptr() as *mut u8,
            &mut len,
        )
    };
    unsafe { RegCloseKey(key) };
    if rc != 0 || (ty != REG_SZ && ty != REG_EXPAND_SZ) {
        return None;
    }
    buf.truncate((len as usize / 2).min(buf.len()));
    let s = String::from_utf16_lossy(&buf);
    let s = s.trim().trim_matches('"').trim_end_matches('\0').trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Locate the VS Code CLI without relying on this process's PATH.
///
/// A GUI process inherits the environment of whatever launched it,
/// which may predate the VS Code install (or the PATH edit its
/// installer made), so spawning bare `code` can fail on a machine
/// where typing `code` in PowerShell works. VS Code's installer
/// registers the editor under the App Paths registry key — the
/// Windows mechanism for exactly this lookup — and the CLI lives in
/// the `bin` directory of the install root. Standard install
/// locations (system and per-user installers, stable and Insiders)
/// are the fallback.
#[cfg(windows)]
pub(crate) fn find_vscode_cli_windows() -> Option<String> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        if let Some(exe) = reg_default_string(
            root,
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\Code.exe",
        ) {
            if let Some(install_dir) = PathBuf::from(exe).parent() {
                let cli = install_dir.join("bin").join("code.cmd");
                if cli.exists() {
                    return Some(cli.display().to_string());
                }
            }
        }
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        roots.push(PathBuf::from(pf));
    }
    if let Some(la) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(la).join("Programs"));
    }
    for root in &roots {
        for (dir, cli_name) in [
            ("Microsoft VS Code", "code.cmd"),
            ("Microsoft VS Code Insiders", "code-insiders.cmd"),
        ] {
            let cli = root.join(dir).join("bin").join(cli_name);
            if cli.exists() {
                return Some(cli.display().to_string());
            }
        }
    }
    None
}

/// Read a generic credential's blob through the Credential Manager
/// [`SecretStore`](crate::secrets::SecretStore) seam (CredReadW) and
/// decode it as text. Returns `None` on any failure (missing target,
/// empty blob, undecodable content). The returned value is a secret: it
/// is never logged and never written to disk by callers — it goes
/// straight into a spawned process's environment.
#[cfg(windows)]
pub(crate) fn cred_read_windows(target: &str) -> Option<String> {
    use crate::secrets::{CredentialManagerSecretStore, SecretStore};
    let blob = CredentialManagerSecretStore.read(target).ok().flatten()?;
    decode_credential_blob(&blob).filter(|v| !v.is_empty())
}

/// Store a generic credential through the Credential Manager
/// [`SecretStore`](crate::secrets::SecretStore) seam (CredWriteW),
/// replacing any existing value for the target. The blob is written as
/// UTF-16LE text — the encoding cmdkey / PowerShell-written secrets
/// use and the one [`cred_read_windows`] decodes natively. Persisted
/// per-user on this machine (CRED_PERSIST_LOCAL_MACHINE).
#[cfg(windows)]
pub(crate) fn cred_write_windows(target: &str, secret: &str) -> Option<()> {
    use crate::secrets::{CredentialManagerSecretStore, SecretStore};
    let blob: Vec<u8> = secret
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    CredentialManagerSecretStore.write(target, &blob).ok()
}

/// Decode a credential blob: generic credentials written by cmdkey /
/// PowerShell hold the secret as UTF-16LE text; anything else is read as
/// UTF-8. Trailing NULs are trimmed either way.
///
/// The UTF-16LE reading is accepted outright only when every decoded
/// char is ASCII: `String::from_utf16` succeeds for most arbitrary byte
/// pairs, so an even-length UTF-8 blob would otherwise "decode" into
/// garbage codepoints and be mistaken for the secret. When the UTF-16
/// reading is not plain ASCII, a valid UTF-8 reading wins; the UTF-16
/// reading remains the fallback (genuine UTF-16LE secrets containing
/// non-ASCII chars are not valid UTF-8 as bytes, so they land there).
#[cfg(any(windows, test))]
pub(crate) fn decode_credential_blob(blob: &[u8]) -> Option<String> {
    let utf16 = if blob.len() >= 2 && blob.len().is_multiple_of(2) {
        let units: Vec<u16> = blob
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| u16::from_le_bytes(*p))
            .collect();
        String::from_utf16(&units)
            .ok()
            .map(|s| s.trim_end_matches('\0').to_string())
    } else {
        None
    };
    if let Some(s) = &utf16 {
        if !s.is_empty() && s.is_ascii() {
            return Some(s.clone());
        }
    }
    if let Ok(s) = std::str::from_utf8(blob) {
        let s = s.trim_end_matches('\0').to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    if let Some(s) = utf16 {
        if !s.is_empty() {
            return Some(s);
        }
    }
    let s = String::from_utf8_lossy(blob)
        .trim_end_matches('\0')
        .to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}
