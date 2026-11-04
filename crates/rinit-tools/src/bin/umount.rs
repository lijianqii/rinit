//! `umount` - unmount filesystems.

use std::ffi::{CString, OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, read_trimmed, usage_error};

fn umount_one(target: &OsStr, flags: libc::c_int) -> Result<(), String> {
    let path = CString::new(target.as_bytes()).map_err(|_| "target contains NUL".to_string())?;
    // SAFETY: `path` is NUL-terminated and lives for the call.
    let rc = unsafe { libc::umount2(path.as_ptr(), flags) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

fn targets_from_mounts(text: &str) -> Vec<OsString> {
    let mut targets: Vec<OsString> = text
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .filter(|target| *target != "/")
        .map(OsString::from)
        .collect();
    targets.reverse();
    targets
}

fn all_targets(root: &std::path::Path) -> Result<Vec<OsString>, String> {
    let text = read_trimmed(&root.join("proc/mounts")).map_err(|e| format!("/proc/mounts: {e}"))?;
    Ok(targets_from_mounts(&text))
}

fn run<A: AsRef<OsStr>>(args: &[A], root: &std::path::Path, program: &str) -> ExitCode {
    let mut flags = 0;
    let mut all = false;
    let mut targets: Vec<OsString> = Vec::new();
    for arg in args {
        match arg.as_ref().to_str() {
            Some("-h" | "--help") => {
                println!("Usage: umount [-a] [-l] [-f] TARGET...");
                return ExitCode::from(EXIT_OK);
            }
            Some("-a" | "--all") => all = true,
            Some("-l" | "--lazy") => flags |= libc::MNT_DETACH,
            Some("-f" | "--force") => flags |= libc::MNT_FORCE,
            Some(value) if value.starts_with('-') => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            _ => targets.push(OsString::from(arg.as_ref())),
        }
    }
    if all {
        match all_targets(root) {
            Ok(list) => targets.extend(list),
            Err(e) => {
                eprintln!("{program}: {e}");
                return ExitCode::from(1);
            }
        }
    }
    if targets.is_empty() {
        return ExitCode::from(usage_error(program, "a target is required"));
    }
    let mut fail = 0;
    for target in targets {
        if let Err(e) = umount_one(&target, flags) {
            eprintln!("{program}: {}: {e}", target.to_string_lossy());
            fail += 1;
        }
    }
    ExitCode::from(if fail == 0 { EXIT_OK } else { 1 })
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(
        std::env::args_os()
            .next()
            .as_deref()
            .and_then(|arg| arg.to_str()),
        "umount",
    );
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    run(&args, std::path::Path::new("/"), &program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_targets_skips_root_and_reverses_mount_order() {
        let text = "/dev/vda / ext4 rw 0 0\ntmpfs /run tmpfs rw 0 0\nproc /proc proc rw 0 0\n";
        assert_eq!(targets_from_mounts(text), ["/proc", "/run"]);
    }
}

#[cfg(test)]
mod root_tests {
    use super::*;

    fn test_root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-umount-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("proc")).expect("proc");
        root
    }

    #[test]
    fn run_handles_options_and_missing_mounts() {
        let root = test_root("run");
        assert_eq!(
            run(&["--help".to_string()], &root, "umount"),
            ExitCode::from(0)
        );
        assert_eq!(
            run(&["--bad".to_string()], &root, "umount"),
            ExitCode::from(2)
        );
        assert_eq!(run(&[] as &[String], &root, "umount"), ExitCode::from(2));
        assert_eq!(
            run(&["/no/such/target".to_string()], &root, "umount"),
            ExitCode::from(1)
        );
        assert_eq!(
            run(
                &["-l".to_string(), "/no/such/target".to_string()],
                &root,
                "umount"
            ),
            ExitCode::from(1)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn run_all_reads_fake_mounts_and_reports_failures() {
        let root = test_root("all");
        std::fs::write(
            root.join("proc/mounts"),
            "/dev/vda / ext4 rw 0 0\ntmpfs /run tmpfs rw 0 0\n",
        )
        .expect("mounts");
        assert_eq!(run(&["-a".to_string()], &root, "umount"), ExitCode::from(1));
        let _ = std::fs::remove_dir_all(root);
    }
}
