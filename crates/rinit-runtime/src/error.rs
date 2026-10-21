//! Runtime error types (plan §2.3 "错误层次").
//!
//! `SyscallError` is the single structured form produced by the syscall
//! layer: every wrapper records the operation name and the `errno`, so errors
//! are never silently lost. `RuntimeError` aggregates syscall, I/O, core-logic
//! and task-join failures for the top-level binaries.

use std::fmt;
use std::io;

use rinit_core::CoreError;

/// A failed system call, carrying the operation name and `errno`.
#[derive(Debug, Clone)]
pub struct SyscallError {
    op: &'static str,
    errno: i32,
    /// What the call was for, when "mount failed" alone would not say (e.g. the
    /// mount point). `None` for calls whose operation name is the whole story.
    context: Option<String>,
}

impl SyscallError {
    /// Build from an explicit `errno` (e.g. the value `posix_spawn` returns).
    pub const fn new(op: &'static str, errno: i32) -> Self {
        SyscallError {
            op,
            errno,
            context: None,
        }
    }

    /// Capture the current thread's `errno`.
    pub fn last_os_error(op: &'static str) -> Self {
        SyscallError::new(op, io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }

    /// Build from an existing [`io::Error`].
    pub fn from_io(op: &'static str, err: &io::Error) -> Self {
        SyscallError::new(op, err.raw_os_error().unwrap_or(0))
    }

    /// Attach what the operation was for, so the message names the object and
    /// not just the syscall: `mount /proc failed: ...` instead of `mount
    /// failed: ...`, which sends the reader to a different mount point than the
    /// one that broke.
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }

    /// The operation that failed (e.g. `"mount"`).
    pub const fn op(&self) -> &'static str {
        self.op
    }

    /// What the operation was for, when it named something (e.g. `/proc`).
    pub fn context(&self) -> Option<&str> {
        self.context.as_deref()
    }

    /// The raw `errno` value.
    pub const fn errno(&self) -> i32 {
        self.errno
    }

    /// Reconstruct a `std::io::Error` from the recorded `errno`.
    pub fn io_error(&self) -> io::Error {
        io::Error::from_raw_os_error(self.errno)
    }
}

impl fmt::Display for SyscallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.op)?;
        if let Some(context) = &self.context {
            write!(f, " {context}")?;
        }
        write!(f, " failed: {} (errno {})", self.io_error(), self.errno)
    }
}

impl std::error::Error for SyscallError {}

/// Aggregated runtime error for the top-level binaries.
#[derive(Debug)]
#[non_exhaustive]
pub enum RuntimeError {
    /// A syscall-layer failure.
    Syscall(SyscallError),
    /// A generic I/O failure (safe `std::io` APIs such as `create_dir_all`).
    Io { op: &'static str, source: io::Error },
    /// A platform-independent core-logic failure.
    Core(CoreError),
    /// A spawned blocking task panicked or was cancelled.
    Join { op: &'static str, detail: String },
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeError::Syscall(e) => write!(f, "{e}"),
            RuntimeError::Io { op, source } => write!(f, "{op} failed: {source}"),
            RuntimeError::Core(e) => write!(f, "{e}"),
            RuntimeError::Join { op, detail } => write!(f, "{op} task failed: {detail}"),
        }
    }
}

impl std::error::Error for RuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RuntimeError::Syscall(e) => Some(e),
            RuntimeError::Io { source, .. } => Some(source),
            RuntimeError::Core(e) => Some(e),
            RuntimeError::Join { .. } => None,
        }
    }
}

impl From<SyscallError> for RuntimeError {
    fn from(e: SyscallError) -> Self {
        RuntimeError::Syscall(e)
    }
}

impl From<CoreError> for RuntimeError {
    fn from(e: CoreError) -> Self {
        RuntimeError::Core(e)
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    #[test]
    fn syscall_error_keeps_op_and_errno() {
        let e = SyscallError::new("mount", libc::EPERM);
        assert_eq!(e.op(), "mount");
        assert_eq!(e.errno(), libc::EPERM);
    }

    #[test]
    fn syscall_error_last_os_error_reads_the_current_errno() {
        // A failing syscall is the only portable way to set errno.
        // SAFETY: `close(-1)` cannot succeed, so no descriptor is affected.
        let failed = unsafe { libc::close(-1) };
        assert_eq!(failed, -1, "close(-1) must fail");

        let e = SyscallError::last_os_error("close");
        assert_eq!(e.op(), "close");
        assert_eq!(e.errno(), libc::EBADF);
    }

    #[test]
    fn syscall_error_from_io_preserves_os_errors() {
        let io = io::Error::from_raw_os_error(libc::ENOENT);
        let e = SyscallError::from_io("open", &io);
        assert_eq!(e.errno(), libc::ENOENT);
        assert_eq!(e.op(), "open");
    }

    /// A non-OS error carries no errno; the wrapper records 0 rather than
    /// inventing a value, and the accessors must not panic on it.
    #[test]
    fn syscall_error_from_io_handles_non_os_errors() {
        let io = io::Error::new(io::ErrorKind::InvalidInput, "no errno here");
        let e = SyscallError::from_io("parse", &io);
        assert_eq!(e.errno(), 0);
        assert_eq!(e.op(), "parse");
    }

    /// The errno survives the round trip so callers can match on it again.
    #[test]
    fn syscall_error_round_trips_through_io_error() {
        let e = SyscallError::new("kill", libc::ESRCH);
        assert_eq!(e.io_error().raw_os_error(), Some(libc::ESRCH));
    }

    #[test]
    fn syscall_error_display_names_op_and_errno() {
        let msg = SyscallError::new("kill", libc::ESRCH).to_string();
        assert!(msg.contains("kill"), "got: {msg}");
        assert!(msg.contains(&libc::ESRCH.to_string()), "got: {msg}");
    }

    #[test]
    fn runtime_error_from_syscall_preserves_payload() {
        let err = RuntimeError::from(SyscallError::new("mount", libc::EPERM));
        assert!(matches!(err, RuntimeError::Syscall(_)));
        assert!(err.to_string().contains("mount"), "got: {err}");
    }

    /// `source()` is what makes error chains useful, so every variant must
    /// implement it deliberately.
    #[test]
    fn runtime_error_sources_are_wired() {
        let syscall = RuntimeError::Syscall(SyscallError::new("mount", libc::EPERM));
        assert!(syscall.source().is_some());

        let io = RuntimeError::Io {
            op: "create_dir_all",
            source: io::Error::from_raw_os_error(libc::EIO),
        };
        assert!(io.source().is_some());

        let core = RuntimeError::Core(CoreError::InvalidUnitName("bad name".to_string()));
        assert!(core.source().is_some());

        // A join failure has no underlying error object, only a message.
        let join = RuntimeError::Join {
            op: "mount",
            detail: "task panicked".to_string(),
        };
        assert!(join.source().is_none());
    }

    #[test]
    fn runtime_error_io_display_mentions_op_and_cause() {
        let err = RuntimeError::Io {
            op: "create_dir_all",
            source: io::Error::from_raw_os_error(libc::EACCES),
        };
        let msg = err.to_string();
        assert!(msg.contains("create_dir_all"), "got: {msg}");
        assert!(msg.contains(&libc::EACCES.to_string()), "got: {msg}");
    }

    #[test]
    fn runtime_error_join_display_mentions_op_and_detail() {
        let err = RuntimeError::Join {
            op: "mount",
            detail: "task panicked".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("mount"), "got: {msg}");
        assert!(msg.contains("task panicked"), "got: {msg}");
    }

    #[test]
    fn runtime_error_from_core_preserves_payload() {
        let err = RuntimeError::from(CoreError::InvalidUnitName("bad name".to_string()));
        assert!(matches!(err, RuntimeError::Core(_)));
        assert!(err.to_string().contains("bad name"), "got: {err}");
    }

    #[test]
    fn a_context_names_the_object_in_the_message() {
        let err = SyscallError::from_io("mount", &io::Error::from_raw_os_error(libc::ENOENT))
            .with_context("/sys");
        let text = err.to_string();
        assert!(text.starts_with("mount /sys failed:"), "{text}");
        assert_eq!(err.context(), Some("/sys"));
    }

    #[test]
    fn a_context_is_optional() {
        let err = SyscallError::new("fork", libc::EAGAIN);
        assert_eq!(err.context(), None);
        assert!(err.to_string().starts_with("fork failed:"));
    }

    /// A runtime error must stay usable as a boxed `dyn Error`; the binaries
    /// return these from `main`, so this is on the real path.
    #[test]
    fn runtime_error_is_a_usable_std_error() {
        let err: Box<dyn std::error::Error> = Box::new(RuntimeError::Syscall(SyscallError::new(
            "kill",
            libc::ESRCH,
        )));
        assert!(err.to_string().contains("kill"));
    }
}
