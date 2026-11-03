//! Credential Manager reads for the shipper (design §5.2 layer 1,
//! §6.5/D6): the ingest token and the literal-blocklist secret
//! values. Values returned here are secrets: they are never logged,
//! never written to disk, and never leave the process except as the
//! `Authorization` header on the shipper's own POSTs (the token) —
//! the blocklist values never leave at all.
//!
//! The Windows implementation mirrors manager-wizard's
//! `cred_read_windows` (CredReadW + the same blob decoding).

/// Read a generic credential's value by target name. `None` on any
/// failure (missing target, empty blob, undecodable content) and on
/// non-Windows platforms, where there is no Credential Manager.
pub fn read_credential(target: &str) -> Option<String> {
    #[cfg(windows)]
    {
        cred_read_windows(target)
    }
    #[cfg(not(windows))]
    {
        let _ = target;
        None
    }
}

#[cfg(windows)]
fn cred_read_windows(target: &str) -> Option<String> {
    use windows_sys::Win32::Security::Credentials::{
        CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC,
    };
    let wide: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: `wide` is a NUL-terminated UTF-16 target name that
    // outlives the call; `cred` is either left null or points to a
    // CREDENTIALW owned by the OS that we free with CredFree below.
    let ok = unsafe { CredReadW(wide.as_ptr(), CRED_TYPE_GENERIC, 0, &mut cred) };
    if ok == 0 || cred.is_null() {
        return None;
    }
    // SAFETY: `cred` is valid per the successful CredReadW above;
    // the blob slice is bounded by CredentialBlobSize.
    let value = unsafe {
        let size = (*cred).CredentialBlobSize as usize;
        let ptr = (*cred).CredentialBlob;
        if ptr.is_null() || size == 0 {
            None
        } else {
            decode_credential_blob(std::slice::from_raw_parts(ptr, size))
        }
    };
    // SAFETY: freeing the exact pointer CredReadW returned.
    unsafe { CredFree(cred as *const std::ffi::c_void) };
    value.filter(|v| !v.is_empty())
}

/// Decode a credential blob: generic credentials written by cmdkey /
/// PowerShell hold the secret as UTF-16LE text; anything else is
/// read as UTF-8. Same rule as manager-wizard's decoder.
#[cfg(any(windows, test))]
fn decode_credential_blob(blob: &[u8]) -> Option<String> {
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
    let utf16_ascii = utf16
        .as_deref()
        .map(|s| !s.is_empty() && s.is_ascii())
        .unwrap_or(false);
    if utf16_ascii {
        return utf16;
    }
    if let Ok(s) = std::str::from_utf8(blob) {
        let s = s.trim_end_matches('\0');
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    utf16.filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_utf16le_and_utf8_blobs() {
        let utf16: Vec<u8> = "secret-value"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(decode_credential_blob(&utf16).as_deref(), Some("secret-value"));
        assert_eq!(
            decode_credential_blob(b"plain-utf8-value").as_deref(),
            Some("plain-utf8-value")
        );
        assert_eq!(decode_credential_blob(b""), None);
    }
}
