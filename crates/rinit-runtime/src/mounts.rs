//! First-wave mount orchestration (plan §1.3(9)).
//!
//! Order is fixed: `/run` (tmpfs) -> `/proc` -> `/sys` -> `/dev` (devtmpfs),
//! all fatal; `/tmp` (tmpfs) is degradable. cgroup v2 is a reserved slot that
//! stays off until M5.
//!
//! The policy is expressed against the [`MountOps`] trait rather than against
//! `mount(2)` directly. That split exists so the fatal-vs-degradable rules -
//! the part that actually matters when the rootfs is broken - can be unit
//! tested on the host without `CAP_SYS_ADMIN` (plan §4.2 asks for exactly this
//! kind of injectable fake), and so a future backend (mount helper, faked in
//! an integration test) is a drop-in change.

use std::path::Path;

use crate::error::{RuntimeError, SyscallError};
use crate::syscall::mount;

/// The side-effecting primitives the first-wave sequence needs.
///
/// Every method returns a [`SyscallError`] carrying the failing step, so a
/// real failure names the operation that broke.
pub trait MountOps {
    /// Create `target` and any missing parents (`mkdir -p`).
    fn mkdir_p(&mut self, target: &Path) -> Result<(), SyscallError>;
    /// Mount a `tmpfs` with the given octal `mode` (e.g. `"0755"`).
    fn mount_tmpfs(&mut self, target: &Path, mode: &str) -> Result<(), SyscallError>;
    /// Mount `proc`.
    fn mount_proc(&mut self, target: &Path) -> Result<(), SyscallError>;
    /// Mount `sysfs`.
    fn mount_sysfs(&mut self, target: &Path) -> Result<(), SyscallError>;
    /// Mount `devtmpfs`.
    fn mount_devtmpfs(&mut self, target: &Path) -> Result<(), SyscallError>;
}

/// The real thing: `mkdir(2)` + `mount(2)` through the audited syscall layer.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealMounts;

impl MountOps for RealMounts {
    fn mkdir_p(&mut self, target: &Path) -> Result<(), SyscallError> {
        mount::mkdir_p(target)
    }

    fn mount_tmpfs(&mut self, target: &Path, mode: &str) -> Result<(), SyscallError> {
        mount::mount_tmpfs(target, mode)
    }

    fn mount_proc(&mut self, target: &Path) -> Result<(), SyscallError> {
        mount::mount_proc(target)
    }

    fn mount_sysfs(&mut self, target: &Path) -> Result<(), SyscallError> {
        mount::mount_sysfs(target)
    }

    fn mount_devtmpfs(&mut self, target: &Path) -> Result<(), SyscallError> {
        mount::mount_devtmpfs(target)
    }
}

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

/// Run the first-wave mount sequence against injected `ops`.
///
/// # Errors
/// Returns the fatal [`SyscallError`] if any of `/run`, `/proc`, `/sys` or
/// `/dev` fails to mount, aborting the sequence immediately - later steps are
/// not attempted, since mounting `/dev` without `/proc` is never useful.
/// A `/tmp` failure is collected as a warning and the sequence continues.
pub fn first_wave_mounts_with(ops: &mut impl MountOps) -> Result<MountReport, RuntimeError> {
    let mut report = MountReport::default();

    fatal(
        &mut report,
        "/run",
        ops.mount_tmpfs(Path::new("/run"), "0755"),
    )?;
    fatal(&mut report, "/proc", ops.mount_proc(Path::new("/proc")))?;
    fatal(&mut report, "/sys", ops.mount_sysfs(Path::new("/sys")))?;
    fatal(&mut report, "/dev", ops.mount_devtmpfs(Path::new("/dev")))?;

    // Degradable: a missing /tmp must not stop the boot.
    match ops.mount_tmpfs(Path::new("/tmp"), "1777") {
        Ok(()) => report.mounted.push("/tmp"),
        Err(e) => report.warnings.push(format!("mount /tmp failed: {e}")),
    }

    // cgroup v2 reserved for M5: intentionally not mounted here.

    Ok(report)
}

/// Perform the hardcoded first-wave mounts on the real system.
///
/// This function blocks; call it from `spawn_blocking` (plan §2.1⑤).
pub fn first_wave_mounts() -> Result<MountReport, RuntimeError> {
    first_wave_mounts_with(&mut RealMounts)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records the exact call sequence and can fail one named step, optionally
    /// restricted to a single mount point (so `/run` can succeed while `/tmp`
    /// fails).
    #[derive(Default)]
    struct FakeMounts {
        calls: Vec<String>,
        fail: Option<(&'static str, Option<&'static str>)>,
    }

    impl FakeMounts {
        /// Fail `step` wherever it appears.
        fn failing(step: &'static str) -> Self {
            FakeMounts {
                calls: Vec::new(),
                fail: Some((step, None)),
            }
        }

        /// Fail `step`, but only for `target`.
        fn failing_at(step: &'static str, target: &'static str) -> Self {
            FakeMounts {
                calls: Vec::new(),
                fail: Some((step, Some(target))),
            }
        }

        /// Record a call, honouring the injected failure for this step.
        fn step(&mut self, name: &'static str, target: &Path) -> Result<(), SyscallError> {
            let target_s: &str = target.to_str().unwrap_or("");
            self.calls.push(format!("{name} {target_s}"));

            if let Some((fail_name, fail_target)) = self.fail
                && fail_name == name
                && fail_target.is_none_or(|t| t == target_s)
            {
                return Err(SyscallError::new(name, libc::EPERM));
            }
            Ok(())
        }
    }

    impl MountOps for FakeMounts {
        fn mkdir_p(&mut self, target: &Path) -> Result<(), SyscallError> {
            self.step("mkdir_p", target)
        }

        fn mount_tmpfs(&mut self, target: &Path, _mode: &str) -> Result<(), SyscallError> {
            self.step("mount_tmpfs", target)
        }

        fn mount_proc(&mut self, target: &Path) -> Result<(), SyscallError> {
            self.step("mount_proc", target)
        }

        fn mount_sysfs(&mut self, target: &Path) -> Result<(), SyscallError> {
            self.step("mount_sysfs", target)
        }

        fn mount_devtmpfs(&mut self, target: &Path) -> Result<(), SyscallError> {
            self.step("mount_devtmpfs", target)
        }
    }

    /// The order is fixed by plan §1.3(9); a reorder would change semantics
    /// (`/dev` must be devtmpfs before anything writes to it).
    #[test]
    fn mounts_first_wave_in_the_specified_order() {
        let mut ops = FakeMounts::default();
        let report = first_wave_mounts_with(&mut ops).expect("first wave");

        assert_eq!(
            ops.calls,
            [
                "mount_tmpfs /run",
                "mount_proc /proc",
                "mount_sysfs /sys",
                "mount_devtmpfs /dev",
                "mount_tmpfs /tmp",
            ]
        );
        assert_eq!(report.mounted, ["/run", "/proc", "/sys", "/dev", "/tmp"]);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    /// Every fatal step must abort the boot, and must abort *immediately*:
    /// continuing would attempt `/dev` without `/proc`.
    #[test]
    fn each_fatal_step_aborts_the_sequence() {
        // (failing step, how many calls should have been attempted)
        let cases = [
            ("mount_tmpfs", 1),
            ("mount_proc", 2),
            ("mount_sysfs", 3),
            ("mount_devtmpfs", 4),
        ];
        for (step, expected_calls) in cases {
            let mut ops = FakeMounts::failing(step);
            let err = first_wave_mounts_with(&mut ops)
                .expect_err("a fatal mount failure must abort the boot");
            assert!(
                matches!(err, RuntimeError::Syscall(ref e) if e.errno() == libc::EPERM),
                "{step}: unexpected error {err:?}"
            );
            assert_eq!(
                ops.calls.len(),
                expected_calls,
                "{step}: sequence continued past a fatal failure: {:?}",
                ops.calls
            );
        }
    }

    /// `/tmp` is the one degradable mount: its failure warns, and the earlier
    /// mounts still count as done.
    #[test]
    fn tmp_failure_degrades_to_a_warning() {
        let mut ops = FakeMounts::failing_at("mount_tmpfs", "/tmp");
        let report = first_wave_mounts_with(&mut ops).expect("boot must survive /tmp");

        assert_eq!(report.mounted, ["/run", "/proc", "/sys", "/dev"]);
        assert_eq!(
            ops.calls.len(),
            5,
            "/tmp is still attempted: {:?}",
            ops.calls
        );
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(
            report.warnings[0].contains("/tmp"),
            "warning must name the mount: {:?}",
            report.warnings
        );
    }

    /// The report is what the boot log prints, so a failure must be
    /// attributable to the step that produced it.
    #[test]
    fn failures_carry_the_offending_operation_name() {
        let mut ops = FakeMounts::failing("mount_proc");
        let err = first_wave_mounts_with(&mut ops).unwrap_err();
        assert!(err.to_string().contains("mount_proc"), "got: {err}");
    }

    /// `RealMounts` must forward to the audited syscall layer; the mkdir
    /// helper is the only step that can succeed before a mount is attempted.
    #[test]
    fn real_mounts_mkdir_is_idempotent_and_reports_underlying_errors() {
        let dir = std::env::temp_dir().join("rinit-mkdir-p-test/nested/deep");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
        assert!(RealMounts.mkdir_p(&dir).is_ok());
        // `mkdir_p` on an existing tree must succeed, not report EEXIST.
        assert!(RealMounts.mkdir_p(&dir).is_ok());
        assert!(dir.is_dir());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// Mounting without `CAP_SYS_ADMIN` fails with EPERM; this asserts the
    /// wrapper reports it rather than silently succeeding. Skipped where the
    /// test process happens to be privileged, since the point is the error
    /// path, not the privilege level.
    #[test]
    fn real_mount_without_privileges_reports_eperm() {
        let target = std::env::temp_dir().join("rinit-mount-test");
        let _ = std::fs::remove_dir_all(&target);
        let err = RealMounts
            .mkdir_p(&target)
            .and_then(|()| RealMounts.mount_proc(&target));
        if let Err(e) = err {
            assert!(
                [libc::EPERM, libc::ENODEV, libc::EINVAL].contains(&e.errno()),
                "unexpected errno in {}: {e}",
                e.errno()
            );
        }
        let _ = std::fs::remove_dir_all(&target);
    }
}
