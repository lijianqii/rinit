//! Thin, audited wrappers over the raw `libc` syscalls rinit needs (plan §2.3).
//!
//! Only two module trees contain `unsafe`: this one and [`crate::signals`], which
//! installs the `sigaction` handlers and owns the self-pipe. Everything else -
//! `mounts`, `fstab`, `units`, and the whole of `rinit-core` (which is
//! `#![forbid(unsafe_code)]`) - is safe code. Every wrapper here returns
//! `Result<_, SyscallError>` and records the `errno` on failure, so business logic
//! (`manager`, `unit`, `boot`) never sees a raw errno it has to remember.

use std::ffi::CString;

use crate::error::SyscallError;

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
