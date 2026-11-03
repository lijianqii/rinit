//! `stat` - display file status.

use std::fs::{self, Metadata};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
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

fn render(format: &str, name: &str, meta: &Metadata) -> String {
    let mut out = String::new();
    let mut chars = format.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        let Some(directive) = chars.next() else {
            out.push('%');
            break;
        };
        match directive {
            '%' => out.push('%'),
            'n' => out.push_str(name),
            's' => out.push_str(&meta.size().to_string()),
            'a' => out.push_str(&format!("{:o}", meta.mode() & 0o7777)),
            'f' => out.push_str(&format!("{:x}", meta.mode())),
            'u' => out.push_str(&meta.uid().to_string()),
            'U' => out.push_str(&user_label(meta.uid())),
            'g' => out.push_str(&meta.gid().to_string()),
            'G' => out.push_str(&group_label(meta.gid())),
            'h' => out.push_str(&meta.nlink().to_string()),
            'i' => out.push_str(&meta.ino().to_string()),
            'd' => out.push_str(&meta.dev().to_string()),
            'D' => out.push_str(&format!("{:x}", meta.dev())),
            'F' => out.push_str(file_type(meta)),
            'X' => out.push_str(&meta.atime().to_string()),
            'Y' => out.push_str(&meta.mtime().to_string()),
            'Z' => out.push_str(&meta.ctime().to_string()),
            'y' => out.push_str(&human_time(meta.mtime())),
            other => {
                out.push('%');
                out.push(other);
            }
        }
    }
    out
}

fn run(args: &[String], program: &str) -> ExitCode {
    let mut follow = false;
    let mut format = DEFAULT_FORMAT.to_string();
    let mut paths: Vec<String> = Vec::new();
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "--help" => {
                println!("Usage: stat [-L] [-c FORMAT] [-t] FILE...");
                return ExitCode::from(EXIT_OK);
            }
            "-L" | "--dereference" => follow = true,
            "-t" | "--terse" => format = TERSE_FORMAT.to_string(),
            "-c" | "--format" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return ExitCode::from(usage_error(program, "-c needs a format"));
                };
                format = value.clone();
            }
            value if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            value => paths.push(value.to_string()),
        }
        index += 1;
    }
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
            Ok(meta) => print!("{}", render(&format, &name, &meta)),
            Err(error) => {
                eprintln!("{program}: {name}: {error}");
                failed = true;
            }
        }
    }
    ExitCode::from(if failed { 1 } else { EXIT_OK })
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(std::env::args().next().as_deref(), "stat");
    let args: Vec<String> = std::env::args().skip(1).collect();
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
        let text = render("%n|%s|%a|%F|%u|%g|%h|%i", "file", &meta);
        assert!(text.starts_with("file|3|"));
        assert!(text.contains("regular file"));
        assert_eq!(render("%%", "file", &meta), "%");
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
        assert_eq!(run(&[], "stat"), ExitCode::from(rinit_tools::EXIT_USAGE));
    }
}
