//! `swapon` - activate swap devices listed in `/etc/fstab` or by path.

use std::ffi::{CString, OsStr, OsString};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::ExitCode;

#[cfg(test)]
use rinit_tools::EXIT_USAGE;
use rinit_tools::{EXIT_OK, fstab, program_name, usage_error};

const SWAP_FLAG_PREFER: libc::c_int = 0x8000;
const SWAP_FLAG_PRIO_MASK: libc::c_int = 0x7fff;
const SWAP_FLAG_DISCARD: libc::c_int = 0x10000;
const SWAP_FLAG_DISCARD_ONCE: libc::c_int = 0x20000;
const SWAP_FLAG_DISCARD_PAGES: libc::c_int = 0x40000;

fn swapon_one(path: &OsStr, flags: libc::c_int, verbose: bool) -> io::Result<()> {
    let path_c = CString::new(path.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "swap device contains NUL"))?;
    // SAFETY: `path_c` is a valid NUL-terminated path and `flags` is a scalar.
    let rc = unsafe { libc::swapon(path_c.as_ptr(), flags) };
    if rc == 0 {
        if verbose {
            println!("{}", path.to_string_lossy());
        }
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn parse_priority(value: &str) -> Result<u16, String> {
    match value.parse::<u16>() {
        Ok(priority) if priority <= SWAP_FLAG_PRIO_MASK as u16 => Ok(priority),
        _ => Err(format!(
            "priority '{value}' must be between 0 and {}",
            SWAP_FLAG_PRIO_MASK
        )),
    }
}

fn priority_flags(priority: u16) -> libc::c_int {
    SWAP_FLAG_PREFER | libc::c_int::from(priority)
}

fn priority_from_options(options: &str) -> Result<Option<u16>, String> {
    let mut priority = None;
    for option in options.split(',') {
        if let Some(value) = option
            .strip_prefix("pri=")
            .or_else(|| option.strip_prefix("priority="))
        {
            priority = Some(
                parse_priority(value)
                    .map_err(|error| format!("invalid fstab priority: {error}"))?,
            );
        }
    }
    Ok(priority)
}

fn discard_flags(options: &str) -> Result<libc::c_int, String> {
    let mut flags = 0;
    for option in options.split(',') {
        match option {
            "discard" => flags = SWAP_FLAG_DISCARD,
            "discard=once" => flags = SWAP_FLAG_DISCARD | SWAP_FLAG_DISCARD_ONCE,
            "discard=pages" => flags = SWAP_FLAG_DISCARD | SWAP_FLAG_DISCARD_PAGES,
            value if value.starts_with("discard=") => {
                return Err(format!("unsupported discard policy '{value}'"));
            }
            _ => {}
        }
    }
    Ok(flags)
}

fn entry_flags(options: &str, override_priority: Option<u16>) -> Result<libc::c_int, String> {
    let priority = match override_priority {
        Some(priority) => Some(priority),
        None => priority_from_options(options)?,
    };
    Ok(priority.map(priority_flags).unwrap_or(0) | discard_flags(options)?)
}

fn show_swaps(root: &Path, program: &str) -> ExitCode {
    match std::fs::read(root.join("proc/swaps")) {
        Ok(mut text) => {
            if !text.ends_with(b"\n") {
                text.push(b'\n');
            }
            match io::stdout().write_all(&text) {
                Ok(()) => ExitCode::from(EXIT_OK),
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::from(EXIT_OK),
                Err(error) => {
                    eprintln!("{program}: cannot write /proc/swaps: {error}");
                    ExitCode::from(1)
                }
            }
        }
        Err(error) => {
            eprintln!("{program}: {}: {error}", root.join("proc/swaps").display());
            ExitCode::from(1)
        }
    }
}

fn activate_all(root: &Path, override_priority: Option<u16>, verbose: bool, program: &str) -> bool {
    let (entries, warnings) = match fstab::read_swap_entries(root, false) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("{program}: {error}");
            return true;
        }
    };
    let mut failed = false;
    for warning in warnings {
        eprintln!("{program}: {warning}");
        failed = true;
    }
    for entry in entries {
        let flags = match entry_flags(&entry.options, override_priority) {
            Ok(flags) => flags,
            Err(error) => {
                eprintln!("{program}: {}: {error}", entry.device.to_string_lossy());
                failed = true;
                continue;
            }
        };
        if let Err(error) = swapon_one(&entry.device, flags, verbose) {
            // `-a` is a bulk operation: an already-active entry is not a
            // reason to fail the whole boot.
            if error.raw_os_error() != Some(libc::EBUSY) {
                eprintln!("{program}: {}: {error}", entry.device.to_string_lossy());
                failed = true;
            }
        }
    }
    failed
}

fn run<A: AsRef<OsStr>>(args: &[A], root: &Path, program: &str) -> ExitCode {
    let mut all = false;
    let mut summary = false;
    let mut verbose = false;
    let mut priority: Option<u16> = None;
    let mut devices: Vec<OsString> = Vec::new();
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        match arg.as_ref().to_str() {
            Some("-h" | "--help") => {
                println!("Usage: swapon [-a] [-s] [-p PRIORITY] [-v] [DEVICE...]");
                return ExitCode::from(EXIT_OK);
            }
            Some("-a" | "--all") => all = true,
            Some("-s" | "--summary") => summary = true,
            Some("-v" | "--verbose") => verbose = true,
            Some("-p" | "--priority") => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return ExitCode::from(usage_error(program, "-p needs a priority"));
                };
                let Some(value) = value.as_ref().to_str() else {
                    return ExitCode::from(usage_error(program, "-p needs a UTF-8 priority"));
                };
                match parse_priority(value) {
                    Ok(value) => priority = Some(value),
                    Err(error) => return ExitCode::from(usage_error(program, &error)),
                }
            }
            Some(value) if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            _ => devices.push(OsString::from(arg.as_ref())),
        }
        index += 1;
    }

    if summary {
        return show_swaps(root, program);
    }
    if !all && devices.is_empty() {
        return ExitCode::from(usage_error(program, "a device or -a is required"));
    }

    let mut failed = false;
    if all {
        failed |= activate_all(root, priority, verbose, program);
    }
    for device in devices {
        let flags = priority.map(priority_flags).unwrap_or(0);
        if let Err(error) = swapon_one(&device, flags, verbose) {
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
        "swapon",
    );
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    run(&args, Path::new("/"), &program)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-swapon-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("etc")).expect("etc");
        std::fs::create_dir_all(root.join("proc")).expect("proc");
        root
    }

    #[test]
    fn run_handles_help_summary_and_bad_input() {
        let root = root("basic");
        std::fs::write(
            root.join("proc/swaps"),
            "Filename Type Size Used Priority\n/swapfile file 32764 0 -2\n",
        )
        .unwrap();
        assert_eq!(
            run(&["--help".to_string()] as &[String], &root, "swapon"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["-s".to_string()] as &[String], &root, "swapon"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["--bad".to_string()] as &[String], &root, "swapon"),
            ExitCode::from(EXIT_USAGE)
        );
        assert_eq!(
            run(&[] as &[String], &root, "swapon"),
            ExitCode::from(EXIT_USAGE)
        );
        assert_eq!(
            run(&["-p".to_string()] as &[String], &root, "swapon"),
            ExitCode::from(EXIT_USAGE)
        );
        assert_eq!(
            run(
                &["-p".to_string(), "40000".to_string()] as &[String],
                &root,
                "swapon"
            ),
            ExitCode::from(EXIT_USAGE)
        );
        assert_eq!(
            run(
                &["-p".to_string(), "abc".to_string()] as &[String],
                &root,
                "swapon"
            ),
            ExitCode::from(EXIT_USAGE)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_summary_file_is_reported() {
        let root = root("nosummary");
        assert_eq!(
            run(&["-s".to_string()] as &[String], &root, "swapon"),
            ExitCode::from(1)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn flags_come_from_the_command_line_or_fstab() {
        assert_eq!(entry_flags("sw,pri=5", None), Ok(SWAP_FLAG_PREFER | 5));
        assert_eq!(entry_flags("sw,priority=7", None), Ok(SWAP_FLAG_PREFER | 7));
        assert_eq!(entry_flags("sw", Some(9)), Ok(SWAP_FLAG_PREFER | 9));
        assert_eq!(
            entry_flags("sw,pri=5", Some(9)),
            Ok(SWAP_FLAG_PREFER | 9),
            "the command line overrides fstab"
        );
        assert_eq!(entry_flags("sw,discard", None), Ok(SWAP_FLAG_DISCARD));
        assert_eq!(
            entry_flags("sw,discard=once", None),
            Ok(SWAP_FLAG_DISCARD | SWAP_FLAG_DISCARD_ONCE)
        );
        assert_eq!(
            entry_flags("sw,discard=pages", None),
            Ok(SWAP_FLAG_DISCARD | SWAP_FLAG_DISCARD_PAGES)
        );
        assert!(entry_flags("sw,pri=32768", None).is_err());
        assert!(entry_flags("sw,pri=abc", None).is_err());
        assert!(entry_flags("sw,discard=sometimes", None).is_err());
    }

    #[test]
    fn all_reports_fstab_warnings_and_failed_devices() {
        let root = root("fstab");
        std::fs::write(
            root.join("etc/fstab"),
            "/dev/vda1 / ext4 defaults 0 1\n\
             broken\n\
             /no/such/swapfile none swap sw,pri=5 0 0\n\
             /spare none swap sw,noauto 0 0\n",
        )
        .unwrap();
        let code = run(
            &["-a".to_string(), "-v".to_string()] as &[String],
            &root,
            "swapon",
        );
        assert_ne!(code, ExitCode::from(EXIT_OK));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_missing_fstab_fails_all_mode() {
        let root = root("nofstab");
        let code = run(&["-a".to_string()] as &[String], &root, "swapon");
        assert_ne!(code, ExitCode::from(EXIT_OK));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_direct_device_is_attempted() {
        let root = root("device");
        let device = root.join("not-a-swap-device");
        std::fs::write(&device, b"x").unwrap();
        let code = run(
            &[device.as_os_str().to_os_string()] as &[OsString],
            &root,
            "swapon",
        );
        assert_ne!(code, ExitCode::from(EXIT_OK));
        let _ = std::fs::remove_dir_all(root);
    }
}
