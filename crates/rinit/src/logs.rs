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
use std::io::{self, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// Default bound for the persisted log file (plan §1.3(7)).
///
/// The plan asks for persistence without asking for a format or a size; a
/// megabyte is the same order as the ring's own bound. `--log-max-bytes`
/// overrides it, and `--log-keep` chooses rotation instead of truncation.
pub(crate) const DEFAULT_LOG_FILE_MAX_BYTES: u64 = 1024 * 1024;

/// The smallest log-file bound accepted from the command line.
pub(crate) const MIN_LOG_FILE_MAX_BYTES: u64 = 4096;

/// Default number of rotated logs: zero keeps the historical truncate policy.
pub(crate) const DEFAULT_LOG_KEEP: usize = 0;

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
    /// Largest batch worth assembling for the configured file bound.
    max_batch: usize,
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
        let max_batch = file.max_bytes.min(LOG_DRAIN_MAX_BYTES as u64).max(1) as usize;
        FileQueue {
            state: Mutex::new(QueueState::default()),
            writer: Mutex::new(file),
            max_batch,
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
                if !batch.is_empty() && bytes + next.len() > self.max_batch {
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
    /// `None` after a failed reopen, so the next append retries instead of
    /// writing through a descriptor that no longer names `path`.
    file: Option<std::fs::File>,
    path: PathBuf,
    /// Bytes written to the current file, so the bound costs no `stat` per line.
    written: u64,
    /// The bound; `DEFAULT_LOG_FILE_MAX_BYTES` in production, smaller in tests.
    max_bytes: u64,
    /// Rotated files to keep; zero means truncate, the historical policy.
    keep: usize,
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
    fn open(path: &Path, max_bytes: u64, keep: usize) -> std::io::Result<Self> {
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
            file: Some(file),
            path: path.to_path_buf(),
            written,
            max_bytes: max_bytes.max(1).min(usize::MAX as u64),
            keep,
            warned: false,
        })
    }

    /// Append one already-formatted record, rotating or truncating first when
    /// the write would pass the bound.
    ///
    /// A drained batch contains whole records. Splitting the batch at newlines
    /// keeps the bound meaningful when a batch is larger than a small
    /// `--log-max-bytes`; one individual record may still be larger than the
    /// bound, because truncating an arbitrary log line would silently lose
    /// more than the size limit itself.
    fn append_text(&mut self, text: &str) {
        for record in text.split_inclusive('\n') {
            if !self.append_record(record) {
                return;
            }
        }
    }

    fn append_record(&mut self, record: &str) -> bool {
        let bytes = record.as_bytes();
        // A single record larger than the whole file must not rotate once per
        // chunk: that would push every rotated history file out and leave only
        // the tail of one log line. Truncate it once, with a marker, and keep
        // the rotation budget for actual records.
        if bytes.len() as u64 > self.max_bytes {
            const MARKER: &[u8] = b"\n[record truncated]\n";
            let max = self.max_bytes as usize;
            let keep = max.saturating_sub(MARKER.len());
            let prefix = bytes.get(..keep).unwrap_or(bytes);
            let suffix = if max >= MARKER.len() { MARKER } else { &[] };
            if self.file.is_none() && !self.reopen() {
                return false;
            }
            if self.written > 0 && !self.rotate(max) {
                return false;
            }
            let Some(file) = self.file.as_mut() else {
                return false;
            };
            if let Err(e) = file.write_all(prefix).and_then(|()| file.write_all(suffix)) {
                self.warn(&e);
                return false;
            }
            self.written = (prefix.len() + suffix.len()) as u64;
            self.warned = false;
            return true;
        }
        let mut offset = 0;
        while offset < bytes.len() {
            if self.file.is_none() && !self.reopen() {
                return false;
            }
            if self.written >= self.max_bytes && !self.rotate(bytes.len() - offset) {
                return false;
            }
            let room = (self.max_bytes - self.written) as usize;
            let take = room.min(bytes.len() - offset);
            let Some(chunk) = bytes.get(offset..offset.saturating_add(take)) else {
                return false;
            };
            let Some(file) = self.file.as_mut() else {
                return false;
            };
            match file.write_all(chunk) {
                Ok(()) => {
                    self.written = self.written.saturating_add(take as u64);
                    self.warned = false;
                    offset += take;
                }
                // A log is diagnostic: a full disk must not take the system with
                // it, and it must not fill the console either - hence once.
                Err(e) => {
                    self.warn(&e);
                    return false;
                }
            }
        }
        true
    }

    /// Make room for the next write.
    ///
    /// With `keep == 0` this is the original policy: drop the file's contents
    /// and leave a marker. With `keep > 0` the current file becomes `path.1`,
    /// older files shift up, and the oldest is removed; the total on-disk bound
    /// is therefore roughly `(keep + 1) * max_bytes`.
    fn rotate(&mut self, needed: usize) -> bool {
        if self.keep == 0 {
            return self.truncate(needed);
        }

        let oldest = self.rotated_path(self.keep);
        if let Err(e) = std::fs::remove_file(&oldest)
            && e.kind() != io::ErrorKind::NotFound
        {
            self.warn(&e);
        }
        for index in (1..self.keep).rev() {
            let from = self.rotated_path(index);
            let to = self.rotated_path(index + 1);
            match std::fs::rename(&from, &to) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => self.warn(&e),
            }
        }
        match std::fs::rename(&self.path, self.rotated_path(1)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                self.warn(&e);
                // Rotation did not happen, but the current file is still the
                // one we own: fall back to the bounded truncate policy.
                return self.truncate(needed);
            }
        }
        // The old descriptor still names the renamed file; close it before
        // creating the new current file.
        self.file = None;
        if !self.reopen() {
            return false;
        }
        self.written = 0;
        true
    }

    /// Drop what is there and start again.
    ///
    /// The marker bytes count against the bound, so a file that is truncated at
    /// `max_bytes` does not silently exceed it just because it says so.
    fn truncate(&mut self, needed: usize) -> bool {
        if self.file.is_none() && !self.reopen() {
            return false;
        }
        let marker = b"log truncated: the previous contents were dropped\n";
        let marker_len = marker.len() as u64;
        let max_bytes = self.max_bytes;
        let marker_fits = marker_len.saturating_add(needed as u64) <= max_bytes;
        let outcome = {
            let Some(file) = self.file.as_mut() else {
                return false;
            };
            file.set_len(0).and_then(|()| {
                if marker_fits {
                    file.write_all(marker)
                } else {
                    Ok(())
                }
            })
        };
        match outcome {
            Ok(()) => {
                self.written = if marker_fits { marker_len } else { 0 };
                self.warned = false;
                true
            }
            Err(e) => {
                self.warn(&e);
                false
            }
        }
    }

    /// Reopen the current path after rotation (or after an earlier failure).
    fn reopen(&mut self) -> bool {
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(file) => {
                self.written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
                self.file = Some(file);
                true
            }
            Err(e) => {
                self.file = None;
                self.warn(&e);
                false
            }
        }
    }

    /// `path.N` for a rotated file.
    fn rotated_path(&self, index: usize) -> PathBuf {
        let mut name = self.path.as_os_str().to_os_string();
        name.push(format!(".{index}"));
        PathBuf::from(name)
    }

    fn warn(&mut self, error: &std::io::Error) {
        if !self.warned {
            rlog!(
                "warning: cannot maintain the log file {}: {error}",
                self.path.display()
            );
            self.warned = true;
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
    pub async fn persist_to(&self, path: &Path, max_bytes: u64, keep: usize) {
        let path = path.to_path_buf();
        let open_path = path.clone();
        let opened =
            tokio::task::spawn_blocking(move || LogFile::open(&open_path, max_bytes, keep)).await;
        match opened {
            Ok(Ok(file)) => {
                *lock(&self.file) = Some(Arc::new(FileQueue::new(file)));
                rlog!("logging to {}", path.display());
            }
            Ok(Err(e)) => {
                rlog!(
                    "warning: cannot persist logs to {}: {e}; keeping them in memory only",
                    path.display()
                );
            }
            Err(e) => {
                rlog!(
                    "warning: cannot open the log file task for {}: {e}; keeping them in memory only",
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
        // Bounded in both passes and wall-clock time: a service still
        // flooding at shutdown, or a disk that has stopped making progress,
        // must not hold the machine here. Whatever is left is already in the
        // ring and is lost with the process, which is the same trade the
        // bounded queue makes.
        let deadline = Instant::now() + Duration::from_millis(50);
        for _ in 0..64 {
            if Instant::now() >= deadline || queue.drain() == 0 {
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
    pub(crate) fn push(&self, unit: &UnitName, line: &str) {
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
                if let Some(last) = pending.rfind('\n') {
                    let complete = last + 1;
                    for line in pending[..complete].split_inclusive('\n') {
                        push_captured(&buffer, &file, &unit, trim_line(line));
                    }
                    pending.drain(..complete);
                }
                cap_pending(&mut pending);
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
        push_captured(&buffer, &file, &unit, trim_line(&pending));
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
fn trim_line(line: &str) -> &str {
    line.trim_end_matches(['\n', '\r'])
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
    #[tokio::test(flavor = "current_thread")]
    async fn a_configured_file_gets_the_same_lines_as_the_ring() {
        let dir = std::env::temp_dir().join(format!("rinit-logs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested/rinit.log");

        let logs = ServiceLogs::new();
        logs.persist_to(&path, DEFAULT_LOG_FILE_MAX_BYTES, 0).await;
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
        let mut file = LogFile::open(&path, 128, 0).expect("open");
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

    /// `--log-keep` keeps the newest files instead of dropping every old line.
    #[test]
    fn the_log_file_rotates_and_keeps_the_requested_history() {
        let dir = std::env::temp_dir().join(format!("rinit-logs-rotate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("rinit.log");
        let mut file = LogFile::open(&path, 64, 2).expect("open");
        for line in 0..12 {
            file.append_text(&format!("app.service: line {line:02}\n"));
        }

        assert!(path.exists(), "the current file must exist");
        assert!(file.rotated_path(1).exists(), "the newest rotation");
        assert!(file.rotated_path(2).exists(), "the oldest kept file");
        assert!(
            !file.rotated_path(3).exists(),
            "only --log-keep files are kept"
        );

        let current = std::fs::read_to_string(&path).expect("current file");
        assert!(current.contains("line 11"), "newest line: {current}");
        let oldest_kept = std::fs::read_to_string(file.rotated_path(2)).expect("second file");
        assert!(
            !oldest_kept.contains("line 00"),
            "the file beyond the keep count is gone: {oldest_kept}"
        );
        for kept in [&path, &file.rotated_path(1), &file.rotated_path(2)] {
            let len = std::fs::metadata(kept).expect("metadata").len();
            assert!(len <= 64 + 32, "bounded file {kept:?}: {len} bytes");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A rename that cannot happen must fall back to the bounded truncate
    /// policy instead of leaving the file unbounded.
    #[test]
    fn a_failed_rotation_falls_back_to_truncation() {
        let dir =
            std::env::temp_dir().join(format!("rinit-logs-rotate-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("rinit.log");
        let mut file = LogFile::open(&path, 64, 2).expect("open");
        file.append_text(&"a".repeat(64));
        for index in [1, 2] {
            let target = file.rotated_path(index);
            std::fs::create_dir_all(&target).expect("blocking directory");
            std::fs::write(target.join("keep"), b"x").expect("non-empty");
        }

        file.append_text("after\n");

        let written = std::fs::read_to_string(&path).expect("current file");
        assert!(
            written.contains("log truncated"),
            "fallback marker: {written}"
        );
        assert!(written.ends_with("after\n"), "newest line kept: {written}");
        assert!(file.rotated_path(1).is_dir(), "the blocker remains");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failed reopen is not fatal: the next append retries after the directory
    /// becomes usable again.
    #[test]
    fn a_failed_reopen_is_retried_on_the_next_write() {
        let dir = std::env::temp_dir().join(format!("rinit-logs-reopen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("rinit.log");
        let mut file = LogFile::open(&path, 1024, 0).expect("open");
        std::fs::remove_dir_all(&dir).expect("remove open file's directory");
        assert!(!file.reopen(), "the missing parent must fail the reopen");

        file.append_text("dropped-until-reopen\n");
        assert!(!path.exists(), "a failed reopen writes nothing");

        std::fs::create_dir_all(&dir).expect("recreate scratch dir");
        file.append_text("second\n");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            "second\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A bound smaller than the marker drops the marker rather than exceeding
    /// the bound just to announce a truncation.
    #[test]
    fn a_tiny_bound_skips_the_truncate_marker() {
        let dir = std::env::temp_dir().join(format!("rinit-logs-tiny-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("rinit.log");
        let mut file = LogFile::open(&path, 4, 0).expect("open");
        file.append_text("abc\n");
        file.append_text("x\n");

        let written = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(written, "x\n");
        assert!(!written.contains("log truncated"), "{written}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_batch_larger_than_the_bound_is_split_at_records() {
        let dir =
            std::env::temp_dir().join(format!("rinit-logs-batch-bound-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("rinit.log");
        let mut file = LogFile::open(&path, 64, 0).expect("open");
        let batch = (0..10)
            .map(|index| format!("app.service: line {index}\n"))
            .collect::<String>();
        file.append_text(&batch);
        let written = std::fs::read(&path).expect("read back");
        assert!(written.len() <= 64, "got {} bytes", written.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_oversized_record_does_not_evict_all_history() {
        let dir = std::env::temp_dir().join(format!(
            "rinit-logs-oversized-record-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("rinit.log");
        let mut file = LogFile::open(&path, 128, 2).expect("open");
        file.append_text("previous record\n");
        file.append_text(&"x".repeat(4096));

        let current = std::fs::read(&path).expect("current");
        assert!(
            current.len() <= 128,
            "current file bound: {}",
            current.len()
        );
        let previous = std::fs::read(file.rotated_path(1)).expect("rotated");
        assert!(
            String::from_utf8_lossy(&previous).contains("previous record"),
            "one oversized record must not flush the older history"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The persisted queue is bounded: a service logging faster than the disk
    /// costs memory in the queue, not in PID 1's address space. The oldest line
    /// goes and the file says how many.
    #[test]
    fn a_full_file_queue_drops_the_oldest_and_reports_it() {
        let path = std::env::temp_dir().join(format!("rinit-logs-queue-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let queue = FileQueue::new(LogFile::open(&path, 1 << 20, 0).expect("open"));
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
