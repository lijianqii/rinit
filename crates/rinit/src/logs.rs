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

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::{Arc, Mutex};

use rinit_core::{LogBuffer, LogLine, UnitName};
use rinit_runtime::RuntimeError;
use rinit_runtime::syscall::fs;
use tokio::io::unix::AsyncFd;

use crate::log::rlog;

/// How much of a read is buffered before splitting into lines.
const READ_CHUNK: usize = 4096;

/// The shared log ring and the tasks filling it.
pub struct ServiceLogs {
    buffer: Arc<Mutex<LogBuffer>>,
}

impl ServiceLogs {
    /// A logs store with the default buffer bounds.
    pub fn new() -> Self {
        ServiceLogs {
            buffer: Arc::new(Mutex::new(LogBuffer::with_defaults())),
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
        let unit = unit.clone();
        // Spawning here is safe: `capture` is only called from the async boot
        // path, which runs inside the runtime.
        tokio::spawn(async move { pump(reader, unit, buffer).await });
        Ok(writer)
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
fn lock(buffer: &Arc<Mutex<LogBuffer>>) -> std::sync::MutexGuard<'_, LogBuffer> {
    buffer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Read `reader` until end-of-file, appending whole lines to `buffer`.
async fn pump(reader: OwnedFd, unit: UnitName, buffer: Arc<Mutex<LogBuffer>>) {
    // SAFETY: `reader` owns a valid descriptor that stays open for the lifetime
    // of the returned `AsyncFd`; it is never closed or replaced.
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
                pending.push_str(&String::from_utf8_lossy(&buf[..n]));
                let mut lines: Vec<String> = Vec::new();
                while let Some(newline) = pending.find('\n') {
                    let line: String = pending.drain(..=newline).collect();
                    lines.push(trim_line(&line));
                }
                if !lines.is_empty() {
                    let mut buffer = lock(&buffer);
                    for line in lines {
                        buffer.push(&unit, &line);
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
        lock(&buffer).push(&unit, &line);
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
