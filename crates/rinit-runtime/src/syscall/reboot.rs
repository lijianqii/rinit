//! `reboot(2)` / `sync(2)` and the last-resort reset (plan §1.3(8)).

use std::io::Write;

use crate::error::SyscallError;

/// `reboot(2)` command: restart the system.
pub const RB_AUTOBOOT: libc::c_int = 0x0123_4567;
/// `reboot(2)` command: power the system off.
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

/// An already-open `/proc/sysrq-trigger`.
///
/// Shutdown unmounts `/proc` before it calls `reboot(2)`, so the last-resort
/// trigger has to be opened *before* that unmount. Keeping the file open also
/// avoids depending on `/proc` still being reachable when the fallback runs.
pub struct SysrqTrigger {
    file: std::fs::File,
}

impl SysrqTrigger {
    /// Open the kernel's SysRq trigger for the final fallback.
    ///
    /// # Errors
    /// [`SyscallError`] with `ENOENT`/`EACCES` when the trigger is unavailable
    /// (no `/proc`, `kernel.sysrq` disabled, or not root).
    pub fn open() -> Result<Self, SyscallError> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open("/proc/sysrq-trigger")
            .map_err(|e| SyscallError::from_io("open(/proc/sysrq-trigger)", &e))?;
        Ok(SysrqTrigger { file })
    }

    /// Ask the kernel to reboot now.
    pub fn reboot(&mut self) -> Result<(), SyscallError> {
        self.write(b'b')
    }

    /// Ask the kernel to power off now.
    pub fn poweroff(&mut self) -> Result<(), SyscallError> {
        self.write(b'o')
    }

    fn write(&mut self, command: u8) -> Result<(), SyscallError> {
        self.file
            .write_all(&[command])
            .map_err(|e| SyscallError::from_io("write(/proc/sysrq-trigger)", &e))?;
        self.file
            .flush()
            .map_err(|e| SyscallError::from_io("flush(/proc/sysrq-trigger)", &e))
    }
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
    SysrqTrigger::open()?.reboot()
}

/// Last-resort poweroff through the kernel's SysRq trigger.
///
/// `o` is the kernel's "power off" action. It is only used after
/// `reboot(RB_POWER_OFF)` failed; the graceful path already ran.
pub fn sysrq_poweroff() -> Result<(), SyscallError> {
    SysrqTrigger::open()?.poweroff()
}
