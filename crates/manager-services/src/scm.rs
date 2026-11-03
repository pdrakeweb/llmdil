//! The [`ServiceControl`] implementations: the real SCM client on
//! Windows, an unsupported stub elsewhere.

use crate::{ServiceControl, ServiceDefinition, ServiceError, ServiceStatus};

/// The platform's service control. Windows: [`WindowsServiceControl`];
/// anything else: the unsupported stub.
#[cfg(windows)]
pub fn platform_control() -> WindowsServiceControl {
    WindowsServiceControl
}

/// The platform's service control (unsupported stub off Windows).
#[cfg(not(windows))]
pub fn platform_control() -> UnsupportedServiceControl {
    UnsupportedServiceControl
}

/// Stub used on non-Windows platforms so the workspace builds and the
/// app's service-mode paths fail with a clear error instead of
/// silently falling back to process supervision.
pub struct UnsupportedServiceControl;

impl ServiceControl for UnsupportedServiceControl {
    fn query(&self, _name: &str) -> Result<ServiceStatus, ServiceError> {
        Err(ServiceError::Unsupported(
            "service supervision requires Windows".to_string(),
        ))
    }

    fn start(&self, _name: &str) -> Result<(), ServiceError> {
        Err(ServiceError::Unsupported(
            "service supervision requires Windows".to_string(),
        ))
    }

    fn stop(&self, _name: &str) -> Result<(), ServiceError> {
        Err(ServiceError::Unsupported(
            "service supervision requires Windows".to_string(),
        ))
    }

    fn register(
        &self,
        _def: &ServiceDefinition,
        _image_path: &str,
        _password: Option<&str>,
    ) -> Result<(), ServiceError> {
        Err(ServiceError::Unsupported(
            "service supervision requires Windows".to_string(),
        ))
    }

    fn unregister(&self, _name: &str) -> Result<(), ServiceError> {
        Err(ServiceError::Unsupported(
            "service supervision requires Windows".to_string(),
        ))
    }
}

#[cfg(windows)]
pub use windows_impl::WindowsServiceControl;

/// The interactive user's account name (`DOMAIN\user`) — the default
/// service account at registration (design note §4b). Windows only;
/// `None` elsewhere or on any lookup failure.
#[cfg(windows)]
pub fn current_user_account() -> Option<String> {
    windows_impl::current_user_account()
}

/// See the Windows version.
#[cfg(not(windows))]
pub fn current_user_account() -> Option<String> {
    None
}

/// The interactive user's SID in string form — embedded in the
/// service DACL at registration so day-to-day start/stop/query run
/// unelevated. Windows only.
#[cfg(windows)]
pub fn current_user_sid() -> Option<String> {
    windows_impl::current_user_sid()
}

/// See the Windows version.
#[cfg(not(windows))]
pub fn current_user_sid() -> Option<String> {
    None
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use crate::{ServiceStartMode, ServiceState};
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, LocalFree, ERROR_SERVICE_ALREADY_RUNNING,
        ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_NOT_ACTIVE, HANDLE,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, LookupAccountSidW, TokenUser, DACL_SECURITY_INFORMATION,
        TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::Services::{
        ChangeServiceConfig2W, CloseServiceHandle, ControlService, CreateServiceW,
        DeleteService, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx, SetServiceObjectSecurity,
        StartServiceW, SC_ACTION, SC_ACTION_RESTART, SC_HANDLE, SC_MANAGER_CREATE_SERVICE,
        SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CHANGE_CONFIG, SERVICE_CONFIG_DESCRIPTION,
        SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONTROL_STOP, SERVICE_DEMAND_START,
        SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW, SERVICE_PAUSED,
        SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START, SERVICE_START_PENDING,
        SERVICE_STATUS, SERVICE_STATUS_PROCESS, SERVICE_STOP, SERVICE_STOPPED,
        SERVICE_STOP_PENDING, SERVICE_WIN32_OWN_PROCESS, SC_STATUS_PROCESS_INFO,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// The real SCM client. Stateless: every operation opens the
    /// handles it needs (the SCM caches its own state; open/close per
    /// call keeps this free of handle-lifetime bugs at dashboard poll
    /// rates).
    pub struct WindowsServiceControl;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn scm_error(operation: &'static str) -> ServiceError {
        ServiceError::Scm {
            operation,
            code: unsafe { GetLastError() },
        }
    }

    struct ScHandle(SC_HANDLE);

    impl Drop for ScHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CloseServiceHandle(self.0) };
            }
        }
    }

    fn open_manager(access: u32) -> Result<ScHandle, ServiceError> {
        let handle = unsafe {
            OpenSCManagerW(std::ptr::null(), std::ptr::null(), access)
        };
        if handle.is_null() {
            return Err(scm_error("OpenSCManagerW"));
        }
        Ok(ScHandle(handle))
    }

    fn open_service(
        manager: &ScHandle,
        name: &str,
        access: u32,
    ) -> Result<Option<ScHandle>, ServiceError> {
        let name_w = wide(name);
        let handle = unsafe { OpenServiceW(manager.0, name_w.as_ptr(), access) };
        if handle.is_null() {
            if unsafe { GetLastError() } == ERROR_SERVICE_DOES_NOT_EXIST {
                return Ok(None);
            }
            return Err(scm_error("OpenServiceW"));
        }
        Ok(Some(ScHandle(handle)))
    }

    fn query_status(handle: &ScHandle, name: &str) -> Result<ServiceStatus, ServiceError> {
        let mut status: SERVICE_STATUS_PROCESS = unsafe { std::mem::zeroed() };
        let mut needed: u32 = 0;
        let ok = unsafe {
            QueryServiceStatusEx(
                handle.0,
                SC_STATUS_PROCESS_INFO,
                &mut status as *mut SERVICE_STATUS_PROCESS as *mut u8,
                std::mem::size_of::<SERVICE_STATUS_PROCESS>() as u32,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(scm_error("QueryServiceStatusEx"));
        }
        let state = match status.dwCurrentState {
            SERVICE_STOPPED => ServiceState::Stopped,
            SERVICE_START_PENDING => ServiceState::StartPending,
            SERVICE_STOP_PENDING => ServiceState::StopPending,
            SERVICE_RUNNING => ServiceState::Running,
            SERVICE_PAUSED => ServiceState::Paused,
            other => ServiceState::Unknown(other),
        };
        Ok(ServiceStatus {
            name: name.to_string(),
            state,
            pid: if status.dwProcessId != 0 {
                Some(status.dwProcessId)
            } else {
                None
            },
            child_pid: None,
            win32_exit_code: status.dwWin32ExitCode,
        })
    }

    /// TokenUser SID of the calling process as a string, and the
    /// account name it resolves to. In the elevated helper the token
    /// belongs to the same user with the admin group enabled, so the
    /// SID is the operator's own.
    fn token_user() -> Option<(String, String)> {
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return None;
            }
            let mut needed: u32 = 0;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
            if needed == 0 {
                CloseHandle(token);
                return None;
            }
            let mut buffer = vec![0u8; needed as usize];
            let ok = GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr() as *mut c_void,
                needed,
                &mut needed,
            );
            if ok == 0 {
                CloseHandle(token);
                return None;
            }
            let sid = (*(buffer.as_ptr() as *const TOKEN_USER)).User.Sid;
            let mut sid_str: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(sid, &mut sid_str) == 0 {
                CloseHandle(token);
                return None;
            }
            let sid_string = {
                let mut len = 0usize;
                while *sid_str.add(len) != 0 {
                    len += 1;
                }
                String::from_utf16_lossy(std::slice::from_raw_parts(sid_str, len))
            };
            LocalFree(sid_str as *mut c_void);
            // Account name: LookupAccountSidW with sized buffers.
            let mut name = vec![0u16; 256];
            let mut name_len = name.len() as u32;
            let mut domain = vec![0u16; 256];
            let mut domain_len = domain.len() as u32;
            let mut use_type = 0i32;
            let ok = LookupAccountSidW(
                std::ptr::null(),
                sid,
                name.as_mut_ptr(),
                &mut name_len,
                domain.as_mut_ptr(),
                &mut domain_len,
                &mut use_type,
            );
            CloseHandle(token);
            if ok == 0 {
                return None;
            }
            let take = |buf: &[u16], len: u32| {
                String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())])
            };
            Some((sid_string, format!("{}\\{}", take(&domain, domain_len), take(&name, name_len))))
        }
    }

    pub fn current_user_sid() -> Option<String> {
        token_user().map(|(sid, _)| sid)
    }

    pub fn current_user_account() -> Option<String> {
        token_user().map(|(_, account)| account)
    }

    impl ServiceControl for WindowsServiceControl {
        fn query(&self, name: &str) -> Result<ServiceStatus, ServiceError> {
            let manager = open_manager(windows_sys::Win32::System::Services::SC_MANAGER_CONNECT)?;
            let Some(handle) = open_service(&manager, name, SERVICE_QUERY_STATUS)? else {
                return Ok(ServiceStatus {
                    name: name.to_string(),
                    state: ServiceState::NotInstalled,
                    pid: None,
                    child_pid: None,
                    win32_exit_code: 0,
                });
            };
            query_status(&handle, name)
        }

        fn start(&self, name: &str) -> Result<(), ServiceError> {
            let manager = open_manager(windows_sys::Win32::System::Services::SC_MANAGER_CONNECT)?;
            let handle = open_service(&manager, name, SERVICE_START | SERVICE_QUERY_STATUS)?
                .ok_or_else(|| ServiceError::NotRegistered(name.to_string()))?;
            let ok = unsafe { StartServiceW(handle.0, 0, std::ptr::null()) };
            if ok == 0 {
                let code = unsafe { GetLastError() };
                if code != ERROR_SERVICE_ALREADY_RUNNING {
                    return Err(ServiceError::Scm {
                        operation: "StartServiceW",
                        code,
                    });
                }
            }
            Ok(())
        }

        fn stop(&self, name: &str) -> Result<(), ServiceError> {
            let manager = open_manager(windows_sys::Win32::System::Services::SC_MANAGER_CONNECT)?;
            let handle = open_service(&manager, name, SERVICE_STOP | SERVICE_QUERY_STATUS)?
                .ok_or_else(|| ServiceError::NotRegistered(name.to_string()))?;
            let mut status: SERVICE_STATUS = unsafe { std::mem::zeroed() };
            let ok = unsafe { ControlService(handle.0, SERVICE_CONTROL_STOP, &mut status) };
            if ok == 0 {
                let code = unsafe { GetLastError() };
                if code != ERROR_SERVICE_NOT_ACTIVE {
                    return Err(ServiceError::Scm {
                        operation: "ControlService",
                        code,
                    });
                }
            }
            Ok(())
        }

        fn register(
            &self,
            def: &ServiceDefinition,
            image_path: &str,
            password: Option<&str>,
        ) -> Result<(), ServiceError> {
            let manager = open_manager(SC_MANAGER_CREATE_SERVICE)?;
            let name_w = wide(&def.name);
            let display_w = wide(&def.display_name);
            let image_w = wide(image_path);
            let account_w = def.account.as_deref().map(wide);
            let password_w = password.map(wide);
            let start_type = match def.start_mode {
                ServiceStartMode::Manual => SERVICE_DEMAND_START,
                ServiceStartMode::Automatic => SERVICE_AUTO_START,
            };
            let handle = unsafe {
                CreateServiceW(
                    manager.0,
                    name_w.as_ptr(),
                    display_w.as_ptr(),
                    SERVICE_ALL_ACCESS,
                    SERVICE_WIN32_OWN_PROCESS,
                    start_type,
                    SERVICE_ERROR_NORMAL,
                    image_w.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    account_w
                        .as_ref()
                        .map(|w| w.as_ptr())
                        .unwrap_or(std::ptr::null()),
                    password_w
                        .as_ref()
                        .map(|w| w.as_ptr())
                        .unwrap_or(std::ptr::null()),
                )
            };
            if handle.is_null() {
                return Err(scm_error("CreateServiceW"));
            }
            let handle = ScHandle(handle);

            // Description.
            let mut desc_text = wide(&def.description);
            let mut description = SERVICE_DESCRIPTIONW {
                lpDescription: desc_text.as_mut_ptr(),
            };
            let ok = unsafe {
                ChangeServiceConfig2W(
                    handle.0,
                    SERVICE_CONFIG_DESCRIPTION,
                    &mut description as *mut SERVICE_DESCRIPTIONW as *mut c_void,
                )
            };
            if ok == 0 {
                return Err(scm_error("ChangeServiceConfig2W(description)"));
            }

            // Failure actions: restart with the definition's delays.
            let fa = def.failure_actions;
            let mut actions = [
                SC_ACTION { Type: SC_ACTION_RESTART, Delay: fa.first_delay_secs * 1000 },
                SC_ACTION { Type: SC_ACTION_RESTART, Delay: fa.second_delay_secs * 1000 },
                SC_ACTION { Type: SC_ACTION_RESTART, Delay: fa.subsequent_delay_secs * 1000 },
            ];
            let mut failure = SERVICE_FAILURE_ACTIONSW {
                dwResetPeriod: fa.reset_period_secs,
                lpRebootMsg: std::ptr::null_mut(),
                lpCommand: std::ptr::null_mut(),
                cActions: actions.len() as u32,
                lpsaActions: actions.as_mut_ptr(),
            };
            let ok = unsafe {
                ChangeServiceConfig2W(
                    handle.0,
                    SERVICE_CONFIG_FAILURE_ACTIONS,
                    &mut failure as *mut SERVICE_FAILURE_ACTIONSW as *mut c_void,
                )
            };
            if ok == 0 {
                return Err(scm_error("ChangeServiceConfig2W(failure actions)"));
            }

            // DACL: SYSTEM full, Administrators full, the operator's
            // own SID query/start/stop — day-to-day control without
            // elevation; create/delete/change stay admin-only.
            if let Some(sid) = current_user_sid() {
                let sddl = format!(
                    "D:(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;SY)\
                     (A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)\
                     (A;;CCLCSWRPWPDTLOCRRC;;;{sid})"
                );
                let sddl_w = wide(&sddl);
                let mut descriptor: *mut c_void = std::ptr::null_mut();
                let ok = unsafe {
                    ConvertStringSecurityDescriptorToSecurityDescriptorW(
                        sddl_w.as_ptr(),
                        SDDL_REVISION_1,
                        &mut descriptor,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 {
                    return Err(scm_error("ConvertStringSecurityDescriptorToSecurityDescriptorW"));
                }
                let ok = unsafe {
                    SetServiceObjectSecurity(handle.0, DACL_SECURITY_INFORMATION, descriptor)
                };
                unsafe { LocalFree(descriptor) };
                if ok == 0 {
                    return Err(scm_error("SetServiceObjectSecurity"));
                }
            }
            Ok(())
        }

        fn unregister(&self, name: &str) -> Result<(), ServiceError> {
            let manager = open_manager(windows_sys::Win32::System::Services::SC_MANAGER_CONNECT)?;
            let handle = open_service(
                &manager,
                name,
                SERVICE_STOP | SERVICE_QUERY_STATUS | SERVICE_CHANGE_CONFIG | 0x0001_0000,
            )?
            .ok_or_else(|| ServiceError::NotRegistered(name.to_string()))?;
            // Best-effort stop first: a running service is only marked
            // for deletion and lingers until it stops.
            let _ = self.stop(name);
            let ok = unsafe { DeleteService(handle.0) };
            if ok == 0 {
                return Err(scm_error("DeleteService"));
            }
            Ok(())
        }
    }
}
