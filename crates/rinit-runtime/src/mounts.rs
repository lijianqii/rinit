//! First-wave mount orchestration (plan §1.3(9)).
//!
//! Order is fixed: `/run` (tmpfs) -> `/proc` -> `/sys` -> `/dev` (devtmpfs),
//! all fatal; `/tmp` (tmpfs) is degradable. cgroup v2 is a reserved slot that
//! stays off until M5. Every fatal mount point is `mkdir -p`'d first, because
//! `mount(2)` requires the target to exist.

use std::path::Path;

use crate::error::{RuntimeError, SyscallError};
use crate::syscall::mount;

/// Outcome of the first-wave mounts.
#[derive(Debug, Default)]
pub struct MountReport {
    /// Mount points mounted successfully (in order).
    pub mounted: Vec<&'static str>,
    /// Non-fatal failures, as human-readable messages.
    pub warnings: Vec<String>,
}

fn fatal(
    report: &mut MountReport,
    target: &'static str,
    res: Result<(), SyscallError>,
) -> Result<(), RuntimeError> {
    match res {
        Ok(()) => {
            report.mounted.push(target);
            Ok(())
        }
        Err(e) => Err(RuntimeError::Syscall(e)),
    }
}

/// Perform the hardcoded first-wave mounts.
///
/// # Errors
/// Returns the fatal [`SyscallError`] if any of `/run`, `/proc`, `/sys` or
/// `/dev` fails to mount. `/tmp` failures are collected as warnings instead.
/// This function blocks; call it from `spawn_blocking` (plan §2.1⑤).
pub fn first_wave_mounts() -> Result<MountReport, RuntimeError> {
    let mut report = MountReport::default();

    fatal(
        &mut report,
        "/run",
        mount::mount_tmpfs(Path::new("/run"), "0755"),
    )?;
    fatal(&mut report, "/proc", mount::mount_proc(Path::new("/proc")))?;
    fatal(&mut report, "/sys", mount::mount_sysfs(Path::new("/sys")))?;
    fatal(
        &mut report,
        "/dev",
        mount::mount_devtmpfs(Path::new("/dev")),
    )?;

    // Degradable: /tmp failure only warns.
    match mount::mount_tmpfs(Path::new("/tmp"), "1777") {
        Ok(()) => report.mounted.push("/tmp"),
        Err(e) => report.warnings.push(format!("mount /tmp failed: {e}")),
    }

    // cgroup v2 reserved for M5: intentionally not mounted here.

    Ok(report)
}
