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

use std::collections::VecDeque;
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

/// Hard bound for a single line that has not seen a newline yet. Without this,
/// a service writing an endless line grows `pending` until PID 1 runs out of
/// memory.
const MAX_PENDING_LINE: usize = 64 * 1024;

/// How large the persisted log file may grow before it is truncated
/// (plan §1.3(7)).
///
/// The plan asks for persistence without asking for a format or a size; a
/// megabyte is the same order as the ring's own bound, and the policy is the
/// same one the ring uses - past the bound, the *oldest* lines are the ones that
/// go.
const LOG_FILE_MAX_BYTES: u64 = 1024 * 1024;

/// How many lines may wait for the persisted log to catch up.
///
/// This is the bound the file's own ring used to lack: a service that logs in a
/// tight loop must cost a fixed amount of memory even when the disk is slower
/// than the service, and the oldest line is the one worth losing.
const LOG_QUEUE_MAX_LINES: usize = 4096;
/// Byte bound for the same queue. A line is at most `MAX_PENDING_LINE` plus
/// its unit prefix, so a handful of the largest lines still fit.
const LOG_QUEUE_MAX_BYTES: usize = 256 * 1024;
/// How much one supervisor pass writes. Bounded so a backlog cannot hold the
/// loop for an unbounded time; the rest waits for the next pass.
const LOG_DRAIN_MAX_LINES: usize = 256;
const LOG_DRAIN_MAX_BYTES: usize = 32 * 1024;

/// The shared log ring and the tasks filling it.
pub struct ServiceLogs {
    /// The in-memory ring. Captured lines reach it without any file I/O.
    buffer: Arc<Mutex<LogBuffer>>,
    /// The persisted sink, when one was configured. Arc so a pump task created
    /// before `persist_to` still sees the file once it is installed.
    file: Arc<Mutex<Option<Arc<FileQueue>>>>,
}

/// Lines waiting for the writer, and the writer itself.
///
/// The queue and the file have separate locks so enqueueing never waits for a
/// disk write. The supervisor drains the queue in bounded batches; rinit stays
/// single-threaded, which is what keeps `fork()` safe (plan §2.1⑤).
struct FileQueue {
    state: Mutex<QueueState>,
    writer: Mutex<LogFile>,
}

#[derive(Default)]
struct QueueState {
    lines: VecDeque<String>,
    bytes: usize,
    /// Lines dropped because the queue was full, reported on the next drain.
    dropped: u64,
}

impl FileQueue {
    fn new(file: LogFile) -> Self {
        FileQueue {
            state: Mutex::new(QueueState::default()),
            writer: Mutex::new(file),
        }
    }

    fn enqueue(&self, text: String) {
        let mut state = lock(&self.state);
        state.bytes = state.bytes.saturating_add(text.len());
        state.lines.push_back(text);
        while state.lines.len() > LOG_QUEUE_MAX_LINES || state.bytes > LOG_QUEUE_MAX_BYTES {
            let Some(oldest) = state.lines.pop_front() else {
                break;
            };
            state.bytes = state.bytes.saturating_sub(oldest.len());
            state.dropped += 1;
        }
    }

    /// Write at most one bounded batch, returning how many lines were written.
    fn drain(&self) -> usize {
        let (batch, dropped) = {
            let mut state = lock(&self.state);
            let mut batch = VecDeque::new();
            let mut bytes = 0usize;
            while batch.len() < LOG_DRAIN_MAX_LINES {
                let Some(next) = state.lines.front() else {
                    break;
                };
                if !batch.is_empty() && bytes + next.len() > LOG_DRAIN_MAX_BYTES {
                    break;
                }
                let Some(line) = state.lines.pop_front() else {
                    break;
                };
                state.bytes = state.bytes.saturating_sub(line.len());
                bytes += line.len();
                batch.push_back(line);
            }
            let dropped = state.dropped;
            state.dropped = 0;
            (batch, dropped)
        };

        let mut text = String::new();
        if dropped > 0 {
            text.push_str(&format!(
                "rinit: warning: dropped {dropped} queued log line(s)\n"
            ));
        }
        for line in &batch {
            text.push_str(line);
        }
        if !text.is_empty() {
            // One write per batch: a serialized log is worth the extra buffer,
            // and it keeps a slow disk from costing one syscall per line.
            lock(&self.writer).append_text(&text);
        }
        batch.len()
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

    /// Append one already-formatted record, truncating first if that would pass
    /// the bound.
    fn append_text(&mut self, text: &str) {
        if self.written >= self.max_bytes {
            self.truncate();
        }
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
            buffer: Arc::new(Mutex::new(LogBuffer::with_defaults())),
            file: Arc::new(Mutex::new(None)),
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
                *lock(&self.file) = Some(Arc::new(FileQueue::new(file)));
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

        let buffer = Arc::clone(&self.buffer);
        let file = Arc::clone(&self.file);
        let unit = unit.clone();
        // Spawning here is safe: `capture` is only called from the async boot
        // path, which runs inside the runtime.
        tokio::spawn(async move { pump(reader, unit, buffer, file).await });
        Ok(writer)
    }

    /// Write one bounded batch of queued lines to the persisted log.
    ///
    /// Called from the supervisor loop, not from a log pump: a service that
    /// writes faster than the disk then costs queue space, never an unbounded
    /// wait inside the reader task.
    pub fn drain_file(&self) {
        let Some(queue) = lock(&self.file).as_ref().map(Arc::clone) else {
            return;
        };
        queue.drain();
    }

    /// Drain the persisted log completely, for shutdown.
    pub fn flush_file(&self) {
        let Some(queue) = lock(&self.file).as_ref().map(Arc::clone) else {
            return;
        };
        // Bounded: a service still flooding at shutdown must not hold the
        // machine here. The queue itself is bounded, so this always ends.
        for _ in 0..LOG_QUEUE_MAX_LINES {
            if queue.drain() == 0 {
                break;
            }
        }
    }

    /// The most recent lines, from one unit or from all of them.
    ///
    /// `count == 0` means "everything buffered".
    pub fn tail(&self, unit: Option<&UnitName>, count: usize) -> Vec<LogLine> {
        let buffer = lock(&self.buffer);
        let count = if count == 0 { usize::MAX } else { count };
        match unit {
            Some(unit) => buffer.tail_for(unit, count),
            None => buffer.tail(count),
        }
    }

    /// Test seam: the same path a captured line takes.
    #[cfg(test)]
    fn push(&self, unit: &UnitName, line: &str) {
        push_captured(&self.buffer, &self.file, unit, line);
    }
}

impl Default for ServiceLogs {
    fn default() -> Self {
        ServiceLogs::new()
    }
}

/// Lock a log mutex, surviving poisoning.
///
/// A panic *while holding one* would mean a bug in the buffer or the file
/// writer, and the data behind it is plain text: recovering is strictly better
/// than turning logs into a crash.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Record one captured line in the ring and enqueue it for the file.
///
/// The buffer lock is held while enqueueing so the two destinations see the same
/// order across concurrent pumps. Enqueueing is the only work done here: the
/// disk write belongs to the supervisor.
fn push_captured(
    buffer: &Arc<Mutex<LogBuffer>>,
    file: &Arc<Mutex<Option<Arc<FileQueue>>>>,
    unit: &UnitName,
    line: &str,
) {
    let mut buffer = lock(buffer);
    buffer.push(unit, line);
    if let Some(queue) = lock(file).as_ref() {
        queue.enqueue(format!("{unit}: {line}\n"));
    }
}

/// Read `reader` until end-of-file, appending whole lines to `buffer`.
async fn pump(
    reader: OwnedFd,
    unit: UnitName,
    buffer: Arc<Mutex<LogBuffer>>,
    file: Arc<Mutex<Option<Arc<FileQueue>>>>,
) {
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
                cap_pending(&mut pending);
                for line in lines {
                    push_captured(&buffer, &file, &unit, &line);
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
        push_captured(&buffer, &file, &unit, &line);
    }
}

/// Keep an unterminated line bounded and tell the reader it was truncated.
fn cap_pending(pending: &mut String) {
    if pending.len() <= MAX_PENDING_LINE {
        return;
    }
    let mut cut = MAX_PENDING_LINE;
    while cut > 0 && !pending.is_char_boundary(cut) {
        cut -= 1;
    }
    pending.truncate(cut);
    pending.push_str("[truncated]\n");
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
        logs.push(&app, "first");
        logs.push(&app, "second");
        logs.drain_file();

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
            file.append_text(&format!("{app}: 0123456789\n"));
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

    /// The persisted queue is bounded: a service logging faster than the disk
    /// costs memory in the queue, not in PID 1's address space. The oldest line
    /// goes and the file says how many.
    #[test]
    fn a_full_file_queue_drops_the_oldest_and_reports_it() {
        let path = std::env::temp_dir().join(format!("rinit-logs-queue-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let queue = FileQueue::new(LogFile::open(&path, 1 << 20).expect("open"));
        let total = LOG_QUEUE_MAX_LINES + 10;
        for line in 0..total {
            queue.enqueue(format!("app.service: line {line}\n"));
        }
        while queue.drain() > 0 {}

        let written = std::fs::read_to_string(&path).expect("read back");
        assert!(
            written.contains("dropped 10 queued log line(s)"),
            "the loss must be visible: {written}"
        );
        assert!(
            !written.contains("app.service: line 0\n"),
            "the oldest lines are the ones dropped"
        );
        assert!(
            written.contains(&format!("app.service: line {}\n", total - 1)),
            "the newest line must survive"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unterminated_line_is_capped() {
        let mut pending = "x".repeat(MAX_PENDING_LINE + 1);
        cap_pending(&mut pending);
        assert!(pending.len() <= MAX_PENDING_LINE + "[truncated]\n".len());
        assert!(pending.ends_with("[truncated]\n"));
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
