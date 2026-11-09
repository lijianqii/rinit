//! `touch` - create files and update timestamps.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rinit_tools::fsutil;
use rinit_tools::{EXIT_OK, program_name, usage_error};

#[derive(Clone, Copy, Default)]
struct Options {
    atime: bool,
    mtime: bool,
    no_create: bool,
    stamp: Option<i64>,
}

impl Options {
    fn wants_atime(self) -> bool {
        self.atime || !self.mtime
    }

    fn wants_mtime(self) -> bool {
        self.mtime || !self.atime
    }
}

fn number(value: &str, start: usize, end: usize) -> Result<i32, String> {
    value
        .get(start..end)
        .ok_or_else(|| format!("invalid timestamp '{value}'"))?
        .parse::<i32>()
        .map_err(|_| format!("invalid timestamp '{value}'"))
}

fn current_year() -> i32 {
    // SAFETY: `time` and `localtime_r` use valid local storage for the call.
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
        return 1970;
    }
    tm.tm_year + 1900
}

fn parse_stamp(value: &str) -> Result<i64, String> {
    let (main, seconds) = match value.split_once('.') {
        Some((main, seconds)) => {
            if seconds.len() != 2 {
                return Err(format!("invalid timestamp '{value}'"));
            }
            (
                main,
                seconds
                    .parse::<i32>()
                    .map_err(|_| format!("invalid timestamp '{value}'"))?,
            )
        }
        None => (value, 0),
    };
    if !main.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("invalid timestamp '{value}'"));
    }
    let (year, month, day, hour, minute) = match main.len() {
        8 => (
            current_year(),
            number(main, 0, 2)?,
            number(main, 2, 4)?,
            number(main, 4, 6)?,
            number(main, 6, 8)?,
        ),
        10 => {
            let short = number(main, 0, 2)?;
            let year = if short < 69 {
                2000 + short
            } else {
                1900 + short
            };
            (
                year,
                number(main, 2, 4)?,
                number(main, 4, 6)?,
                number(main, 6, 8)?,
                number(main, 8, 10)?,
            )
        }
        12 => (
            number(main, 0, 4)?,
            number(main, 4, 6)?,
            number(main, 6, 8)?,
            number(main, 8, 10)?,
            number(main, 10, 12)?,
        ),
        _ => return Err(format!("invalid timestamp '{value}'")),
    };
    if !(0..=59).contains(&seconds) || !(0..=59).contains(&minute) {
        return Err(format!("invalid timestamp '{value}'"));
    }
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    tm.tm_year = year - 1900;
    tm.tm_mon = month - 1;
    tm.tm_mday = day;
    tm.tm_hour = hour;
    tm.tm_min = minute;
    tm.tm_sec = seconds;
    tm.tm_isdst = -1;
    // SAFETY: `tm` is fully initialized and `mktime` only reads it.
    let stamp = unsafe { libc::mktime(&mut tm) };
    if stamp == -1 {
        return Err(format!("invalid timestamp '{value}'"));
    }
    let mut check = unsafe { std::mem::zeroed::<libc::tm>() };
    // SAFETY: `stamp` and `check` are valid local storage for the call.
    if unsafe { libc::localtime_r(&stamp, &mut check) }.is_null()
        || check.tm_year != year - 1900
        || check.tm_mon != month - 1
        || check.tm_mday != day
        || check.tm_hour != hour
        || check.tm_min != minute
        || check.tm_sec != seconds
    {
        return Err(format!("invalid timestamp '{value}'"));
    }
    Ok(stamp as i64)
}

// `time_t` and `c_long` are 32-bit on armv7 and 64-bit on the host; the casts
// are required on the target and are no-ops on the host.
#[allow(clippy::unnecessary_cast)]
fn now() -> io::Result<(i64, i64)> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `time` is valid local storage for the call.
    if unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut time) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((time.tv_sec as i64, time.tv_nsec as i64))
}

fn touch_path(path: &Path, options: Options) -> Result<(), String> {
    let existing = match fs::metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    if existing.is_none() {
        if options.no_create {
            return Ok(());
        }
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    let meta = fs::metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let (now_sec, now_nsec) = now().map_err(|error| format!("{}: {error}", path.display()))?;
    let stamp = options.stamp.unwrap_or(now_sec);
    let stamp_nsec = if options.stamp.is_some() { 0 } else { now_nsec };
    let atime = if options.wants_atime() {
        stamp
    } else {
        meta.atime()
    };
    let atime_nsec = if options.wants_atime() {
        stamp_nsec
    } else {
        meta.atime_nsec()
    };
    let mtime = if options.wants_mtime() {
        stamp
    } else {
        meta.mtime()
    };
    let mtime_nsec = if options.wants_mtime() {
        stamp_nsec
    } else {
        meta.mtime_nsec()
    };
    fsutil::set_times(path, atime, atime_nsec, mtime, mtime_nsec)
        .map_err(|error| format!("{}: {error}", path.display()))
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
    let mut index = 0;
    while let Some(arg) = options_args.get(index) {
        match arg.as_ref().to_str() {
            Some("--help") => {
                println!("Usage: touch [-amc] [-t STAMP] FILE...");
                return ExitCode::from(EXIT_OK);
            }
            Some("-a") => options.atime = true,
            Some("-m") => options.mtime = true,
            Some("-c" | "--no-create") => options.no_create = true,
            Some("-t") => {
                index += 1;
                let Some(value) = options_args.get(index) else {
                    return ExitCode::from(usage_error(program, "-t needs a timestamp"));
                };
                let Some(value) = value.as_ref().to_str() else {
                    return ExitCode::from(usage_error(program, "-t needs a UTF-8 timestamp"));
                };
                match parse_stamp(value) {
                    Ok(stamp) => options.stamp = Some(stamp),
                    Err(error) => return ExitCode::from(usage_error(program, &error)),
                }
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
    for display in paths {
        if let Err(error) = touch_path(&display, options) {
            eprintln!("{program}: {error}");
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
        "touch",
    );
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-touch-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn creates_missing_files_and_honours_no_create() {
        let root = temp_root("create");
        let file = root.join("file");
        touch_path(&file, Options::default()).expect("create");
        assert!(file.exists());
        let missing = root.join("missing");
        let options = Options {
            no_create: true,
            ..Options::default()
        };
        touch_path(&missing, options).expect("skip");
        assert!(!missing.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn explicit_stamp_sets_both_times() {
        let root = temp_root("stamp");
        let file = root.join("file");
        fs::write(&file, b"x").expect("file");
        let stamp = parse_stamp("202611021200.30").expect("stamp");
        let options = Options {
            stamp: Some(stamp),
            ..Options::default()
        };
        touch_path(&file, options).expect("touch");
        let meta = fs::metadata(&file).expect("meta");
        assert_eq!(meta.atime(), stamp);
        assert_eq!(meta.mtime(), stamp);
        assert_eq!(meta.mtime_nsec(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn atime_only_preserves_mtime() {
        let root = temp_root("atime");
        let file = root.join("file");
        fs::write(&file, b"x").expect("file");
        fsutil::set_times(&file, 1_000, 0, 2_000, 0).expect("seed");
        let options = Options {
            atime: true,
            stamp: Some(3_000),
            ..Options::default()
        };
        touch_path(&file, options).expect("touch");
        let meta = fs::metadata(&file).expect("meta");
        assert_eq!(meta.atime(), 3_000);
        assert_eq!(meta.mtime(), 2_000);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn timestamp_parser_accepts_documented_shapes() {
        for value in ["01020304", "2601020304", "202601020304", "202601020304.05"] {
            assert!(parse_stamp(value).is_ok(), "{value}");
        }
        for value in ["", "x", "123", "202613020304", "202601020304.7"] {
            assert!(parse_stamp(value).is_err(), "{value}");
        }
    }

    #[test]
    fn run_reports_usage_and_bad_options() {
        let root = temp_root("run");
        let missing = root.join("missing");
        assert_eq!(
            run(&["-c".to_string(), missing.display().to_string()], "touch"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["--help".to_string()], "touch"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["--bad".to_string()], "touch"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        assert_eq!(
            run(&[] as &[String], "touch"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        let _ = fs::remove_dir_all(root);
    }
}
