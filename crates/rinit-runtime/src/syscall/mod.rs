//! Thin, audited wrappers over the raw `libc` syscalls rinit needs (plan §2.3).
//!
//! This is the *only* module tree allowed to contain `unsafe`. Every wrapper
//! returns `Result<_, SyscallError>` and records the `errno` on failure;
//! business logic (`manager`, `unit`, `boot`) stays free of raw `unsafe`.

pub mod mount;
pub mod process;
pub mod reboot;
pub mod signal;
pub mod wait;
