//! `stat` - display file status.

use std::ffi::OsStr;
use std::fs::{self, Metadata};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rinit_tools::users;
use rinit_tools::{EXIT_OK, program_name, usage_error};

const DEFAULT_FORMAT: &str = "  File: %n\n  Size: %s\n  Mode: %a\n  Type: %F\n   Uid: %u (%U)\n   Gid: %g (%G)\n Links: %h\n Inode: %i\nDevice: %d\nAccess: %X\nModify: %Y\nChange: %Z\n";
const TERSE_FORMAT: &str = "%n %s %a %u %g %d %i %h %X %Y %Z\n";

fn file_type(meta: &Metadata) -> &'static str {
    let kind = meta.file_type();
    if kind.is_file() {
        "regular file"
    } else if kind.is_dir() {
        "directory"
    } else if kind.is_symlink() {
        "symbolic link"
    } else if kind.is_char_device() {
        "character special file"
    } else if kind.is_block_device() {
        "block special file"
    } else if kind.is_fifo() {
        "fifo"
    } else if kind.is_socket() {
        "socket"
    } else {
        "unknown"
    }
}

fn human_time(seconds: i64) -> String {
    let time = seconds as libc::time_t;
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    // SAFETY: `time` and `tm` are valid local storage for the call.
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        return "????-??-?? ??:??:??".to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

fn user_label(uid: u32) -> String {
    users::user_name(uid).unwrap_or_else(|| "UNKNOWN".to_string())
}

fn group_label(gid: u32) -> String {
    users::group_name(gid).unwrap_or_else(|| "UNKNOWN".to_string())
}

fn render(format: &str, name: impl AsRef<Path>, meta: &Metadata) -> Vec<u8> {
    let name = name.as_ref().as_os_str().as_bytes();
    let mut out = Vec::new();
    let mut chars = format.chars();
    let push = |out: &mut Vec<u8>, text: &str| out.extend_from_slice(text.as_bytes());
    while let Some(ch) = chars.next() {
        if ch != '%' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        let Some(directive) = chars.next() else {
            out.push(b'%');
            break;
        };
        match directive {
            '%' => out.push(b'%'),
            'n' => out.extend_from_slice(name),
            's' => push(&mut out, &meta.size().to_string()),
            'a' => push(&mut out, &format!("{:o}", meta.mode() & 0o7777)),
            'f' => push(&mut out, &format!("{:x}", meta.mode())),
            'u' => push(&mut out, &meta.uid().to_string()),
            'U' => push(&mut out, &user_label(meta.uid())),
            'g' => push(&mut out, &meta.gid().to_string()),
            'G' => push(&mut out, &group_label(meta.gid())),
            'h' => push(&mut out, &meta.nlink().to_string()),
            'i' => push(&mut out, &meta.ino().to_string()),
            'd' => push(&mut out, &meta.dev().to_string()),
            'D' => push(&mut out, &format!("{:x}", meta.dev())),
            'F' => push(&mut out, file_type(meta)),
            'X' => push(&mut out, &meta.atime().to_string()),
            'Y' => push(&mut out, &meta.mtime().to_string()),
            'Z' => push(&mut out, &meta.ctime().to_string()),
            'y' => push(&mut out, &human_time(meta.mtime())),
            other => {
                out.push(b'%');
                let mut buf = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out
}

fn run<A: AsRef<OsStr>>(args: &[A], program: &str) -> ExitCode {
    let mut follow = false;
    let mut custom_format = false;
    let mut format = DEFAULT_FORMAT.to_string();
    let mut paths: Vec<PathBuf> = Vec::new();
    let marker = args.iter().position(|arg| arg.as_ref() == OsStr::new("--"));
    let (options_args, trailing): (&[A], &[A]) = match marker {
        Some(index) => (
            args.get(..index).unwrap_or(&[]),
            args.get(index + 1..).unwrap_or(&[]),
        ),
        None => (args, &[] as &[A]),
    };
    let mut index = 0;
    while let Some(arg) = options_args.get(index) {
        match arg.as_ref().to_str() {
            Some("--help") => {
                println!("Usage: stat [-L] [-c FORMAT] [-t] FILE...");
                return ExitCode::from(EXIT_OK);
            }
            Some("-L" | "--dereference") => follow = true,
            Some("-t" | "--terse") => format = TERSE_FORMAT.to_string(),
            Some("-c" | "--format") => {
                index += 1;
                let Some(value) = options_args.get(index) else {
                    return ExitCode::from(usage_error(program, "-c needs a format"));
                };
                let Some(value) = value.as_ref().to_str() else {
                    return ExitCode::from(usage_error(program, "-c needs a UTF-8 format"));
                };
                format = value.to_string();
                custom_format = true;
            }
            Some(value) if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            _ => paths.push(PathBuf::from(arg.as_ref())),
        }
        index += 1;
    }
    paths.extend(trailing.iter().map(|arg| PathBuf::from(arg.as_ref())));
    if paths.is_empty() {
        return ExitCode::from(usage_error(program, "a file operand is required"));
    }
    let mut failed = false;
    for name in paths {
        let result = if follow {
            fs::metadata(&name)
        } else {
            fs::symlink_metadata(&name)
        };
        match result {
            Ok(meta) => {
                let text = render(&format, &name, &meta);
                let mut stdout = std::io::stdout().lock();
                let result = if custom_format {
                    stdout
                        .write_all(&text)
                        .and_then(|()| stdout.write_all(b"\n"))
                } else {
                    stdout.write_all(&text)
                };
                if let Err(error) = result {
                    if error.kind() == std::io::ErrorKind::BrokenPipe {
                        return ExitCode::from(EXIT_OK);
                    }
                    eprintln!("{program}: cannot write output: {error}");
                    failed = true;
                }
            }
            Err(error) => {
                eprintln!("{program}: {}: {error}", name.display());
                failed = true;
            }
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
        "stat",
    );
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-stat-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn format_directives_render_metadata() {
        let root = temp_root("format");
        let file = root.join("file");
        fs::write(&file, b"abc").expect("file");
        let meta = fs::metadata(&file).expect("meta");
        let text = String::from_utf8(render("%n|%s|%a|%F|%u|%g|%h|%i", "file", &meta))
            .expect("UTF-8 render");
        assert!(text.starts_with("file|3|"));
        assert!(text.contains("regular file"));
        assert_eq!(render("%%", "file", &meta), b"%");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_non_utf8_name_is_rendered_as_bytes() {
        use std::os::unix::ffi::OsStringExt;

        let root = temp_root("raw-name");
        let path = root.join(std::ffi::OsString::from_vec(b"f\xff".to_vec()));
        fs::write(&path, b"x").expect("file");
        let meta = fs::symlink_metadata(&path).expect("meta");
        let rendered = render("%n", &path, &meta);
        assert!(
            rendered.windows(2).any(|window| window == b"f\xff"),
            "{rendered:?}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn symlink_metadata_can_be_followed() {
        let root = temp_root("link");
        let target = root.join("target");
        let link = root.join("link");
        fs::write(&target, b"x").expect("target");
        std::os::unix::fs::symlink(&target, &link).expect("link");
        let link_meta = fs::symlink_metadata(&link).expect("link");
        assert_eq!(file_type(&link_meta), "symbolic link");
        let target_meta = fs::metadata(&link).expect("target");
        assert_eq!(file_type(&target_meta), "regular file");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn run_handles_help_and_missing_files() {
        assert_eq!(
            run(&["--help".to_string()], "stat"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["--bad".to_string()], "stat"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        assert_eq!(
            run(&["/definitely/missing".to_string()], "stat"),
            ExitCode::from(1)
        );
        assert_eq!(
            run(&[] as &[String], "stat"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;
    use std::path::PathBuf;

    fn root(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("rinit-{}-{}-{tag}", "stat", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn run_formats_files_and_reports_missing_ones() {
        let root = root("run");
        let file = root.join("file");
        fs::write(&file, b"x").expect("file");
        let path = file.display().to_string();
        assert_eq!(
            run(std::slice::from_ref(&path), "stat"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["-t".to_string(), path.clone()], "stat"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["-c".to_string(), "%n".to_string(), path], "stat"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&[root.join("missing").display().to_string()], "stat"),
            ExitCode::from(1)
        );
        assert_eq!(
            run(&["--help".to_string()], "stat"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["--bad".to_string()], "stat"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        assert_eq!(
            run(&[] as &[String], "stat"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        assert_eq!(
            run(&["-c".to_string()], "stat"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
