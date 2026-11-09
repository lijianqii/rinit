//! `mount` - the small, explicit subset the device image needs.

use std::ffi::{CString, OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, fstab, program_name, read_trimmed, usage_error};

#[derive(Default)]
struct ParsedOptions {
    flags: libc::c_ulong,
    data: Vec<String>,
}

fn apply_options(parsed: &mut ParsedOptions, spec: &str) {
    for option in spec.split(',').filter(|option| !option.is_empty()) {
        match option {
            "defaults" | "auto" | "noauto" | "user" | "users" | "owner" | "group" => {}
            "ro" => parsed.flags |= libc::MS_RDONLY,
            "rw" => parsed.flags &= !libc::MS_RDONLY,
            "nosuid" => parsed.flags |= libc::MS_NOSUID,
            "suid" => parsed.flags &= !libc::MS_NOSUID,
            "nodev" => parsed.flags |= libc::MS_NODEV,
            "dev" => parsed.flags &= !libc::MS_NODEV,
            "noexec" => parsed.flags |= libc::MS_NOEXEC,
            "exec" => parsed.flags &= !libc::MS_NOEXEC,
            "sync" => parsed.flags |= libc::MS_SYNCHRONOUS,
            "async" => parsed.flags &= !libc::MS_SYNCHRONOUS,
            "noatime" => parsed.flags |= libc::MS_NOATIME,
            "atime" => parsed.flags &= !libc::MS_NOATIME,
            "nodiratime" => parsed.flags |= libc::MS_NODIRATIME,
            "diratime" => parsed.flags &= !libc::MS_NODIRATIME,
            "relatime" => parsed.flags |= libc::MS_RELATIME,
            "strictatime" => parsed.flags |= libc::MS_STRICTATIME,
            "remount" => parsed.flags |= libc::MS_REMOUNT,
            "bind" => parsed.flags |= libc::MS_BIND,
            "rbind" => parsed.flags |= libc::MS_BIND | libc::MS_REC,
            "rec" => parsed.flags |= libc::MS_REC,
            other => parsed.data.push(other.to_string()),
        }
    }
}

fn parse_options(spec: &str) -> ParsedOptions {
    let mut parsed = ParsedOptions::default();
    apply_options(&mut parsed, spec);
    parsed
}

fn cstring(value: &OsStr, what: &str) -> Result<CString, String> {
    CString::new(value.as_bytes()).map_err(|_| format!("{what} contains NUL"))
}

fn mount_one(
    source: &OsStr,
    target: &OsStr,
    fstype: Option<&str>,
    options: &ParsedOptions,
) -> Result<(), String> {
    let source_c = cstring(source, "source")?;
    let target_c = cstring(target, "target")?;
    let fstype_c = match fstype {
        Some(value) if !value.is_empty() && value != "none" => {
            Some(cstring(OsStr::new(value), "filesystem type")?)
        }
        _ => None,
    };
    let data = options.data.join(",");
    let data_c = if data.is_empty() {
        None
    } else {
        Some(cstring(OsStr::new(&data), "mount options")?)
    };
    // SAFETY: every pointer is borrowed from a NUL-terminated CString for the
    // duration of the call; an absent type/data is a null pointer.
    let rc = unsafe {
        libc::mount(
            source_c.as_ptr(),
            target_c.as_ptr(),
            fstype_c
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr()),
            options.flags,
            data_c
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr().cast()),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

fn list_mounts(root: &std::path::Path) -> Result<(), String> {
    let text = read_trimmed(&root.join("proc/mounts")).map_err(|e| format!("/proc/mounts: {e}"))?;
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 {
            continue;
        }
        println!(
            "{} on {} type {} ({})",
            rinit_runtime::mounts::unescape_mount_field(fields[0]),
            rinit_runtime::mounts::unescape_mount_field(fields[1]),
            fields[2],
            fields[3]
        );
    }
    Ok(())
}

fn mount_fstab(
    root: &std::path::Path,
    fstype_filter: Option<&str>,
    read_only: Option<bool>,
    option_specs: &[String],
) -> Result<(), String> {
    let text =
        std::fs::read_to_string(root.join("etc/fstab")).map_err(|e| format!("/etc/fstab: {e}"))?;
    let (entries, warnings) = fstab::parse(&text);
    let mut fail = warnings.len();
    for warning in warnings {
        eprintln!("mount: {warning}");
    }
    for entry in entries {
        // `swap` is an fstab entry, but switch activation belongs to
        // `swapon -a`; passing it to `mount(2)` against `none` always fails.
        if entry.fstype == "swap" || fstab::has_option(&entry.options, "noauto") {
            continue;
        }
        if fstype_filter.is_some_and(|filter| filter != entry.fstype) {
            continue;
        }
        let mut options = parse_options(&entry.options);
        // Command-line `-r`/`-w` and `-o` override the fstab entry, with later
        // options winning inside the same `-o` list too.
        if let Some(read_only) = read_only {
            if read_only {
                options.flags |= libc::MS_RDONLY;
            } else {
                options.flags &= !libc::MS_RDONLY;
            }
        }
        for spec in option_specs {
            apply_options(&mut options, spec);
        }
        if let Err(e) = mount_one(
            &entry.device,
            &entry.mountpoint,
            Some(&entry.fstype),
            &options,
        ) {
            eprintln!("mount: {}: {e}", entry.mountpoint.to_string_lossy());
            fail += 1;
        }
    }
    if fail == 0 {
        Ok(())
    } else {
        Err(format!("{fail} mount(s) failed"))
    }
}

fn run<A: AsRef<OsStr>>(args: &[A], root: &std::path::Path, program: &str) -> ExitCode {
    if args.is_empty() {
        return match list_mounts(root) {
            Ok(()) => ExitCode::from(EXIT_OK),
            Err(e) => {
                eprintln!("{program}: {e}");
                ExitCode::from(1)
            }
        };
    }

    let mut fstype: Option<String> = None;
    let mut options = ParsedOptions::default();
    let mut read_only: Option<bool> = None;
    let mut option_specs: Vec<String> = Vec::new();
    let mut all = false;
    let mut positional: Vec<OsString> = Vec::new();
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        match arg.as_ref().to_str() {
            Some("-h" | "--help") => {
                println!("Usage: mount [-a] [-t TYPE] [-o OPTIONS] [DEVICE] DIR");
                return ExitCode::from(EXIT_OK);
            }
            Some("-a" | "--all") => all = true,
            Some("-n") => {}
            Some("-r") => {
                options.flags |= libc::MS_RDONLY;
                read_only = Some(true);
            }
            Some("-w") => {
                options.flags &= !libc::MS_RDONLY;
                read_only = Some(false);
            }
            Some("-t") => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return ExitCode::from(usage_error(program, "-t needs a type"));
                };
                let Some(value) = value.as_ref().to_str() else {
                    return ExitCode::from(usage_error(program, "-t needs a UTF-8 type"));
                };
                fstype = Some(value.to_string());
            }
            Some("-o") => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return ExitCode::from(usage_error(program, "-o needs options"));
                };
                let Some(value) = value.as_ref().to_str() else {
                    return ExitCode::from(usage_error(program, "-o needs UTF-8 options"));
                };
                apply_options(&mut options, value);
                option_specs.push(value.to_string());
            }
            Some(value) if value.starts_with('-') => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            _ => positional.push(OsString::from(arg.as_ref())),
        }
        index += 1;
    }

    if all {
        return match mount_fstab(root, fstype.as_deref(), read_only, &option_specs) {
            Ok(()) => ExitCode::from(EXIT_OK),
            Err(e) => {
                eprintln!("{program}: {e}");
                ExitCode::from(1)
            }
        };
    }
    let (source, target) = match positional.as_slice() {
        [] => {
            return match list_mounts(root) {
                Ok(()) => ExitCode::from(EXIT_OK),
                Err(e) => {
                    eprintln!("{program}: {e}");
                    ExitCode::from(1)
                }
            };
        }
        [source, target] => (source.as_os_str(), target.as_os_str()),
        [target] => (OsStr::new(""), target.as_os_str()),
        _ => return ExitCode::from(usage_error(program, "too many arguments")),
    };
    match mount_one(source, target, fstype.as_deref(), &options) {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(e) => {
            eprintln!("{program}: {}: {e}", target.to_string_lossy());
            ExitCode::from(1)
        }
    }
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(
        std::env::args_os()
            .next()
            .as_deref()
            .and_then(|arg| arg.to_str()),
        "mount",
    );
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    run(&args, std::path::Path::new("/"), &program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_split_flags_from_data() {
        let parsed = parse_options("ro,noexec,mode=755,size=64k");
        assert_ne!(parsed.flags & libc::MS_RDONLY, 0);
        assert_ne!(parsed.flags & libc::MS_NOEXEC, 0);
        assert_eq!(parsed.data, ["mode=755", "size=64k"]);
    }

    #[test]
    fn rw_clears_the_read_only_flag() {
        let parsed = parse_options("ro,rw");
        assert_eq!(parsed.flags & libc::MS_RDONLY, 0);
    }

    #[test]
    fn a_later_option_spec_clears_an_earlier_one() {
        let mut parsed = parse_options("ro,noexec");
        apply_options(&mut parsed, "rw,exec");
        assert_eq!(parsed.flags & libc::MS_RDONLY, 0);
        assert_eq!(parsed.flags & libc::MS_NOEXEC, 0);
    }

    #[test]
    fn nul_paths_are_rejected() {
        assert!(cstring(OsStr::new("a\0b"), "path").is_err());
        assert!(cstring(OsStr::new("ok"), "path").is_ok());
    }
}

#[cfg(test)]
mod root_tests {
    use super::*;

    fn test_root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-mount-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("proc")).expect("proc");
        std::fs::create_dir_all(root.join("etc")).expect("etc");
        root
    }

    #[test]
    fn run_lists_fake_mounts_and_handles_bad_options() {
        let root = test_root("list");
        std::fs::write(
            root.join("proc/mounts"),
            "/dev/vda / ext4 rw 0 0\ntmpfs /run tmpfs rw 0 0\n",
        )
        .expect("mounts");
        assert_eq!(run(&[] as &[String], &root, "mount"), ExitCode::from(0));
        assert_eq!(
            run(&["--help".to_string()], &root, "mount"),
            ExitCode::from(0)
        );
        assert_eq!(
            run(&["--bad".to_string()], &root, "mount"),
            ExitCode::from(2)
        );
        assert_eq!(run(&["-t".to_string()], &root, "mount"), ExitCode::from(2));
        assert_eq!(run(&["-o".to_string()], &root, "mount"), ExitCode::from(2));
        assert_eq!(
            run(
                &["a".to_string(), "b".to_string(), "c".to_string()],
                &root,
                "mount"
            ),
            ExitCode::from(2)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn run_mounts_a_noauto_fstab_without_syscalls() {
        let root = test_root("fstab");
        std::fs::write(
            root.join("etc/fstab"),
            "tmpfs /tmp tmpfs rw,noauto 0 0\n/swapfile none swap sw 0 0\n# comment\n",
        )
        .expect("fstab");
        assert_eq!(run(&["-a".to_string()], &root, "mount"), ExitCode::from(0));
        std::fs::write(root.join("etc/fstab"), "broken line\n").expect("fstab");
        assert_eq!(run(&["-a".to_string()], &root, "mount"), ExitCode::from(1));
        std::fs::remove_file(root.join("etc/fstab")).ok();
        assert_eq!(run(&["-a".to_string()], &root, "mount"), ExitCode::from(1));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_mounts_and_missing_target_are_errors() {
        let root = test_root("missing");
        assert_eq!(run(&[] as &[String], &root, "mount"), ExitCode::from(1));
        assert_eq!(
            run(
                &["tmpfs".to_string(), "/no/such/target".to_string()],
                &root,
                "mount"
            ),
            ExitCode::from(1)
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
