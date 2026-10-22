//! The second wave of mounts: `/etc/fstab` (plan §1.3(9)).
//!
//! The first wave is a fixed list in `mounts.rs`; this one is whatever the board
//! wrote down, and the plan is explicit about how little that is worth: a
//! six-field parser, `noauto` entries ignored, no UUID/LABEL resolution, no
//! generator and no `.mount` units - the entries are mounted in file order with
//! `mount(2)`, and one that fails is a warning rather than a stopped boot.
//! `swap` entries are the deliberate exception: they are parsed and left for
//! `swapon -a` instead of being passed to `mount(2)`.
//!
//! That shape is the point. A data partition that will not mount (`/data` on a
//! board whose eMMC is missing) is a degraded system, not a dead one, and
//! decoupling it from the dependency graph is what keeps a mount failure from
//! becoming an ordering problem.
//!
//! Nothing here needs privileges: the parse, the option translation and the
//! sequence are all testable through [`MountOps`], and the guest test mounts a
//! real `tmpfs` to prove the sequence reaches `mount(2)`.

use std::path::{Path, PathBuf};

use crate::error::SyscallError;
use crate::mounts::MountOps;

/// One parsed `/etc/fstab` line.
///
/// The six fields are kept whole rather than pre-split: `device`, `mountpoint`,
/// `fstype` and the option string are what `mount(2)` wants, and `dump`/`pass`
/// are parsed only so that a line with them is not mistaken for a malformed one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The device, a `PARTUUID=`, or `none` - passed to `mount(2)` as written,
    /// or to `swapon -a` for a swap entry.
    pub device: String,
    /// Where it goes. An absolute path, except for swap entries (`none`).
    pub mountpoint: PathBuf,
    /// The filesystem type, passed through as written.
    pub fstype: String,
    /// The option field, as written.
    pub options: String,
}

impl Entry {
    /// The option string split into `MS_*` flags and what is left for the
    /// filesystem.
    ///
    /// This is the one piece of judgement in the file. An option with an `MS_*`
    /// equivalent (`ro`, `nosuid`, `noexec`, `noatime`, ...) has to go in
    /// `mount(2)`'s *flags* argument: the kernel looks at the flags for these and
    /// never at the data string, so passing them as data would parse a unit file
    /// that says `nosuid` and mount it setuid-capable. Everything else
    /// (`mode=0755`, `size=64m`, `defaults`, ...) is the filesystem's own, and
    /// `defaults` is dropped because it is a no-op rather than an option.
    pub fn flags_and_data(&self) -> (libc::c_ulong, Option<String>) {
        let mut flags: libc::c_ulong = 0;
        let mut data: Vec<&str> = Vec::new();
        for option in self
            .options
            .split(',')
            .map(str::trim)
            .filter(|o| !o.is_empty())
        {
            match option {
                "ro" => flags |= libc::MS_RDONLY,
                "rw" => flags &= !libc::MS_RDONLY,
                "sync" => flags |= libc::MS_SYNCHRONOUS,
                "async" => flags &= !libc::MS_SYNCHRONOUS,
                "dirsync" => flags |= libc::MS_DIRSYNC,
                "noatime" => flags |= libc::MS_NOATIME,
                "atime" => flags &= !libc::MS_NOATIME,
                "nodiratime" => flags |= libc::MS_NODIRATIME,
                "diratime" => flags &= !libc::MS_NODIRATIME,
                "relatime" => flags |= libc::MS_RELATIME,
                "norelatime" => flags &= !libc::MS_RELATIME,
                "strictatime" => flags |= libc::MS_STRICTATIME,
                "nostrictatime" => flags &= !libc::MS_STRICTATIME,
                "suid" => flags &= !libc::MS_NOSUID,
                "nosuid" => flags |= libc::MS_NOSUID,
                "dev" => flags &= !libc::MS_NODEV,
                "nodev" => flags |= libc::MS_NODEV,
                "exec" => flags &= !libc::MS_NOEXEC,
                "noexec" => flags |= libc::MS_NOEXEC,
                "defaults" => {}
                other => data.push(other),
            }
        }
        let data = if data.is_empty() {
            None
        } else {
            Some(data.join(","))
        };
        (flags, data)
    }
}

/// What happened when the second wave ran.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FstabReport {
    /// Mountpoints that are now mounted, in the order they were mounted.
    pub mounted: Vec<String>,
    /// One line per entry that was skipped or failed, for the boot log.
    pub warnings: Vec<String>,
}

/// Parse an `/etc/fstab` body.
///
/// Returns the entries in file order plus a warning per line that could not be
/// used. A malformed line is a warning rather than an error for the same reason a
/// failed mount is: the file is the board's, and the board still has to boot.
///
/// `noauto` entries are skipped, as the plan says, and so are comment and blank
/// lines.
pub fn parse(text: &str) -> (Vec<Entry>, Vec<String>) {
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((device, mountpoint, fstype, options)) = read_entry(line) else {
            warnings.push(format!(
                "/etc/fstab:{}: expected at least four fields, skipping: {line}",
                index + 1
            ));
            continue;
        };
        // Swap entries use `none` as their (non-)mountpoint. They are kept in
        // the parsed list so callers can see them, but `mount_with` does not
        // pass them to mount(2); busybox `swapon -a` activates them from this
        // same file.
        if fstype != "swap" && !mountpoint.starts_with('/') {
            warnings.push(format!(
                "/etc/fstab:{}: mountpoint '{mountpoint}' is not absolute, skipping",
                index + 1
            ));
            continue;
        }
        if options.split(',').any(|option| option.trim() == "noauto") {
            continue;
        }
        entries.push(Entry {
            device: device.to_string(),
            mountpoint: PathBuf::from(mountpoint),
            fstype: fstype.to_string(),
            options: options.to_string(),
        });
    }
    (entries, warnings)
}

/// The four fields that matter, if the line has them.
///
/// `split_whitespace` with `next()` rather than a slice index: the crate denies
/// indexing, and more to the point a line the board got wrong has to come out of
/// here as `None` rather than as a panic in PID 1.
fn read_entry(line: &str) -> Option<(String, String, String, String)> {
    let mut fields = line.split_whitespace();
    Some((
        fields.next()?.to_string(),
        fields.next()?.to_string(),
        fields.next()?.to_string(),
        fields.next()?.to_string(),
    ))
}

/// Whether the boot wants the second wave at all.
///
/// The plan's trigger is the kernel command line: `nofstab` means "I mounted what
/// I need". Split out from reading `/proc/cmdline` so it is testable as what it
/// is - a question about a string.
pub fn enabled(cmdline: &str) -> bool {
    !cmdline.split_whitespace().any(|token| token == "nofstab")
}

/// Mount every non-swap entry of `text`, in order, through `ops`.
///
/// Swap entries are skipped: `swapon -a` consumes the same file. A failure is
/// collected and the next entry is still attempted: the plan's "失败记日志并继续",
/// and the reason the second wave is not expressed as dependencies.
///
/// The mountpoint is created if it does not exist, as the first wave's are: an
/// fstab line names where a filesystem *goes*, and a board whose `/data` is empty
/// because nothing has ever mounted there is the normal case.
pub fn mount_with(ops: &mut impl MountOps, text: &str) -> FstabReport {
    let (entries, mut warnings) = parse(text);
    let mut mounted = Vec::new();
    for entry in entries {
        // Swap is not a mount. It is activated by `swapon -a`, which reads the
        // same /etc/fstab line; rinit must not try to mount it on `none`.
        if entry.fstype == "swap" {
            continue;
        }
        let point = entry.mountpoint.to_string_lossy().into_owned();
        if let Err(e) = ops.mkdir_p(&entry.mountpoint) {
            warnings.push(format!("cannot create {point}: {e}"));
            continue;
        }
        let (flags, data) = entry.flags_and_data();
        if let Err(e) = ops.mount_fs(
            &entry.device,
            &entry.mountpoint,
            &entry.fstype,
            flags,
            data.as_deref(),
        ) {
            warnings.push(format!(
                "cannot mount {} on {point} ({}, {e})",
                entry.device, entry.fstype
            ));
            continue;
        }
        mounted.push(point);
    }
    FstabReport { mounted, warnings }
}

/// Read `path` and run the second wave, or say why not.
///
/// # Errors
/// [`SyscallError`] only for a file that exists and cannot be *read*: a missing
/// `/etc/fstab` is the plan's "optional" case, not a failure.
pub fn run_with(ops: &mut impl MountOps, path: &Path) -> Result<FstabReport, SyscallError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(FstabReport::default()),
        Err(e) => return Err(SyscallError::from_io("read(/etc/fstab)", &e)),
    };
    Ok(mount_with(ops, &text))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recording `MountOps`: the sequence, the flags and the data are what the
    /// second wave is, and none of it needs a privilege to observe.
    #[derive(Default)]
    struct Recorder {
        calls: Vec<String>,
        /// Mountpoints that should refuse.
        fail: Vec<String>,
        created: Vec<String>,
    }

    impl MountOps for Recorder {
        fn mkdir_p(&mut self, target: &Path) -> Result<(), SyscallError> {
            self.created.push(target.display().to_string());
            Ok(())
        }
        fn mount_tmpfs(&mut self, target: &Path, _mode: &str) -> Result<(), SyscallError> {
            self.calls.push(format!("tmpfs {}", target.display()));
            Ok(())
        }
        fn mount_proc(&mut self, _target: &Path) -> Result<(), SyscallError> {
            Ok(())
        }
        fn mount_sysfs(&mut self, _target: &Path) -> Result<(), SyscallError> {
            Ok(())
        }
        fn mount_devtmpfs(&mut self, _target: &Path) -> Result<(), SyscallError> {
            Ok(())
        }
        fn mount_cgroup2(&mut self, _target: &Path) -> Result<(), SyscallError> {
            Ok(())
        }
        fn mount_fs(
            &mut self,
            source: &str,
            target: &Path,
            fstype: &str,
            flags: libc::c_ulong,
            data: Option<&str>,
        ) -> Result<(), SyscallError> {
            let point = target.display().to_string();
            self.calls.push(format!(
                "{source} {point} {fstype} flags={flags:#x} data={}",
                data.unwrap_or("-")
            ));
            if self.fail.contains(&point) {
                return Err(SyscallError::new("mount", libc::ENODEV));
            }
            Ok(())
        }
        fn umount2(&mut self, _target: &Path, _flags: libc::c_int) -> Result<(), SyscallError> {
            // The second wave only mounts; unmounting is the shutdown's job, and a
            // test that sees this happen has found the two paths mixed up.
            Err(SyscallError::new("umount2", libc::EINVAL))
        }
    }

    /// The six fields, in order, with comments and `noauto` skipped.
    #[test]
    fn the_six_fields_are_parsed_and_noauto_is_skipped() {
        let (entries, warnings) = parse(
            "# a comment\n\
             \n\
             /dev/mmcblk0p3  /data   ext4   rw,noatime   0  2\n\
             tmpfs           /run/x  tmpfs  mode=0755,noauto 0 0\n\
             proc            /proc   proc   defaults     0  0\n",
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(entries.len(), 2, "the noauto entry is not one");
        assert_eq!(entries[0].device, "/dev/mmcblk0p3");
        assert_eq!(entries[0].mountpoint, PathBuf::from("/data"));
        assert_eq!(entries[0].fstype, "ext4");
        assert_eq!(entries[0].options, "rw,noatime");
        assert_eq!(entries[1].mountpoint, PathBuf::from("/proc"));
    }

    /// A line the board got wrong is a warning, not a stopped boot - and neither
    /// is a line with too few fields.
    #[test]
    fn malformed_lines_are_warnings() {
        let (entries, warnings) = parse(
            "tmpfs /only-two\n\
             tmpfs relative/mount tmpfs defaults 0 0\n\
             tmpfs /ok tmpfs defaults 0 0\n",
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].mountpoint, PathBuf::from("/ok"));
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("four fields"), "{warnings:?}");
        assert!(warnings[1].contains("not absolute"), "{warnings:?}");
    }

    /// Swap entries are legal fstab entries even though `none` is not an
    /// absolute mountpoint. They stay in the parsed list for callers to see,
    /// but the mount pass leaves them to `swapon -a`.
    #[test]
    fn swap_entries_parse_but_are_not_mounted() {
        let text = "/swapfile none swap sw 0 0\n";
        let (entries, warnings) = parse(text);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].device, "/swapfile");
        assert_eq!(entries[0].fstype, "swap");

        let mut ops = Recorder::default();
        let report = mount_with(&mut ops, text);
        assert!(report.mounted.is_empty());
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(ops.calls.is_empty(), "{:?}", ops.calls);
    }

    /// The one piece of judgement in the file: VFS options that *must* become
    /// flags, and filesystem options that must not.
    #[test]
    fn vfs_options_become_flags_and_the_rest_is_data() {
        let entry = Entry {
            device: "tmpfs".into(),
            mountpoint: PathBuf::from("/data"),
            fstype: "tmpfs".into(),
            options: "rw,nosuid,nodev,noexec,noatime,mode=0755,size=64m,defaults".into(),
        };
        let (flags, data) = entry.flags_and_data();
        assert_eq!(flags & libc::MS_RDONLY, 0, "rw is not read-only");
        assert_ne!(flags & libc::MS_NOSUID, 0);
        assert_ne!(flags & libc::MS_NODEV, 0);
        assert_ne!(flags & libc::MS_NOEXEC, 0);
        assert_ne!(flags & libc::MS_NOATIME, 0);
        assert_eq!(data.as_deref(), Some("mode=0755,size=64m"));

        // And the other direction: `ro` sets the bit, `suid` clears it again.
        let entry = Entry {
            options: "ro,suid".into(),
            ..entry
        };
        let (flags, data) = entry.flags_and_data();
        assert_ne!(flags & libc::MS_RDONLY, 0);
        assert_eq!(flags & libc::MS_NOSUID, 0);
        assert_eq!(data, None, "nothing left for the filesystem");
    }

    /// The sequence: file order, the mountpoint created first, and a failure that
    /// does not stop the entries after it.
    #[test]
    fn entries_are_mounted_in_order_and_a_failure_does_not_stop_the_rest() {
        let mut ops = Recorder {
            fail: vec!["/broken".to_string()],
            ..Recorder::default()
        };
        let report = mount_with(
            &mut ops,
            "tmpfs /first tmpfs defaults 0 0\n\
             /dev/nope /broken ext4 defaults 0 0\n\
             tmpfs /last tmpfs defaults 0 0\n",
        );

        assert_eq!(report.mounted, ["/first", "/last"]);
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(
            report.warnings[0].contains("/broken"),
            "{:?}",
            report.warnings
        );
        assert_eq!(
            ops.created,
            ["/first", "/broken", "/last"],
            "each mountpoint is created before its mount, even the one that fails"
        );
        assert_eq!(
            ops.calls.len(),
            3,
            "and the third entry was still attempted"
        );
    }

    /// `nofstab` is the plan's trigger, and it is a whole token rather than a
    /// substring: `nofstabx` is not it.
    #[test]
    fn nofstab_is_a_token_and_only_a_token() {
        assert!(enabled("console=ttyAMA0 rdinit=/sbin/rinit"));
        assert!(!enabled("console=ttyAMA0 nofstab"));
        assert!(!enabled("nofstab console=ttyAMA0"));
        assert!(enabled("console=ttyAMA0 nofstabx"));
    }

    /// A missing `/etc/fstab` is not a failure - and one that exists and cannot be
    /// read is, because that is a file the board meant rinit to use.
    #[test]
    fn a_missing_file_is_nothing_and_an_unreadable_one_is_an_error() {
        let mut ops = Recorder::default();
        let missing = std::env::temp_dir().join("rinit-fstab-does-not-exist");
        let report = run_with(&mut ops, &missing).expect("missing is not an error");
        assert_eq!(report, FstabReport::default());

        let dir = std::env::temp_dir().join(format!("rinit-fstab-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create");
        assert!(
            run_with(&mut ops, &dir).is_err(),
            "a directory is not fstab"
        );
        let _ = std::fs::remove_dir(&dir);
    }
}
