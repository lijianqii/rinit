//! `ls` - list directory contents.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, usage_error};

#[derive(Clone, Copy, Default)]
struct Options {
    all: bool,
    almost_all: bool,
    long: bool,
    human: bool,
    directory: bool,
}

fn type_char(file_type: &fs::FileType) -> char {
    if file_type.is_dir() {
        'd'
    } else if file_type.is_symlink() {
        'l'
    } else if file_type.is_block_device() {
        'b'
    } else if file_type.is_char_device() {
        'c'
    } else if file_type.is_fifo() {
        'p'
    } else if file_type.is_socket() {
        's'
    } else {
        '-'
    }
}

fn bit(mode: u32, value: u32, yes: char) -> char {
    if mode & value != 0 { yes } else { '-' }
}

fn execute_char(mode: u32, execute: u32, special: u32, set: char) -> char {
    if mode & special != 0 {
        if mode & execute != 0 {
            set
        } else {
            set.to_ascii_uppercase()
        }
    } else if mode & execute != 0 {
        'x'
    } else {
        '-'
    }
}

fn mode_string(meta: &fs::Metadata) -> String {
    let mode = meta.mode();
    format!(
        "{}{}{}{}{}{}{}{}{}{}",
        type_char(&meta.file_type()),
        bit(mode, 0o400, 'r'),
        bit(mode, 0o200, 'w'),
        execute_char(mode, 0o100, 0o4000, 's'),
        bit(mode, 0o040, 'r'),
        bit(mode, 0o020, 'w'),
        execute_char(mode, 0o010, 0o2000, 's'),
        bit(mode, 0o004, 'r'),
        bit(mode, 0o002, 'w'),
        execute_char(mode, 0o001, 0o1000, 't')
    )
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}G", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1}M", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1}K", bytes as f64 / 1024.0)
    } else {
        bytes.to_string()
    }
}

fn format_time(seconds: i64) -> String {
    let time = seconds as libc::time_t;
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    // SAFETY: `time` and `tm` point at valid local storage for the call.
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        return "????-??-?? ??:??".to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

fn format_entry(path: &Path, name: &OsStr, meta: &fs::Metadata, options: Options) -> Vec<u8> {
    if !options.long {
        return name.as_bytes().to_vec();
    }
    let size = if options.human {
        human_size(meta.size())
    } else {
        meta.size().to_string()
    };
    let mut line = format!(
        "{} {:>2} {:>5} {:>5} {:>8} {} ",
        mode_string(meta),
        meta.nlink(),
        meta.uid(),
        meta.gid(),
        size,
        format_time(meta.mtime()),
    )
    .into_bytes();
    line.extend_from_slice(name.as_bytes());
    if meta.file_type().is_symlink()
        && let Ok(target) = fs::read_link(path)
    {
        line.extend_from_slice(b" -> ");
        line.extend_from_slice(target.as_os_str().as_bytes());
    }
    line
}

fn format_dir(path: &Path, options: Options) -> io::Result<Vec<u8>> {
    let mut entries: Vec<(OsString, fs::Metadata)> = Vec::new();
    if options.all {
        entries.push((".".into(), fs::symlink_metadata(path)?));
        // `Path::parent(".")` and `parent("foo")` are both `Some("")`; joining
        // `..` works for both and avoids statting the empty path.
        entries.push(("..".into(), fs::symlink_metadata(path.join(".."))?));
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        let hidden = name.as_bytes().first() == Some(&b'.');
        if hidden && !options.all && !options.almost_all {
            continue;
        }
        let meta = fs::symlink_metadata(entry.path())?;
        entries.push((name, meta));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut out = Vec::new();
    for (name, meta) in entries {
        out.extend(format_entry(&path.join(&name), &name, &meta, options));
        out.push(b'\n');
    }
    Ok(out)
}

fn list_path(path: &Path, display: &OsStr, options: Options) -> io::Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path)?;
    let follows_to_directory = !options.directory
        && !options.long
        && fs::metadata(path)
            .map(|target| target.is_dir())
            .unwrap_or(false);
    if follows_to_directory {
        format_dir(path, options)
    } else {
        let mut out = format_entry(path, display, &meta, options);
        out.push(b'\n');
        Ok(out)
    }
}

/// Write raw listing bytes. A closed pipe ends the command successfully; any
/// other write error is a real failure.
fn emit(bytes: &[u8]) -> bool {
    match std::io::stdout().write_all(bytes) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => false,
        Err(error) => {
            eprintln!("ls: cannot write output: {error}");
            false
        }
    }
}

fn run<A: AsRef<OsStr>>(args: &[A], program: &str) -> ExitCode {
    let mut options = Options::default();
    let mut paths: Vec<PathBuf> = Vec::new();
    let marker = args.iter().position(|arg| arg.as_ref() == OsStr::new("--"));
    let (options_args, trailing): (&[A], &[A]) = match marker {
        Some(index) => (
            args.get(..index).unwrap_or(&[]),
            args.get(index + 1..).unwrap_or(&[]),
        ),
        None => (args, &[] as &[A]),
    };
    for arg in options_args {
        match arg.as_ref().to_str() {
            Some("--help") => {
                println!("Usage: ls [-aAldh] [FILE...]");
                return ExitCode::from(EXIT_OK);
            }
            Some("-a" | "--all") => options.all = true,
            Some("-A" | "--almost-all") => options.almost_all = true,
            Some("-l") => options.long = true,
            Some("-h" | "--human-readable") => options.human = true,
            Some("-d" | "--directory") => options.directory = true,
            Some("-1") => {}
            Some(value) if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            _ => paths.push(PathBuf::from(arg.as_ref())),
        }
    }
    paths.extend(trailing.iter().map(|arg| PathBuf::from(arg.as_ref())));
    if paths.is_empty() {
        paths.push(PathBuf::from("."));
    }
    let multiple = paths.len() > 1;
    let mut failed = false;
    for (index, path) in paths.iter().enumerate() {
        let display = path.as_os_str();
        if multiple && index > 0 && !emit(b"\n") {
            return ExitCode::from(EXIT_OK);
        }
        match list_path(path, display, options) {
            Ok(text) => {
                if multiple && (!emit(display.as_bytes()) || !emit(b":\n")) {
                    return ExitCode::from(EXIT_OK);
                }
                if !emit(&text) {
                    return ExitCode::from(EXIT_OK);
                }
            }
            Err(error) => {
                eprintln!("{program}: {}: {error}", path.display());
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
        "ls",
    );
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn text(bytes: Vec<u8>) -> String {
        String::from_utf8(bytes).expect("the test fixture is ASCII")
    }

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-ls-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn mode_strings_cover_types_and_special_bits() {
        let root = temp_root("mode");
        let file = root.join("file");
        fs::write(&file, b"x").expect("file");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o4755)).expect("mode");
        let meta = fs::symlink_metadata(&file).expect("meta");
        assert_eq!(mode_string(&meta), "-rwsr-xr-x");
        let dir = root.join("dir");
        fs::create_dir(&dir).expect("dir");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("dir mode");
        let meta = fs::symlink_metadata(&dir).expect("meta");
        assert_eq!(mode_string(&meta), "drwxr-xr-x");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn human_sizes_and_times_render() {
        assert_eq!(human_size(0), "0");
        assert_eq!(human_size(1024), "1.0K");
        assert_eq!(human_size(1024 * 1024), "1.0M");
        assert!(!format_time(0).is_empty());
    }

    #[test]
    fn directory_listing_filters_and_sorts_names() {
        let root = temp_root("listing");
        fs::write(root.join("visible"), b"x").expect("visible");
        fs::write(root.join(".hidden"), b"x").expect("hidden");
        let plain = text(format_dir(&root, Options::default()).expect("plain"));
        assert!(plain.contains("visible"));
        assert!(!plain.contains(".hidden"));
        let all = text(
            format_dir(
                &root,
                Options {
                    all: true,
                    ..Options::default()
                },
            )
            .expect("all"),
        );
        assert!(all.contains(".hidden"));
        assert!(all.contains("..\n"));
    }

    /// `ls -a` without a path uses `"."`, whose `Path::parent()` is the empty
    /// path; the `..` row must still resolve.
    #[test]
    fn the_default_directory_lists_dot_dot() {
        let listing = text(
            format_dir(
                Path::new("."),
                Options {
                    all: true,
                    ..Options::default()
                },
            )
            .expect("the default directory must be listable"),
        );
        assert!(listing.contains(".."), "{listing}");
    }

    #[test]
    fn long_format_contains_metadata_columns() {
        let root = temp_root("long");
        let file = root.join("file");
        fs::write(&file, b"abc").expect("file");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("file mode");
        let listing = text(
            format_dir(
                &root,
                Options {
                    long: true,
                    human: true,
                    ..Options::default()
                },
            )
            .expect("long"),
        );
        assert!(listing.contains("-rw-r--r--"));
        assert!(listing.contains("file"));
    }

    /// A directory entry is bytes; it must appear unchanged in the raw listing.
    #[test]
    fn non_utf8_names_are_emitted_as_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let root = temp_root("nonutf8");
        let name = OsString::from_vec(vec![b'f', 0xff, b'x']);
        fs::write(root.join(&name), b"x").expect("file");
        let listing = format_dir(&root, Options::default()).expect("listing");
        assert!(
            listing.windows(3).any(|window| window == b"f\xffx"),
            "the raw name must survive: {listing:?}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn explicit_symlink_to_directory_lists_the_target() {
        let root = temp_root("symlink-dir");
        let dir = root.join("dir");
        fs::create_dir(&dir).expect("dir");
        fs::write(dir.join("inside"), b"x").expect("inside");
        let link = root.join("link");
        std::os::unix::fs::symlink(&dir, &link).expect("link");
        let listed = text(list_path(&link, OsStr::new("link"), Options::default()).expect("list"));
        assert!(listed.contains("inside"));
        let as_entry = text(
            list_path(
                &link,
                OsStr::new("link"),
                Options {
                    directory: true,
                    ..Options::default()
                },
            )
            .expect("entry"),
        );
        assert!(as_entry.contains("link"));
    }

    #[test]
    fn run_reports_missing_paths_and_bad_options() {
        let root = temp_root("run");
        let missing = root.join("missing").display().to_string();
        assert_eq!(run(&[missing], "ls"), ExitCode::from(1));
        assert_eq!(run(&["--help".to_string()], "ls"), ExitCode::from(EXIT_OK));
        assert_eq!(
            run(&["--bad".to_string()], "ls"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        assert_eq!(
            run(&["-d".to_string(), root.display().to_string()], "ls"),
            ExitCode::from(EXIT_OK)
        );
        let _ = fs::remove_dir_all(root);
    }
}
