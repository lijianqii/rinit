//! `mount(2)` / mount-point helpers (plan §1.3(9)).

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::cstring;
use crate::error::SyscallError;

/// Ensure `dir` exists, creating parents as needed.
pub fn mkdir_p(dir: &Path) -> Result<(), SyscallError> {
    std::fs::create_dir_all(dir).map_err(|e| SyscallError::from_io("create_dir_all", &e))
}

/// Like [`cstring`], but for an OS path (which need not be valid UTF-8).
fn cpath(op: &'static str, path: &Path) -> Result<CString, SyscallError> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| SyscallError::new(op, libc::EINVAL))
}

/// Raw `mount(2)` wrapper. The caller is responsible for creating `target`.
///
/// # Errors
/// Returns [`SyscallError`] with the operation name `"mount"` and the captured
/// `errno` if the syscall fails (e.g. `EPERM` without `CAP_SYS_ADMIN`,
/// `ENODEV` for an unknown filesystem, or `ENOENT` if `target` is missing).
pub fn mount(
    source: &str,
    target: &Path,
    fstype: &str,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> Result<(), SyscallError> {
    let src = cstring("mount", source)?;
    let tgt = cpath("mount", target)?;
    let fst = cstring("mount", fstype)?;
    let dat = data.map(|d| cstring("mount", d)).transpose()?;
    let data_ptr = dat.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()).cast();

    // SAFETY: all pointers originate from CString values that outlive the call;
    // `data_ptr` is either null or a valid NUL-terminated string.
    let rc = unsafe { libc::mount(src.as_ptr(), tgt.as_ptr(), fst.as_ptr(), flags, data_ptr) };
    if rc == 0 {
        Ok(())
    } else {
        Err(SyscallError::last_os_error("mount"))
    }
}

/// Mount a `tmpfs` at `target` with the given `mode` (octal string, e.g. `"0755"`).
pub fn mount_tmpfs(target: &Path, mode: &str) -> Result<(), SyscallError> {
    mkdir_p(target)?;
    let data = format!("mode={mode}");
    mount(
        "tmpfs",
        target,
        "tmpfs",
        libc::MS_NOSUID | libc::MS_NODEV,
        Some(&data),
    )
}

/// Mount `proc` at `target`.
pub fn mount_proc(target: &Path) -> Result<(), SyscallError> {
    mkdir_p(target)?;
    mount(
        "proc",
        target,
        "proc",
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        None,
    )
}

/// Mount `sysfs` at `target`.
pub fn mount_sysfs(target: &Path) -> Result<(), SyscallError> {
    mkdir_p(target)?;
    mount(
        "sysfs",
        target,
        "sysfs",
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        None,
    )
}

/// Mount `devtmpfs` at `target`.
pub fn mount_devtmpfs(target: &Path) -> Result<(), SyscallError> {
    mkdir_p(target)?;
    mount(
        "devtmpfs",
        target,
        "devtmpfs",
        libc::MS_NOSUID,
        Some("mode=0755"),
    )
}

/// Mount the cgroup v2 unified hierarchy at `target`.
///
/// Reserved for M5, where it joins the first-wave sequence. Public API of the
/// syscall layer, so it is intentionally callable before then.
pub fn mount_cgroup2(target: &Path) -> Result<(), SyscallError> {
    mkdir_p(target)?;
    mount(
        "cgroup2",
        target,
        "cgroup2",
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        None,
    )
}
