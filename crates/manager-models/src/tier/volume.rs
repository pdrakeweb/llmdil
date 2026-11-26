//! Volume queries for tier roots: free/total bytes and same-volume
//! detection. `None` means "could not tell" — callers treat it as
//! unknown, never as zero.

use std::path::Path;

/// Free bytes available to the caller on the volume holding `path`.
pub fn free_bytes(path: &Path) -> Option<u64> {
    disk_space(path).map(|(free, _)| free)
}

/// Total bytes of the volume holding `path`.
pub fn total_bytes(path: &Path) -> Option<u64> {
    disk_space(path).map(|(_, total)| total)
}

/// Whether two existing paths live on the same volume.
pub fn same_volume(a: &Path, b: &Path) -> Option<bool> {
    #[cfg(windows)]
    {
        let (va, vb) = (volume_root(a)?, volume_root(b)?);
        Some(va.eq_ignore_ascii_case(&vb))
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(std::fs::metadata(a).ok()?.dev() == std::fs::metadata(b).ok()?.dev())
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (a, b);
        None
    }
}

#[cfg(windows)]
fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
fn volume_root(path: &Path) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;
    let w = wide(path);
    let mut buf = vec![0u16; 1024];
    // SAFETY: `w` is NUL-terminated; `buf` is writable for its length.
    let ok = unsafe { GetVolumePathNameW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if ok == 0 {
        return None;
    }
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

#[cfg(windows)]
fn disk_space(path: &Path) -> Option<(u64, u64)> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let w = wide(path);
    let (mut free, mut total, mut total_free) = (0u64, 0u64, 0u64);
    // SAFETY: `w` is NUL-terminated; the out-pointers are valid u64s.
    let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut free, &mut total, &mut total_free) };
    (ok != 0).then_some((free, total))
}

#[cfg(not(windows))]
fn disk_space(_path: &Path) -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_dir_reports_its_volume() {
        let t = std::env::temp_dir();
        assert_eq!(same_volume(&t, &t), Some(true));
        #[cfg(windows)]
        {
            let free = free_bytes(&t).expect("free space on the temp volume");
            let total = total_bytes(&t).expect("total size of the temp volume");
            assert!(free <= total && total > 0);
        }
        assert_eq!(same_volume(&t, &t.join("does-not-exist")), {
            #[cfg(windows)]
            {
                // GetVolumePathNameW resolves non-existent leaves too.
                Some(true)
            }
            #[cfg(not(windows))]
            {
                None
            }
        });
    }
}
