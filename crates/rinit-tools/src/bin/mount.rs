//! `mount` - the small, explicit subset the device image needs.

use std::ffi::CString;
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, read_trimmed, usage_error};

#[derive(Default)]
struct ParsedOptions {
    flags: libc::c_ulong,
    data: Vec<String>,
}

fn parse_options(spec: &str) -> ParsedOptions {
    let mut parsed = ParsedOptions::default();
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
    parsed
}

fn cstring(value: &str, what: &str) -> Result<CString, String> {
    CString::new(value).map_err(|_| format!("{what} contains NUL"))
}

fn mount_one(
    source: &str,
    target: &str,
    fstype: Option<&str>,
    options: &ParsedOptions,
) -> Result<(), String> {
    let source_c = cstring(source, "source")?;
    let target_c = cstring(target, "target")?;
    let fstype_c = match fstype {
        Some(value) if !value.is_empty() && value != "none" => {
            Some(cstring(value, "filesystem type")?)
        }
        _ => None,
    };
    let data = options.data.join(",");
    let data_c = if data.is_empty() {
        None
    } else {
        Some(cstring(&data, "mount options")?)
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

fn list_mounts() -> Result<(), String> {
    let text = read_trimmed(std::path::Path::new("/proc/mounts"))
        .map_err(|e| format!("/proc/mounts: {e}"))?;
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 {
            continue;
        }
        println!(
            "{} on {} type {} ({})",
            fields[0], fields[1], fields[2], fields[3]
        );
    }
    Ok(())
}

fn mount_fstab() -> Result<(), String> {
    let text = std::fs::read_to_string("/etc/fstab").map_err(|e| format!("/etc/fstab: {e}"))?;
    let mut fail = 0;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 {
            eprintln!("mount: malformed fstab line: {line}");
            fail += 1;
            continue;
        }
        let options = parse_options(fields[3]);
        if fields[3].split(',').any(|option| option == "noauto") {
            continue;
        }
        if let Err(e) = mount_one(fields[0], fields[1], Some(fields[2]), &options) {
            eprintln!("mount: {}: {e}", fields[1]);
            fail += 1;
        }
    }
    if fail == 0 {
        Ok(())
    } else {
        Err(format!("{fail} mount(s) failed"))
    }
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(std::env::args().next().as_deref(), "mount");
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        return match list_mounts() {
            Ok(()) => ExitCode::from(EXIT_OK),
            Err(e) => {
                eprintln!("{program}: {e}");
                ExitCode::from(1)
            }
        };
    }

    let mut fstype: Option<String> = None;
    let mut options = ParsedOptions::default();
    let mut all = false;
    let mut positional = Vec::new();
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("Usage: mount [-a] [-t TYPE] [-o OPTIONS] [DEVICE] DIR");
                return ExitCode::from(EXIT_OK);
            }
            "-a" | "--all" => all = true,
            "-n" => {}
            "-r" => options.flags |= libc::MS_RDONLY,
            "-w" => options.flags &= !libc::MS_RDONLY,
            "-t" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return ExitCode::from(usage_error(&program, "-t needs a type"));
                };
                fstype = Some(value.clone());
            }
            "-o" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return ExitCode::from(usage_error(&program, "-o needs options"));
                };
                let extra = parse_options(value);
                options.flags |= extra.flags;
                options.data.extend(extra.data);
            }
            value if value.starts_with('-') => {
                return ExitCode::from(usage_error(&program, &format!("unknown option '{value}'")));
            }
            value => positional.push(value.to_string()),
        }
        index += 1;
    }

    if all {
        return match mount_fstab() {
            Ok(()) => ExitCode::from(EXIT_OK),
            Err(e) => {
                eprintln!("{program}: {e}");
                ExitCode::from(1)
            }
        };
    }
    let (source, target) = match positional.as_slice() {
        [] => {
            return match list_mounts() {
                Ok(()) => ExitCode::from(EXIT_OK),
                Err(e) => {
                    eprintln!("{program}: {e}");
                    ExitCode::from(1)
                }
            };
        }
        [source, target] => (source.as_str(), target.as_str()),
        [target] => ("", target.as_str()),
        _ => return ExitCode::from(usage_error(&program, "too many arguments")),
    };
    match mount_one(source, target, fstype.as_deref(), &options) {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(e) => {
            eprintln!("{program}: {target}: {e}");
            ExitCode::from(1)
        }
    }
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
    fn nul_paths_are_rejected() {
        assert!(cstring("a\0b", "path").is_err());
        assert!(cstring("ok", "path").is_ok());
    }
}
