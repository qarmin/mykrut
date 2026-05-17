//! Disk-free / total reporting via `statvfs(2)` on Unix.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

#[derive(Clone, Copy, Debug, Default)]
pub struct DiskSpace {
    pub free: u64,
    pub total: u64,
}

/// Query free + total bytes for the filesystem containing `path`.
/// Returns `None` if the syscall fails (e.g., path doesn't exist).
pub fn query(path: &Path) -> Option<DiskSpace> {
    let cpath = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `libc::statvfs` is a plain-old-data struct, so an all-zero bit
    // pattern is a valid (if meaningless) initial value before the syscall fills it.
    let mut sv: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `cpath` is a valid NUL-terminated C string and `sv` is a valid,
    // writable `statvfs`; the return code is checked before any field is read.
    let rc = unsafe { libc::statvfs(cpath.as_ptr(), &mut sv) };
    if rc != 0 {
        return None;
    }
    let block = sv.f_frsize as u64;
    Some(DiskSpace {
        free: sv.f_bavail as u64 * block,
        total: sv.f_blocks as u64 * block,
    })
}
