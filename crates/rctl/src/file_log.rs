//! Local reader for the persisted service log.
//!
//! `rctl log --file` deliberately stays out of the control protocol: the file
//! belongs to the target, may be on a slow or absent filesystem, and a PID 1
//! that blocks on reading it would be worse than a client-side error. Rotated
//! files are read oldest-first, then the current file.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rinit_core::MAX_LOG_KEEP;

use crate::parse::FileLog;

/// Run the file-backed `log` command.
pub fn run(log: &FileLog) -> ExitCode {
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    match write_log(log, &mut out) {
        Ok(()) => ExitCode::SUCCESS,
        // A closed pipe is a normal downstream condition, exactly as it is for
        // the control-socket reply path.
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rctl: {error}");
            ExitCode::from(1)
        }
    }
}

/// The paths in oldest-first order: `path.N`, ..., `path.1`, then `path`.
fn log_paths(path: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(MAX_LOG_KEEP + 1);
    for index in (1..=MAX_LOG_KEEP).rev() {
        let mut rotated = path.as_os_str().to_os_string();
        rotated.push(format!(".{index}"));
        paths.push(PathBuf::from(rotated));
    }
    paths.push(path.to_path_buf());
    paths
}

/// The paths in newest-first order: `path`, then `path.1` ... `path.N`.
///
/// `log_paths` remains the streaming order for a full read; a tail only needs
/// the newer files until it has enough lines, so it walks this direction and
/// stops early.
fn log_paths_newest_first(path: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(MAX_LOG_KEEP + 1);
    paths.push(path.to_path_buf());
    for index in 1..=MAX_LOG_KEEP {
        let mut rotated = path.as_os_str().to_os_string();
        rotated.push(format!(".{index}"));
        paths.push(PathBuf::from(rotated));
    }
    paths
}

/// Open one log path, skipping a missing rotated file.
///
/// A directory at `path.1` or `.2` is skipped (a failed rotation can leave
/// one); the current path is an error because the user asked for a log, not a
/// directory. A file that disappears between metadata and open is also normal
/// during rotation.
fn open_log(path: &Path, current: &Path) -> io::Result<Option<BufReader<std::fs::File>>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("cannot read {}: {error}", path.display()),
            ));
        }
    };
    if !metadata.is_file() {
        if path == current {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("cannot read {}: not a regular file", path.display()),
            ));
        }
        return Ok(None);
    }
    match std::fs::File::open(path) {
        Ok(file) => Ok(Some(BufReader::new(file))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("cannot read {}: {error}", path.display()),
        )),
    }
}

fn no_log_file(log: &FileLog) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("cannot read {}: no such file", log.path.display()),
    )
}

/// Write the selected lines, preserving the file's line endings.
fn write_log<W: Write>(log: &FileLog, out: &mut W) -> io::Result<()> {
    let prefix = log.unit.as_ref().map(|unit| format!("{unit}: "));
    if log.tail == 0 {
        write_all(log, out, prefix.as_deref())
    } else {
        write_tail(log, out, prefix.as_deref())
    }
}

/// Stream every matching line, oldest file first.
fn write_all<W: Write>(log: &FileLog, out: &mut W, prefix: Option<&str>) -> io::Result<()> {
    let mut found = false;
    let mut line = Vec::new();
    for path in log_paths(&log.path) {
        let Some(mut reader) = open_log(&path, &log.path)? else {
            continue;
        };
        found = true;
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            let content = line.strip_suffix(b"\n").unwrap_or(&line);
            let content = content.strip_suffix(b"\r").unwrap_or(content);
            if let Some(prefix) = prefix
                && !content.starts_with(prefix.as_bytes())
            {
                continue;
            }
            out.write_all(&line)?;
        }
    }
    if !found {
        return Err(no_log_file(log));
    }
    out.flush()
}

/// Keep the last `tail` matching lines while reading newest files first.
///
/// A rotated log can be tens of megabytes; if the current file already holds
/// enough matching lines, none of the older files need to be read at all.
fn write_tail<W: Write>(log: &FileLog, out: &mut W, prefix: Option<&str>) -> io::Result<()> {
    let tail = log.tail as usize;
    let mut selected: VecDeque<Vec<u8>> = VecDeque::new();
    let mut found = false;
    let mut line = Vec::new();
    for path in log_paths_newest_first(&log.path) {
        let Some(mut reader) = open_log(&path, &log.path)? else {
            continue;
        };
        found = true;
        let mut file_lines: VecDeque<Vec<u8>> = VecDeque::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            let content = line.strip_suffix(b"\n").unwrap_or(&line);
            let content = content.strip_suffix(b"\r").unwrap_or(content);
            if let Some(prefix) = prefix
                && !content.starts_with(prefix.as_bytes())
            {
                continue;
            }
            if file_lines.len() == tail {
                file_lines.pop_front();
            }
            file_lines.push_back(std::mem::take(&mut line));
        }
        // Keep only the part of this older file that still belongs to the
        // newest `tail` lines across all files.
        let keep = tail.saturating_sub(selected.len());
        while file_lines.len() > keep {
            file_lines.pop_front();
        }
        while let Some(line) = file_lines.pop_back() {
            selected.push_front(line);
        }
        if selected.len() >= tail {
            break;
        }
    }
    if !found {
        return Err(no_log_file(log));
    }
    for line in selected {
        out.write_all(&line)?;
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rinit_core::UnitName;

    fn unit(name: &str) -> UnitName {
        UnitName::parse(name).expect("valid unit")
    }

    fn write(path: &Path, text: &str) {
        std::fs::write(path, text).expect("write scratch log");
    }

    fn write_bytes(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).expect("write scratch log");
    }

    fn rotated(path: &Path, index: usize) -> PathBuf {
        let mut name = path.as_os_str().to_os_string();
        name.push(format!(".{index}"));
        PathBuf::from(name)
    }

    #[test]
    fn unit_filter_keeps_the_prefix_and_all_matching_lines() {
        let dir = std::env::temp_dir().join(format!("rctl-file-log-filter-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("log");
        write(
            &path,
            "app.service: one\nother.service: noise\napp.service: two\n",
        );

        let log = FileLog {
            path: path.clone(),
            unit: Some(unit("app.service")),
            tail: 0,
        };
        let mut out = Vec::new();
        write_log(&log, &mut out).expect("read log");
        assert_eq!(
            String::from_utf8(out).expect("UTF-8"),
            "app.service: one\napp.service: two\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_utf8_lines_are_preserved_byte_for_byte() {
        let dir = std::env::temp_dir().join(format!("rctl-file-log-bytes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("log");
        let bytes = b"app.service: one\napp.service: \xff\xfe\napp.service: two";
        write_bytes(&path, bytes);

        let log = FileLog {
            path: path.clone(),
            unit: Some(unit("app.service")),
            tail: 0,
        };
        let mut out = Vec::new();
        write_log(&log, &mut out).expect("read log");
        assert_eq!(out, bytes);

        let tail = FileLog {
            path,
            unit: Some(unit("app.service")),
            tail: 1,
        };
        let mut out = Vec::new();
        write_log(&tail, &mut out).expect("read tail");
        assert_eq!(out, b"app.service: two");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotated_files_are_read_oldest_first_and_tail_keeps_the_newest() {
        let dir = std::env::temp_dir().join(format!("rctl-file-log-rotate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("log");
        write(&rotated(&path, 2), "app.service: oldest\n");
        write(&rotated(&path, 1), "app.service: middle\n");
        write(&path, "app.service: newest\n");

        let all = FileLog {
            path: path.clone(),
            unit: Some(unit("app.service")),
            tail: 0,
        };
        let mut out = Vec::new();
        write_log(&all, &mut out).expect("read all");
        assert_eq!(
            String::from_utf8(out).expect("UTF-8"),
            "app.service: oldest\napp.service: middle\napp.service: newest\n"
        );

        let tail = FileLog {
            path,
            unit: Some(unit("app.service")),
            tail: 2,
        };
        let mut out = Vec::new();
        write_log(&tail, &mut out).expect("read tail");
        assert_eq!(
            String::from_utf8(out).expect("UTF-8"),
            "app.service: middle\napp.service: newest\n"
        );

        let huge = FileLog {
            path: dir.join("log"),
            unit: Some(unit("app.service")),
            tail: u32::MAX,
        };
        let mut out = Vec::new();
        write_log(&huge, &mut out).expect("a huge tail must not preallocate it");
        assert_eq!(
            String::from_utf8(out).expect("UTF-8"),
            "app.service: oldest\napp.service: middle\napp.service: newest\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_non_file_rotation_is_skipped_but_the_current_file_is_read() {
        let dir =
            std::env::temp_dir().join(format!("rctl-file-log-bad-rotation-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("log");
        std::fs::create_dir(rotated(&path, 2)).expect("bad rotation directory");
        write(&path, "app.service: current\n");

        let log = FileLog {
            path,
            unit: Some(unit("app.service")),
            tail: 0,
        };
        let mut out = Vec::new();
        write_log(&log, &mut out).expect("bad rotation must not abort");
        assert_eq!(
            String::from_utf8(out).expect("UTF-8"),
            "app.service: current\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_an_error_and_an_empty_match_is_not() {
        let dir =
            std::env::temp_dir().join(format!("rctl-file-log-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("log");
        let missing = FileLog {
            path: path.clone(),
            unit: None,
            tail: 0,
        };
        let mut out = Vec::new();
        let error = write_log(&missing, &mut out).expect_err("missing file");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);

        write(&path, "other.service: noise\n");
        let no_match = FileLog {
            path,
            unit: Some(unit("app.service")),
            tail: 0,
        };
        let mut out = Vec::new();
        write_log(&no_match, &mut out).expect("no match is success");
        assert!(out.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
