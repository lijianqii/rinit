//! `reboot(2)` / `sync(2)` (plan §1.3(8)).

use crate::error::SyscallError;

/// `reboot(2)` command: restart the system.
pub const RB_AUTOBOOT: libc::c_int = 0x0123_4567;
/// `reboot(2)` command: power off the system.
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
