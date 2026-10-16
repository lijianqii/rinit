//! rinit-runtime - platform-dependent runtime primitives for rinit.
//!
//! Owns the single audited `libc` boundary (`syscall`) plus the mount and
//! reaping helpers built on top of it. Depends on `std`, `libc` and (in the
//! binaries) `tokio`; per plan §2.3 it does *not* pull in `nix`.
//!
//! M0 scope: first-wave mounts, `posix_spawn`/`kill`, non-blocking reaping and
//! the `mount`/`reboot`/`sync` wrappers.

// Production code must not panic (plan §3 M4). This crate is the `libc`
// boundary, so "the syscall returned an error we did not expect" is a normal
// state to report, never a reason to abort PID 1.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing
    )
)]

pub mod error;
pub mod mounts;
pub mod signals;
pub mod syscall;
pub mod units;

pub use error::{RuntimeError, SyscallError};
pub use mounts::{
    MountOps, MountReport, RealMounts, UnmountReport, first_wave_mounts, first_wave_mounts_with,
    unmount_filesystems, unmount_order,
};
pub use signals::{Signal, SignalSet};
pub use units::{DEFAULT_TARGET, DEFAULT_UNIT_DIR, UnitFile, load_unit_files};
