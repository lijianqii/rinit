//! `df` - filesystem space from `/proc/mounts` and `statvfs`.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, read_trimmed, usage_error};

fn unescape(value: &str) -> String {
    // mountinfo uses octal escapes for spaces, tabs and backslashes.
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let digits = &value[index + 1..index + 4];
            if let Ok(code) = u8::from_str_radix(digits, 8) {
                out.push(code as char);
                index += 4;
                continue;
            }
        }
        out.push(value[index..].chars().next().unwrap_or('\0'));
        index += value[index..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
    }
    out
}

fn mount_line(line: &str) -> Option<(String, String)> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 2 {
        return None;
    }
    Some((unescape(fields[0]), unescape(fields[1])))
}

fn human(kib: u64) -> String {
    if kib >= 1024 * 1024 {
        format!("{:.1}G", kib as f64 / (1024.0 * 1024.0))
    } else if kib >= 1024 {
        format!("{:.1}M", kib as f64 / 1024.0)
    } else {
        format!("{kib}K")
    }
}

fn statvfs(path: &str) -> io::Result<(u64, u64, u64)> {
    let cpath = std::ffi::CString::new(path)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let mut vfs = unsafe { std::mem::zeroed::<libc::statvfs>() };
    // SAFETY: `cpath` is NUL-terminated and `vfs` is valid writable storage.
    if unsafe { libc::statvfs(cpath.as_ptr(), &mut vfs) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let frsize = vfs.f_frsize as u64;
    let blocks = vfs.f_blocks as u64;
    let free = vfs.f_bfree as u64;
    let avail = vfs.f_bavail as u64;
    Ok((
        (blocks * frsize) / 1024,
        ((blocks - free) * frsize) / 1024,
        (avail * frsize) / 1024,
    ))
}

fn run<A: AsRef<OsStr>>(args: &[A], root: &std::path::Path, program: &str) -> ExitCode {
    let mut human_flag = false;
    let mut paths: Vec<PathBuf> = Vec::new();
    for arg in args {
        match arg.as_ref().to_str() {
            Some("-h") => human_flag = true,
            Some("-k") => human_flag = false,
            Some("-P") => {}
            Some("--help") => {
                println!("Usage: df [-h] [-k] [-P] [PATH...]");
                return ExitCode::from(EXIT_OK);
            }
            Some(value) if value.starts_with('-') => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            _ => paths.push(PathBuf::from(arg.as_ref())),
        }
    }

    let mut mounts = Vec::new();
    match read_trimmed(&root.join("proc/mounts")) {
        Ok(text) => {
            for line in text.lines() {
                if let Some((device, mount)) = mount_line(line) {
                    mounts.push((device, mount));
                }
            }
        }
        Err(e) => {
            eprintln!("{program}: cannot read /proc/mounts: {e}");
            return ExitCode::from(1);
        }
    }
    let mut failed = false;
    if !paths.is_empty() {
        let mut selected = Vec::new();
        for path in &paths {
            let path = match std::fs::canonicalize(path) {
                Ok(path) => path,
                Err(error) => {
                    eprintln!("{program}: {}: {error}", path.display());
                    failed = true;
                    continue;
                }
            };
            match mount_for(&mounts, &path) {
                Some(entry) => {
                    if !selected.contains(entry) {
                        selected.push(entry.clone());
                    }
                }
                None => {
                    eprintln!("{program}: {}: not found", path.display());
                    failed = true;
                }
            }
        }
        mounts = selected;
    }

    println!("Filesystem     1K-blocks      Used Available Use% Mounted on");
    for (device, mount) in mounts {
        match statvfs(&mount) {
            Ok((total, used, avail)) => {
                let denominator = used.saturating_add(avail);
                let percent = used
                    .saturating_mul(100)
                    .saturating_add(denominator.saturating_sub(1))
                    .checked_div(denominator)
                    .unwrap_or(0);
                let (total_s, used_s, avail_s) = if human_flag {
                    (human(total), human(used), human(avail))
                } else {
                    (total.to_string(), used.to_string(), avail.to_string())
                };
                println!(
                    "{device:<14} {total_s:>10} {used_s:>9} {avail_s:>9} {percent:>3}% {mount}"
                );
            }
            Err(e) => {
                eprintln!("{program}: {mount}: {e}");
                failed = true;
            }
        }
    }
    ExitCode::from(if failed { 1 } else { EXIT_OK })
}

/// The mount point a path belongs to: the longest mount that is a path-prefix
/// of it, with a component boundary so `/mntx` does not match `/mnt`.
fn mount_for<'a>(mounts: &'a [(String, String)], path: &Path) -> Option<&'a (String, String)> {
    mounts
        .iter()
        .filter(|(_, mount)| path.starts_with(Path::new(mount)))
        .max_by_key(|(_, mount)| mount.len())
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(
        std::env::args_os()
            .next()
            .as_deref()
            .and_then(|arg| arg.to_str()),
        "df",
    );
    let Some(args) = rinit_tools::utf8_args(&program) else {
        return ExitCode::from(rinit_tools::EXIT_USAGE);
    };
    let args: Vec<String> = args.into_iter().skip(1).collect();
    run(&args, std::path::Path::new("/"), &program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_escapes_are_decoded() {
        assert_eq!(unescape(r"/tmp/a\040b"), "/tmp/a b");
        assert_eq!(unescape(r"/tmp/a\011b"), "/tmp/a\tb");
    }

    #[test]
    fn mount_lines_need_device_and_target() {
        assert_eq!(
            mount_line("/dev/vda / ext4 rw 0 0"),
            Some(("/dev/vda".to_string(), "/".to_string()))
        );
        assert_eq!(mount_line("too-few"), None);
    }
}

#[cfg(test)]
mod root_tests {
    use super::*;

    fn test_root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-df-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("proc")).expect("proc");
        std::fs::create_dir_all(root.join("mnt")).expect("mnt");
        root
    }

    #[test]
    fn run_lists_a_fake_mount_and_filters_paths() {
        let root = test_root("run");
        let mount = root.join("mnt");
        std::fs::create_dir(mount.join("child")).expect("child");
        let line = format!("tmpfs {} tmpfs rw 0 0\n", mount.display());
        std::fs::write(root.join("proc/mounts"), line).expect("mounts");
        assert_eq!(run(&[] as &[String], &root, "df"), ExitCode::from(0));
        assert_eq!(run(&["-h".to_string()], &root, "df"), ExitCode::from(0));
        assert_eq!(
            run(&["-k".to_string(), "-P".to_string()], &root, "df"),
            ExitCode::from(0)
        );
        assert_eq!(
            run(&[mount.display().to_string()], &root, "df"),
            ExitCode::from(0)
        );
        assert_eq!(
            run(&[format!("{}/child", mount.display())], &root, "df"),
            ExitCode::from(0),
            "a path inside a mount resolves to that mount"
        );
        assert_eq!(
            run(&["/not-mounted".to_string()], &root, "df"),
            ExitCode::from(1),
            "an unknown path is a failure, not an empty success"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn run_rejects_bad_options_and_missing_proc() {
        let root = test_root("missing");
        std::fs::remove_file(root.join("proc/mounts")).ok();
        assert_eq!(run(&[] as &[String], &root, "df"), ExitCode::from(1));
        assert_eq!(run(&["--help".to_string()], &root, "df"), ExitCode::from(0));
        assert_eq!(run(&["--bad".to_string()], &root, "df"), ExitCode::from(2));
        let _ = std::fs::remove_dir_all(root);
    }
}
