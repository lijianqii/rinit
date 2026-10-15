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
}

impl SyscallError {
    /// Build from an explicit `errno` (e.g. the value `posix_spawn` returns).
    pub const fn new(op: &'static str, errno: i32) -> Self {
        SyscallError { op, errno }
    }

    /// Capture the current thread's `errno`.
    pub fn last_os_error(op: &'static str) -> Self {
        SyscallError::new(op, io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }

    /// Build from an existing [`io::Error`].
    pub fn from_io(op: &'static str, err: &io::Error) -> Self {
        SyscallError::new(op, err.raw_os_error().unwrap_or(0))
    }

    /// The operation that failed (e.g. `"mount"`).
    pub const fn op(&self) -> &'static str {
        self.op
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
        write!(
            f,
            "{} failed: {} (errno {})",
            self.op,
            self.io_error(),
            self.errno
        )
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
