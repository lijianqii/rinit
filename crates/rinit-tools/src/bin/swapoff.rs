//! `swapoff` - deactivate swap devices from `/proc/swaps`, `/etc/fstab` or by path.

use std::ffi::{CString, OsStr, OsString};
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use std::process::ExitCode;

#[cfg(test)]
use rinit_tools::EXIT_USAGE;
use rinit_tools::{EXIT_OK, fstab, program_name, usage_error};

fn swapoff_one(path: &OsStr, verbose: bool) -> io::Result<()> {
    let path_c = CString::new(path.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "swap device contains NUL"))?;
    // SAFETY: `path_c` is a valid NUL-terminated path.
    let rc = unsafe { libc::swapoff(path_c.as_ptr()) };
    if rc == 0 {
        if verbose {
            let mut stdout = io::stdout().lock();
            stdout.write_all(path.as_bytes())?;
            stdout.write_all(b"\n")?;
        }
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn active_swaps(root: &Path) -> io::Result<Vec<OsString>> {
    let text = std::fs::read(root.join("proc/swaps"))?;
    let mut devices = Vec::new();
    for line in text.split(|byte| *byte == b'\n') {
        let Some(field) = line
            .split(|byte| byte.is_ascii_whitespace())
            .find(|field| !field.is_empty())
        else {
            continue;
        };
        // The header starts with a name, every device with `/`. This also
        // tolerates a missing header in a test fixture.
        if field.first() == Some(&b'/') {
            devices.push(OsString::from_vec(field.to_vec()));
        }
    }
    Ok(devices)
}

fn already_inactive(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EINVAL) | Some(libc::ENOENT)
    )
}

fn deactivate_all(root: &Path, verbose: bool, program: &str) -> bool {
    let mut failed = false;

    // An active swap device is not necessarily listed in fstab (it may have
    // been enabled by hand), so `-a` starts from the kernel's own list.
    match active_swaps(root) {
        Ok(devices) => {
            for device in devices {
                if let Err(error) = swapoff_one(&device, verbose)
                    && !already_inactive(&error)
                {
                    eprintln!("{program}: {}: {error}", device.to_string_lossy());
                    failed = true;
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            eprintln!("{program}: {}: {error}", root.join("proc/swaps").display());
            failed = true;
        }
    }

    // BusyBox processes fstab even for `noauto` entries on swapoff, so an
    // explicitly activated entry is disabled too.
    match fstab::read_swap_entries(root, true) {
        Ok((entries, warnings)) => {
            for warning in warnings {
                eprintln!("{program}: {warning}");
                failed = true;
            }
            for entry in entries {
                if let Err(error) = swapoff_one(&entry.device, verbose)
                    && !already_inactive(&error)
                {
                    eprintln!("{program}: {}: {error}", entry.device.to_string_lossy());
                    failed = true;
                }
            }
        }
        Err(error) => {
            eprintln!("{program}: {error}");
            failed = true;
        }
    }
    failed
}

fn run<A: AsRef<OsStr>>(args: &[A], root: &Path, program: &str) -> ExitCode {
    let mut all = false;
    let mut verbose = false;
    let mut devices: Vec<OsString> = Vec::new();
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        match arg.as_ref().to_str() {
            Some("-h" | "--help") => {
                println!("Usage: swapoff [-a] [-v] [DEVICE...]");
                return ExitCode::from(EXIT_OK);
            }
            Some("-a" | "--all") => all = true,
            Some("-v" | "--verbose") => verbose = true,
            Some(value) if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            _ => devices.push(OsString::from(arg.as_ref())),
        }
        index += 1;
    }

    if !all && devices.is_empty() {
        return ExitCode::from(usage_error(program, "a device or -a is required"));
    }

    let mut failed = false;
    if all {
        failed |= deactivate_all(root, verbose, program);
    }
    // Like BusyBox, devices named on the command line are still processed
    // after `-a`; they are not quieted.
    for device in devices {
        if let Err(error) = swapoff_one(&device, verbose) {
            eprintln!("{program}: {}: {error}", device.to_string_lossy());
            failed = true;
        }
    }
    ExitCode::from(if failed { 1 } else { EXIT_OK })
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(
        std::env::args_os()
            .next()
            .as_deref()
            .and_then(|arg| arg.to_str()),
        "swapoff",
    );
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    run(&args, Path::new("/"), &program)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-swapoff-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("etc")).expect("etc");
        std::fs::create_dir_all(root.join("proc")).expect("proc");
        root
    }

    #[test]
    fn active_swaps_reads_only_device_lines() {
        let root = root("proc");
        std::fs::write(
            root.join("proc/swaps"),
            "Filename Type Size Used Priority\n\
             /swapfile file 32764 0 -2\n\
             \n\
             /dev/vda2 partition 1024 0 -3\n",
        )
        .unwrap();
        let devices = active_swaps(&root).expect("proc/swaps");
        assert_eq!(
            devices,
            [OsString::from("/swapfile"), OsString::from("/dev/vda2")]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn run_handles_help_and_bad_input() {
        let root = root("basic");
        assert_eq!(
            run(&["--help".to_string()] as &[String], &root, "swapoff"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["--bad".to_string()] as &[String], &root, "swapoff"),
            ExitCode::from(EXIT_USAGE)
        );
        assert_eq!(
            run(&[] as &[String], &root, "swapoff"),
            ExitCode::from(EXIT_USAGE)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn all_ignores_missing_or_already_inactive_entries() {
        let root = root("all");
        std::fs::write(
            root.join("etc/fstab"),
            "/no/such/swapfile none swap sw 0 0\n\
             /spare none swap sw,noauto 0 0\n",
        )
        .unwrap();
        // As root an absent target is ENOENT, which `-a` ignores; an
        // unprivileged test process gets EPERM before that check.
        let expected = if unsafe { libc::geteuid() } == 0 {
            ExitCode::from(EXIT_OK)
        } else {
            ExitCode::from(1)
        };
        assert_eq!(
            run(
                &["-a".to_string(), "-v".to_string()] as &[String],
                &root,
                "swapoff"
            ),
            expected
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn only_missing_or_inactive_errors_are_quiet() {
        assert!(already_inactive(&io::Error::from_raw_os_error(
            libc::ENOENT
        )));
        assert!(already_inactive(&io::Error::from_raw_os_error(
            libc::EINVAL
        )));
        assert!(!already_inactive(&io::Error::from_raw_os_error(
            libc::EPERM
        )));
    }

    #[test]
    fn all_reports_malformed_fstab_lines() {
        let root = root("malformed");
        std::fs::write(root.join("etc/fstab"), "broken\n").unwrap();
        assert_eq!(
            run(&["-a".to_string()] as &[String], &root, "swapoff"),
            ExitCode::from(1)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_missing_fstab_fails_all_mode() {
        let root = root("nofstab");
        assert_eq!(
            run(&["-a".to_string()] as &[String], &root, "swapoff"),
            ExitCode::from(1)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_direct_device_failure_is_reported() {
        let root = root("device");
        let device = root.join("not-active");
        let code = run(
            &[device.as_os_str().to_os_string()] as &[OsString],
            &root,
            "swapoff",
        );
        assert_ne!(code, ExitCode::from(EXIT_OK));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_nul_in_the_path_is_an_error() {
        let path = OsString::from_vec(vec![b'/', 0, b'x']);
        let error = swapoff_one(&path, false).expect_err("NUL path");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
