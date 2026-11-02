//! `df` - filesystem space from `/proc/mounts` and `statvfs`.

use std::io;
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

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(std::env::args().next().as_deref(), "df");
    let mut human_flag = false;
    let mut paths = Vec::new();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-h" => human_flag = true,
            "-k" => human_flag = false,
            "-P" => {}
            "--help" => {
                println!("Usage: df [-h] [-k] [-P] [PATH...]");
                return ExitCode::from(EXIT_OK);
            }
            value if value.starts_with('-') => {
                return ExitCode::from(usage_error(&program, &format!("unknown option '{value}'")));
            }
            value => paths.push(value.to_string()),
        }
    }

    let mut mounts = Vec::new();
    match read_trimmed(std::path::Path::new("/proc/mounts")) {
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
    if !paths.is_empty() {
        mounts.retain(|(_, mount)| paths.iter().any(|path| path == mount));
    }

    println!("Filesystem     1K-blocks      Used Available Use% Mounted on");
    for (device, mount) in mounts {
        match statvfs(&mount) {
            Ok((total, used, avail)) => {
                let percent = used.saturating_mul(100).checked_div(total).unwrap_or(0);
                let (total_s, used_s, avail_s) = if human_flag {
                    (human(total), human(used), human(avail))
                } else {
                    (total.to_string(), used.to_string(), avail.to_string())
                };
                println!(
                    "{device:<14} {total_s:>10} {used_s:>9} {avail_s:>9} {percent:>3}% {mount}"
                );
            }
            Err(e) => eprintln!("{program}: {mount}: {e}"),
        }
    }
    ExitCode::from(EXIT_OK)
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
