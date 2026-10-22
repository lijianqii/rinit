//! Thin, audited wrappers over the raw `libc` syscalls rinit needs (plan §2.3).
//!
//! Most of rinit's `unsafe` is here and in [`crate::signals`] (the `sigaction`
//! handlers and the self-pipe). The other audited call sites, each with its own
//! SAFETY note, are three `AsyncFd::register` calls (`crate::signals`,
//! `rinit/src/logs.rs`, `rinit/src/activation.rs`) - the safe `AsyncFd::new` is
//! deprecated as *unsound*, because it cannot know the caller keeps the
//! descriptor valid - and the `kill(pid, 0)` existence probe in
//! `rinit/src/unit_manager.rs`. Everything else - `mounts`, `fstab`, `units`, and
//! the whole of `rinit-core` (which is `#![forbid(unsafe_code)]`) - is safe code.
//! Every wrapper here returns
//! `Result<_, SyscallError>` and records the `errno` on failure, so business logic
//! (`manager`, `unit`, `boot`) never sees a raw errno it has to remember.

use crate::error::SyscallError;
use std::ffi::CString;

pub mod cgroup;
pub mod cred;
pub mod fs;
pub mod mount;
pub mod process;
pub mod reboot;
pub mod signal;
pub mod socket;
pub mod wait;
pub mod watchdog;

/// Convert `s` into a `CString` for a syscall argument.
///
/// Every wrapper that passes a path, program name or option through `libc`
/// needs this, and getting it wrong means silent truncation: an interior NUL
/// would turn `/bin/foo\0bar` into a *successful* call on `/bin/foo`.
pub fn cstring(op: &'static str, s: &str) -> Result<CString, SyscallError> {
    CString::new(s).map_err(|_| SyscallError::new(op, libc::EINVAL))
}

/// An `ioctl` request constant as the type this target's `ioctl` wants.
///
/// glibc's prototype takes `unsigned long` and musl's takes `int` - libc's
/// `Ioctl` is `c_ulong` on the first and `c_int` on the second - while the
/// request constants (`TIOCSCTTY`, `WDIOC_SETTIMEOUT`, ...) are `c_ulong`
/// either way. So every request has to be cast, and the cast is a
/// reinterpretation rather than a conversion: `WDIOC_SETTIMEOUT` is above
/// `i32::MAX`, and its bit pattern is what the kernel compares. Without this the
/// musl target does not compile at all, which is how it was found.
#[must_use]
pub const fn ioctl_request(request: libc::c_ulong) -> libc::Ioctl {
    request as libc::Ioctl
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_string_converts() {
        assert_eq!(cstring("op", "/bin/sh").unwrap().to_bytes(), b"/bin/sh");
    }

    #[test]
    fn empty_string_converts_to_an_empty_c_string() {
        // Valid, and distinct from a null pointer: some args allow "".
        assert_eq!(cstring("op", "").unwrap().to_bytes(), b"");
    }

    /// The reason this helper exists: an interior NUL must be an error, never
    /// a silent truncation to a different path.
    #[test]
    fn interior_nul_is_rejected_with_einval() {
        let err = cstring("mount", "/bin/foo\0bar").expect_err("interior NUL must fail");
        assert_eq!(err.errno(), libc::EINVAL);
        assert_eq!(err.op(), "mount");
    }

    #[test]
    fn trailing_nul_is_rejected_too() {
        assert!(cstring("op", "/bin/sh\0").is_err());
    }
}
