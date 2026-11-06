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

/// Write the selected lines, preserving the file's line endings.
fn write_log<W: Write>(log: &FileLog, out: &mut W) -> io::Result<()> {
    let prefix = log.unit.as_ref().map(|unit| format!("{unit}: "));
    let mut selected: Option<VecDeque<String>> = if log.tail == 0 {
        None
    } else {
        // Do not trust the requested count as an allocation size: a CLI can
        // ask for `u32::MAX` while the file is tiny.
        Some(VecDeque::new())
    };
    let mut found = false;
    let mut line = String::new();

    for path in log_paths(&log.path) {
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("cannot read {}: {error}", path.display()),
                ));
            }
        };
        if !metadata.is_file() {
            // A failed rotation can leave a directory at `.1` or `.2`; that
            // must not hide a perfectly good current file. The current path is
            // still an error because the user asked for a log, not a directory.
            if path == log.path {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("cannot read {}: not a regular file", path.display()),
                ));
            }
            continue;
        }
        let file = std::fs::File::open(&path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot read {}: {error}", path.display()),
            )
        })?;
        found = true;
        let mut reader = BufReader::new(file);
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let content = line.strip_suffix('\n').unwrap_or(&line);
            let content = content.strip_suffix('\r').unwrap_or(content);
            if let Some(prefix) = &prefix
                && !content.starts_with(prefix)
            {
                continue;
            }
            if let Some(queue) = &mut selected {
                queue.push_back(line.clone());
                if queue.len() > log.tail as usize {
                    queue.pop_front();
                }
            } else {
                out.write_all(line.as_bytes())?;
            }
        }
    }

    if !found {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("cannot read {}: no such file", log.path.display()),
        ));
    }

    if let Some(queue) = selected {
        for line in queue {
            out.write_all(line.as_bytes())?;
        }
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
