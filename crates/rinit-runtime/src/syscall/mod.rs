//! Thin, audited wrappers over the raw `libc` syscalls rinit needs (plan §2.3).
//!
//! This is the *only* module tree allowed to contain `unsafe`. Every wrapper
//! returns `Result<_, SyscallError>` and records the `errno` on failure;
//! business logic (`manager`, `unit`, `boot`) stays free of raw `unsafe`.

use std::ffi::CString;

use crate::error::SyscallError;

pub mod fs;
pub mod mount;
pub mod process;
pub mod reboot;
pub mod signal;
pub mod wait;

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
