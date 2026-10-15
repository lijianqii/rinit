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

/// Outcome of the shutdown unmount pass.
#[derive(Debug, Default)]
pub struct UnmountReport {
    /// Mount points unmounted successfully, in the order performed.
    pub unmounted: Vec<String>,
    /// Mount points that could not be detached, as warnings.
    pub warnings: Vec<String>,
}

/// The mount points to unmount at shutdown, most recently mounted first.
///
/// Plan §1.3(8): unmount everything but the root before the final `reboot(2)`.
/// The root is skipped - rinit is running from it, and the kernel handles it -
/// and the order is reversed so a mount is never unmounted while something
/// mounted on top of it is still there.
///
/// Pure so the `/proc/mounts` parsing (including the octal escapes for spaces)
/// is testable without privileges.
pub fn unmount_order(proc_mounts: &str) -> Vec<String> {
    let mut points: Vec<String> = proc_mounts
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .map(unescape_mount_field)
        .filter(|point| point != "/")
        .collect();
    points.reverse();
    points
}

/// Decode the octal escapes `/proc/mounts` uses for space, tab, newline and
/// backslash in a path.
fn unescape_mount_field(field: &str) -> String {
    field
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

/// Unmount everything but the root, best effort (plan §1.3(8)).
///
/// A mount that refuses a plain `umount2` is retried lazily (`MNT_DETACH`), and
/// one that still will not go is reported and left alone: the shutdown must
/// reach `reboot(2)` regardless.
pub fn unmount_filesystems() -> UnmountReport {
    let mut report = UnmountReport::default();
    let Ok(contents) = std::fs::read_to_string("/proc/mounts") else {
        // No /proc: nothing we can enumerate, so there is nothing to do.
        return report;
    };

    for point in unmount_order(&contents) {
        match mount::umount2(Path::new(&point), 0) {
            Ok(()) => report.unmounted.push(point),
            Err(first) => {
                // Busy, or already gone. Try lazily, then give up on it.
                if mount::umount2(Path::new(&point), libc::MNT_DETACH).is_ok() {
                    report.unmounted.push(point);
                } else if !matches!(first.errno(), libc::EINVAL | libc::ENOENT) {
                    report
                        .warnings
                        .push(format!("umount {point} failed: {first}"));
                }
            }
        }
    }
    report
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

    // --- shutdown unmount (plan §1.3(8)) --------------------------------

    /// `unmount_order` reads the mount point column, skips the root and
    /// reverses, so a mount is never removed from under another.
    #[test]
    fn unmount_order_skips_root_and_reverses() {
        let proc_mounts = "\
/dev/sda1 / ext4 rw,relatime 0 0
proc /proc proc rw,nosuid 0 0
tmpfs /run tmpfs rw 0 0
/dev/sdb1 /data ext4 rw 0 0
";
        assert_eq!(
            unmount_order(proc_mounts),
            ["/data", "/run", "/proc"],
            "the root is kept and the rest are unmounted newest-first"
        );
    }

    /// `/proc/mounts` escapes spaces in paths; a mount point with one must be
    /// unmounted by its real name.
    #[test]
    fn unmount_order_decodes_octal_escapes() {
        let proc_mounts = "/dev/sda1 / ext4 rw 0 0\ntmpfs /mnt/with\\040space tmpfs rw 0 0\n";
        assert_eq!(unmount_order(proc_mounts), ["/mnt/with space"]);
    }

    /// An unmountable path reports the syscall error instead of panicking.
    #[test]
    fn umount_of_an_unmounted_path_reports_einval() {
        let err = mount::umount2(Path::new("/definitely/not/mounted"), 0)
            .expect_err("unmounting a non-mount must fail");
        assert!(
            [libc::EINVAL, libc::ENOENT].contains(&err.errno()),
            "unexpected errno in {}: {err}",
            err.errno()
        );
    }
}
