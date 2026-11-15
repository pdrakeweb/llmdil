//! `SecretStore`: the platform seam for secrets at rest (T-06).
//!
//! A byte-oriented read/write/delete interface over the OS secret
//! store. On Windows that is Credential Manager generic credentials
//! ([`CredentialManagerSecretStore`]: `CredReadW` / `CredWriteW` /
//! `CredDeleteW`, per-user, `CRED_PERSIST_LOCAL_MACHINE`, encrypted at
//! rest by the OS under the user's logon credentials). Elsewhere there
//! is no backend yet: [`UnsupportedSecretStore`] answers every call
//! with [`SecretStoreError::NotSupported`]. [`MemorySecretStore`] is
//! the in-memory implementation for tests.
//!
//! The trait lives here rather than in `manager-api` (where T-06
//! sketched it) because this crate owns the Credential Manager FFI and
//! `manager-api` depends on it, not the other way round;
//! `manager_api::secrets` re-exports this module.
//!
//! Values are opaque bytes. Text encodings (the UTF-16LE convention of
//! cmdkey / PowerShell-written secrets) are the caller's concern: the
//! wizard's `credential_read` / `credential_write` encode on top of
//! this seam, the Admin API's TLS key is stored as UTF-8 PEM.

use std::collections::HashMap;
use std::sync::Mutex;

/// Largest value Credential Manager accepts for a generic credential
/// (`CRED_MAX_CREDENTIAL_BLOB_SIZE`, 5 * 512 bytes).
pub const CRED_MAX_CREDENTIAL_BLOB_SIZE: usize = 5 * 512;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SecretStoreError {
    /// No secret store on this platform.
    #[error("secret storage is not supported on this platform")]
    NotSupported,
    /// The value exceeds the backend's size limit.
    #[error("secret is {len} bytes; the store holds at most {max}")]
    TooLarge { len: usize, max: usize },
    /// The backend refused or failed the operation. Never contains the
    /// secret value.
    #[error("secret store error: {0}")]
    Storage(String),
}

/// Custody for secrets at rest. Implementations must never log values.
pub trait SecretStore: Send + Sync {
    /// The value stored under `key`; `Ok(None)` when there is none.
    fn read(&self, key: &str) -> Result<Option<Vec<u8>>, SecretStoreError>;
    /// Store `value` under `key`, replacing any existing value.
    fn write(&self, key: &str, value: &[u8]) -> Result<(), SecretStoreError>;
    /// Remove the value under `key`. Removing an absent key succeeds.
    fn delete(&self, key: &str) -> Result<(), SecretStoreError>;
}

/// In-memory store (tests, and callers that must not touch the OS
/// store).
#[derive(Default)]
pub struct MemorySecretStore {
    map: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemorySecretStore {
    /// Whether anything is stored under `key` (test assertions).
    pub fn contains(&self, key: &str) -> bool {
        self.map.lock().unwrap().contains_key(key)
    }

    /// Number of stored entries (test assertions).
    pub fn len(&self) -> usize {
        self.map.lock().unwrap().len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl SecretStore for MemorySecretStore {
    fn read(&self, key: &str) -> Result<Option<Vec<u8>>, SecretStoreError> {
        Ok(self.map.lock().unwrap().get(key).cloned())
    }
    fn write(&self, key: &str, value: &[u8]) -> Result<(), SecretStoreError> {
        self.map
            .lock()
            .unwrap()
            .insert(key.to_string(), value.to_vec());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), SecretStoreError> {
        self.map.lock().unwrap().remove(key);
        Ok(())
    }
}

/// The store for platforms without a backend: every call fails with
/// [`SecretStoreError::NotSupported`] (graceful degradation, no panic).
pub struct UnsupportedSecretStore;

impl SecretStore for UnsupportedSecretStore {
    fn read(&self, _key: &str) -> Result<Option<Vec<u8>>, SecretStoreError> {
        Err(SecretStoreError::NotSupported)
    }
    fn write(&self, _key: &str, _value: &[u8]) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::NotSupported)
    }
    fn delete(&self, _key: &str) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::NotSupported)
    }
}

/// Windows Credential Manager, generic credentials for the current
/// user, persisted on this machine only (`CRED_PERSIST_LOCAL_MACHINE`:
/// not roamed with the profile).
#[cfg(windows)]
pub struct CredentialManagerSecretStore;

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
impl SecretStore for CredentialManagerSecretStore {
    fn read(&self, key: &str) -> Result<Option<Vec<u8>>, SecretStoreError> {
        use windows_sys::Win32::Foundation::{GetLastError, ERROR_NOT_FOUND};
        use windows_sys::Win32::Security::Credentials::{
            CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC,
        };
        let target = wide(key);
        let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: `target` is NUL-terminated and outlives the call;
        // `cred` receives an allocation freed below.
        let ok = unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut cred) };
        if ok == 0 || cred.is_null() {
            // SAFETY: plain thread-local error read.
            let code = unsafe { GetLastError() };
            if code == ERROR_NOT_FOUND {
                return Ok(None);
            }
            return Err(SecretStoreError::Storage(format!(
                "CredReadW failed (error {code})"
            )));
        }
        // SAFETY: `cred` is valid per the successful CredReadW above;
        // the blob pointer/size pair describes its value.
        let value = unsafe {
            let size = (*cred).CredentialBlobSize as usize;
            let ptr = (*cred).CredentialBlob;
            if ptr.is_null() || size == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(ptr, size).to_vec()
            }
        };
        // SAFETY: freeing the exact pointer CredReadW returned.
        unsafe { CredFree(cred as *const std::ffi::c_void) };
        Ok(Some(value))
    }

    fn write(&self, key: &str, value: &[u8]) -> Result<(), SecretStoreError> {
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::Security::Credentials::{
            CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
        };
        if value.len() > CRED_MAX_CREDENTIAL_BLOB_SIZE {
            return Err(SecretStoreError::TooLarge {
                len: value.len(),
                max: CRED_MAX_CREDENTIAL_BLOB_SIZE,
            });
        }
        let mut target = wide(key);
        // SAFETY: all-zero is a valid CREDENTIALW (null pointers, zero
        // sizes); the fields that matter are set below.
        let mut cred: CREDENTIALW = unsafe { std::mem::zeroed() };
        cred.Type = CRED_TYPE_GENERIC;
        cred.TargetName = target.as_mut_ptr();
        cred.CredentialBlobSize = value.len() as u32;
        cred.CredentialBlob = value.as_ptr() as *mut u8;
        cred.Persist = CRED_PERSIST_LOCAL_MACHINE;
        // SAFETY: `cred` points at `target` and `value`, both alive for
        // the call; CredWriteW copies them.
        let ok = unsafe { CredWriteW(&cred, 0) };
        if ok == 0 {
            // SAFETY: plain thread-local error read.
            let code = unsafe { GetLastError() };
            return Err(SecretStoreError::Storage(format!(
                "CredWriteW failed (error {code})"
            )));
        }
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), SecretStoreError> {
        use windows_sys::Win32::Foundation::{GetLastError, ERROR_NOT_FOUND};
        use windows_sys::Win32::Security::Credentials::{CredDeleteW, CRED_TYPE_GENERIC};
        let target = wide(key);
        // SAFETY: `target` is NUL-terminated and outlives the call.
        let ok = unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) };
        if ok == 0 {
            // SAFETY: plain thread-local error read.
            let code = unsafe { GetLastError() };
            if code != ERROR_NOT_FOUND {
                return Err(SecretStoreError::Storage(format!(
                    "CredDeleteW failed (error {code})"
                )));
            }
        }
        Ok(())
    }
}

/// The platform's secret store: Credential Manager on Windows,
/// [`UnsupportedSecretStore`] elsewhere.
pub fn platform_secret_store() -> &'static dyn SecretStore {
    #[cfg(windows)]
    {
        &CredentialManagerSecretStore
    }
    #[cfg(not(windows))]
    {
        &UnsupportedSecretStore
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trips() {
        let store = MemorySecretStore::default();
        assert_eq!(store.read("k").unwrap(), None);
        store.write("k", b"value").unwrap();
        assert_eq!(store.read("k").unwrap().as_deref(), Some(&b"value"[..]));
        store.write("k", b"other").unwrap();
        assert_eq!(store.read("k").unwrap().as_deref(), Some(&b"other"[..]));
        store.delete("k").unwrap();
        assert_eq!(store.read("k").unwrap(), None);
        // Deleting an absent key succeeds.
        store.delete("k").unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn unsupported_store_fails_without_panicking() {
        let store = UnsupportedSecretStore;
        assert_eq!(store.read("k"), Err(SecretStoreError::NotSupported));
        assert_eq!(store.write("k", b"v"), Err(SecretStoreError::NotSupported));
        assert_eq!(store.delete("k"), Err(SecretStoreError::NotSupported));
    }

    /// Real Credential Manager round trip under a unique target that is
    /// deleted afterwards. Ignored by default: it writes to the current
    /// user's credential store. Run with `--ignored` on Windows.
    #[cfg(windows)]
    #[test]
    #[ignore = "touches the real user credential store"]
    fn credential_manager_round_trip() {
        let store = CredentialManagerSecretStore;
        let mut nonce = [0u8; 8];
        getrandom::fill(&mut nonce).unwrap();
        let hex: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        let key = format!("llmdil/test/secret-store/{hex}");
        let result = std::panic::catch_unwind(|| {
            assert_eq!(store.read(&key).unwrap(), None);
            store.write(&key, b"round trip").unwrap();
            assert_eq!(
                store.read(&key).unwrap().as_deref(),
                Some(&b"round trip"[..])
            );
            let big = vec![b'x'; CRED_MAX_CREDENTIAL_BLOB_SIZE + 1];
            assert!(matches!(
                store.write(&key, &big),
                Err(SecretStoreError::TooLarge { .. })
            ));
            store.delete(&key).unwrap();
            assert_eq!(store.read(&key).unwrap(), None);
        });
        let _ = store.delete(&key);
        result.unwrap();
    }
}
