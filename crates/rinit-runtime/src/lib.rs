//! rinit-runtime - platform-dependent runtime primitives for rinit.
//!
//! Owns the single audited `libc` boundary (`syscall`) plus the mount and
//! reaping helpers built on top of it. Depends on `std`, `libc` and (in the
//! binaries) `tokio`; per plan §2.3 it does *not* pull in `nix`.
//!
//! M0 scope: first-wave mounts, `posix_spawn`/`kill`, non-blocking reaping and
//! the `mount`/`reboot`/`sync` wrappers.

pub mod error;
pub mod mounts;
pub mod signals;
pub mod syscall;

pub use error::{RuntimeError, SyscallError};
pub use mounts::{MountReport, first_wave_mounts};
pub use signals::{Signal, SignalSet};
