//! `reboot(2)` / `sync(2)` and the last-resort reset (plan §1.3(8)).

use std::io::Write;

use crate::error::SyscallError;

/// `reboot(2)` command: restart the system.
pub const RB_AUTOBOOT: libc::c_int = 0x0123_4567;
/// `reboot(2)` command: power off the system.
///
/// rinit always reboots today; M4 needs this to distinguish a `poweroff` from a
/// `reboot` (plan §7.10).
pub const RB_POWER_OFF: libc::c_int = 0x4321_fedc;

/// Invoke `reboot(2)`. Requires `CAP_SYS_BOOT` (rinit runs as root).
pub fn reboot(cmd: libc::c_int) -> Result<(), SyscallError> {
    // SAFETY: `reboot` takes only scalar arguments.
    let rc = unsafe { libc::reboot(cmd) };
    if rc == 0 {
        Ok(())
    } else {
        Err(SyscallError::last_os_error("reboot"))
    }
}

/// Flush filesystem buffers.
pub fn sync() {
    // SAFETY: `sync` takes no arguments and never fails.
    unsafe { libc::sync() }
}

/// Last-resort reset through the kernel's SysRq trigger (plan §1.3(8)).
///
/// Used only when `reboot(2)` itself failed. Writing `b` to
/// `/proc/sysrq-trigger` makes the kernel reboot immediately, without
/// unmounting or syncing again - which is exactly what is wanted at that point:
/// the graceful path already ran and failed.
///
/// # Errors
/// [`SyscallError`] with `ENOENT`/`EACCES` when the trigger is unavailable
/// (no `/proc`, `kernel.sysrq` disabled, or not root).
pub fn sysrq_reboot() -> Result<(), SyscallError> {
    let mut trigger = std::fs::OpenOptions::new()
        .write(true)
        .open("/proc/sysrq-trigger")
        .map_err(|e| SyscallError::from_io("open(/proc/sysrq-trigger)", &e))?;
    trigger
        .write_all(b"b")
        .map_err(|e| SyscallError::from_io("write(/proc/sysrq-trigger)", &e))?;
    trigger
        .flush()
        .map_err(|e| SyscallError::from_io("flush(/proc/sysrq-trigger)", &e))
}
