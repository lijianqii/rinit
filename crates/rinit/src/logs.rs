//! Capturing service output into the shared log ring (plan §1.3(7)).
//!
//! Each service's `stdout`/`stderr` are redirected into a pipe at `exec` time;
//! a task per service reads that pipe and appends whole lines to one shared,
//! bounded buffer. The buffer itself lives in `rinit-core` (so its memory bound
//! is unit-tested); this module is only the plumbing.
//!
//! The read end is non-blocking (so the reactor can wait on it) while the write
//! end the child inherits stays *blocking*: a service that fills the pipe must
//! block, not lose output or get an error it does not expect.

use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rinit_core::{LogBuffer, LogLine, UnitName};
use rinit_runtime::RuntimeError;
use rinit_runtime::syscall::fs;
use tokio::io::unix::AsyncFd;

use crate::log::rlog;

/// How much of a read is buffered before splitting into lines.
const READ_CHUNK: usize = 4096;

/// How large the persisted log file may grow before it is truncated
/// (plan §1.3(7)).
///
/// The plan asks for persistence without asking for a format or a size; a
/// megabyte is the same order as the ring's own bound, and the policy is the
/// same one the ring uses - past the bound, the *oldest* lines are the ones that
/// go.
const LOG_FILE_MAX_BYTES: u64 = 1024 * 1024;

/// The shared log ring and the tasks filling it.
pub struct ServiceLogs {
    /// The ring, and the file it is mirrored to if there is one. One lock, so a
    /// line reaches both in the same order it was produced.
    sink: Arc<Mutex<Sink>>,
}

/// Where captured lines go: the ring always, a file when one was configured.
struct Sink {
    buffer: LogBuffer,
    file: Option<LogFile>,
}

impl Sink {
    /// The one place a line enters the log, in either direction.
    fn push(&mut self, unit: &UnitName, line: &str) {
        self.buffer.push(unit, line);
        if let Some(file) = self.file.as_mut() {
            file.append(unit, line);
        }
    }
}

/// An append-only mirror of the ring (plan §1.3(7)).
///
/// Written with a plain `write_all` and no user-space buffer, deliberately: the
/// point of the file is a log someone can read after the fact, and a line still
/// sitting in a `BufWriter` when the machine goes down is a line nobody has. The
/// ring is the fast path; this is the durable one.
struct LogFile {
    file: std::fs::File,
    path: PathBuf,
    /// Bytes written so far, so the bound costs no `stat` per line.
    written: u64,
    /// The bound; `LOG_FILE_MAX_BYTES` in production, smaller in tests.
    max_bytes: u64,
    /// Set once, so a filesystem that fails on every write warns once.
    warned: bool,
}

impl LogFile {
    /// Open (or create) `path`, appending to whatever is there.
    ///
    /// # Errors
    /// Whatever the directory creation or the open returns - a read-only rootfs,
    /// a path with no parent rinit may create. The caller degrades: no file, and
    /// the ring still works.
    fn open(path: &Path, max_bytes: u64) -> std::io::Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            // `/var/log/rinit` is rinit's to create: the plan's path names a
            // directory, and an init that refused to make one would only push
            // the problem onto whoever runs it.
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        Ok(LogFile {
            file,
            path: path.to_path_buf(),
            written,
            max_bytes: max_bytes.max(1),
            warned: false,
        })
    }

    /// Append one line, truncating first if that would pass the bound.
    fn append(&mut self, unit: &UnitName, line: &str) {
        if self.written >= self.max_bytes {
            self.truncate();
        }
        let text = format!("{unit}: {line}\n");
        match self.file.write_all(text.as_bytes()) {
            Ok(()) => {
                self.written += text.len() as u64;
                self.warned = false;
            }
            // A log is diagnostic: a full disk must not take the system with it,
            // and it must not fill the console either - hence once.
            Err(e) => {
                if !self.warned {
                    rlog!(
                        "warning: cannot write the log file {}: {e}",
                        self.path.display()
                    );
                    self.warned = true;
                }
            }
        }
    }

    /// Drop what is there and start again.
    ///
    /// Truncate rather than rotate to a second file: the ring drops its oldest
    /// lines past its bound, and a file that behaves the same way is one fewer
    /// thing to watch on a rootfs. `O_APPEND` is what makes this safe without
    /// seeking - every write goes to the end of the file as it is *now*.
    fn truncate(&mut self) {
        if self.file.set_len(0).is_ok() {
            self.written = 0;
            let _ = self
                .file
                .write_all(b"log truncated: the previous contents were dropped\n");
        }
    }
}

impl ServiceLogs {
    /// A logs store with the default buffer bounds and no file.
    pub fn new() -> Self {
        ServiceLogs {
            sink: Arc::new(Mutex::new(Sink {
                buffer: LogBuffer::with_defaults(),
                file: None,
            })),
        }
    }

    /// Mirror every captured line into `path` (plan §1.3(7)).
    ///
    /// Best effort by design: a board whose rootfs is read-only, or whose
    /// `/var/log` cannot be created, boots with the ring and a warning - the same
    /// degradation every other optional facility here has.
    pub fn persist_to(&self, path: &Path) {
        match LogFile::open(path, LOG_FILE_MAX_BYTES) {
            Ok(file) => {
                lock(&self.sink).file = Some(file);
                rlog!("logging to {}", path.display());
            }
            Err(e) => {
                rlog!(
                    "warning: cannot persist logs to {}: {e}; keeping them in memory only",
                    path.display()
                );
            }
        }
    }

    /// Start capturing `unit`'s output, returning the descriptor to inherit.
    ///
    /// The caller **must drop the returned descriptor right after spawning**:
    /// it is rinit's copy of the pipe's write end, and as long as it is open the
    /// reader never sees EOF and the task never ends.
    ///
    /// # Errors
    /// [`RuntimeError`] if the pipe cannot be created or made non-blocking.
    pub fn capture(&self, unit: &UnitName) -> Result<OwnedFd, RuntimeError> {
        let (reader, writer) = fs::pipe()?;
        fs::set_nonblocking(reader.as_raw_fd(), true)?;

        let sink = Arc::clone(&self.sink);
        let unit = unit.clone();
        // Spawning here is safe: `capture` is only called from the async boot
        // path, which runs inside the runtime.
        tokio::spawn(async move { pump(reader, unit, sink).await });
        Ok(writer)
    }

    /// The most recent lines, from one unit or from all of them.
    ///
    /// `count == 0` means "everything buffered".
    pub fn tail(&self, unit: Option<&UnitName>, count: usize) -> Vec<LogLine> {
        let sink = lock(&self.sink);
        let count = if count == 0 { usize::MAX } else { count };
        match unit {
            Some(unit) => sink.buffer.tail_for(unit, count),
            None => sink.buffer.tail(count),
        }
    }
}

impl Default for ServiceLogs {
    fn default() -> Self {
        ServiceLogs::new()
    }
}

/// Lock the buffer, surviving a poisoned mutex.
///
/// A panic *while holding this lock* would mean a bug in `LogBuffer`, and the
/// data behind it is plain text: recovering is strictly better than turning
/// logs into a crash.
fn lock(sink: &Arc<Mutex<Sink>>) -> std::sync::MutexGuard<'_, Sink> {
    sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Read `reader` until end-of-file, appending whole lines to `buffer`.
async fn pump(reader: OwnedFd, unit: UnitName, sink: Arc<Mutex<Sink>>) {
    // SAFETY: `reader` is an `OwnedFd`, so this call owns a valid descriptor and
    // never closes or replaces it while the `AsyncFd` lives; that is exactly the
    // obligation `register` documents. The safe alternative, `AsyncFd::new`, is
    // deprecated as *unsound* (it cannot know the caller keeps the descriptor
    // valid), so `register` plus this invariant is the correct form. This is one
    // of the binary's three `unsafe` call sites outside the syscall layer and
    // `signals`; the others are `activation.rs` (the same `register`) and the
    // `kill(pid, 0)` probe in `unit_manager.rs`.
    let afd = match unsafe { AsyncFd::register(reader) } {
        Ok(afd) => afd,
        Err(e) => {
            rlog!("warning: cannot watch {unit}'s log pipe: {e}");
            return;
        }
    };

    let mut pending = String::new();
    let mut buf = [0u8; READ_CHUNK];
    loop {
        let mut guard = match afd.readable().await {
            Ok(guard) => guard,
            Err(e) => {
                rlog!("warning: {unit} log pipe failed: {e}");
                break;
            }
        };

        let read = guard.try_io(|inner| {
            fs::read(inner.get_ref().as_raw_fd(), &mut buf).map_err(|e| e.io_error())
        });
        match read {
            // End of file: every writer is gone, so the service is finished.
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                // Lossy: a service writing invalid UTF-8 must not lose its log.
                pending.push_str(&String::from_utf8_lossy(buf.get(..n).unwrap_or(&[])));
                let mut lines: Vec<String> = Vec::new();
                while let Some(newline) = pending.find('\n') {
                    let line: String = pending.drain(..=newline).collect();
                    lines.push(trim_line(&line));
                }
                if !lines.is_empty() {
                    let mut sink = lock(&sink);
                    for line in lines {
                        sink.push(&unit, &line);
                    }
                }
            }
            // A real read error (not "would block", which `try_io` turns into
            // the outer `Err`).
            Ok(Err(e)) => {
                rlog!("warning: reading {unit}'s log: {e}");
                break;
            }
            // Readiness was cleared under us: await it again.
            Err(_) => continue,
        }
    }

    // A final partial line with no trailing newline still belongs in the log.
    if !pending.is_empty() {
        let line = trim_line(&pending);
        lock(&sink).push(&unit, &line);
    }
}

/// Strip the line terminator from a captured line.
fn trim_line(line: &str) -> String {
    line.trim_end_matches(['\n', '\r']).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn unit(name: &str) -> UnitName {
        UnitName::parse(name).expect("valid name")
    }

    /// The whole path: a writer's bytes reach the buffer as whole lines.
    ///
    /// This is a real pipe and a real reader task, so it covers the redirection
    /// plumbing rather than the buffer's own rules (which `rinit-core` tests).
    #[tokio::test(flavor = "current_thread")]
    async fn captured_output_reaches_the_buffer() {
        let logs = ServiceLogs::new();
        let app = unit("app.service");

        let writer_fd = logs.capture(&app).expect("capture");
        let mut writer = std::fs::File::from(writer_fd);
        writer.write_all(b"first\nsecond\n").expect("write");
        // Also exercise a split write: half a line, then the rest.
        writer.write_all(b"thi").expect("write");
        writer.flush().expect("flush");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        writer.write_all(b"rd\n").expect("write");

        // Dropping the write end is what makes the reader finish.
        drop(writer);
        wait_for(&logs, &app, 3).await;

        let lines: Vec<String> = logs
            .tail(Some(&app), 0)
            .iter()
            .map(|line| line.text.clone())
            .collect();
        assert_eq!(lines, ["first", "second", "third"]);
    }

    /// Plan §1.3(7): with a file configured, a captured line reaches the ring
    /// *and* the file - including the directory the plan's path implies, which
    /// rinit has to create itself.
    #[test]
    fn a_configured_file_gets_the_same_lines_as_the_ring() {
        let dir = std::env::temp_dir().join(format!("rinit-logs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested/rinit.log");

        let logs = ServiceLogs::new();
        logs.persist_to(&path);
        let app = unit("app.service");
        lock(&logs.sink).push(&app, "first");
        lock(&logs.sink).push(&app, "second");

        assert_eq!(logs.tail(Some(&app), 0).len(), 2, "the ring still has them");
        let written = std::fs::read_to_string(&path).expect("the log file");
        assert_eq!(written, "app.service: first\napp.service: second\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Past its bound the file drops what it had - the ring's own policy, for the
    /// same reason: a rootfs with room for a megabyte does not have room for
    /// whatever a churning service prints overnight.
    #[test]
    fn the_log_file_truncates_at_its_bound() {
        let path = std::env::temp_dir().join(format!("rinit-logs-cap-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut file = LogFile::open(&path, 64).expect("open");
        let app = unit("app.service");
        for _ in 0..20 {
            file.append(&app, "0123456789");
        }

        let written = std::fs::read_to_string(&path).expect("read back");
        assert!(
            written.len() <= 128,
            "the bound has to hold, got {} bytes",
            written.len()
        );
        assert!(written.contains("log truncated"), "and say so: {written}");
        assert!(
            written.ends_with("app.service: 0123456789\n"),
            "keeping the newest lines, not the oldest: {written}"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A line left without a trailing newline is still logged.
    #[tokio::test(flavor = "current_thread")]
    async fn a_trailing_partial_line_is_kept() {
        let logs = ServiceLogs::new();
        let app = unit("app.service");
        let mut writer = std::fs::File::from(logs.capture(&app).expect("capture"));
        writer.write_all(b"no newline").expect("write");
        drop(writer);
        wait_for(&logs, &app, 1).await;

        assert_eq!(logs.tail(Some(&app), 0)[0].text, "no newline");
    }

    /// Wait until `count` lines have been captured, or fail.
    async fn wait_for(logs: &ServiceLogs, unit: &UnitName, count: usize) {
        for _ in 0..100 {
            if logs.tail(Some(unit), 0).len() >= count {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("expected {count} lines, got {:?}", logs.tail(Some(unit), 0));
    }
}
