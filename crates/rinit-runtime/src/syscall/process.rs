//! Process creation and signalling (plan §2.3).
//!
//! M0 uses `posix_spawn` for the hardcoded test service. The controlled
//! `fork`+`exec` path (for `User=`, capabilities, etc.) arrives in M5 and must
//! obey the single-threaded-fork constraint documented in plan §2.1⑤.

use std::ffi::{CStr, CString};

use crate::error::SyscallError;

/// Spawn `path` with `argv`/`envp` via `posix_spawn(3)`.
///
/// `argv` should include `argv[0]`. A terminating NULL is appended to both
/// vectors automatically.
///
/// # Errors
/// `posix_spawn` returns the error number directly (it does not set `errno`);
/// that value is preserved in the [`SyscallError`].
pub fn posix_spawn(
    path: &CStr,
    argv: &[CString],
    envp: &[CString],
) -> Result<libc::pid_t, SyscallError> {
    let mut argv_ptrs: Vec<*mut libc::c_char> = argv
        .iter()
        .map(|a| a.as_ptr() as *mut libc::c_char)
        .collect();
    argv_ptrs.push(std::ptr::null_mut());

    let mut env_ptrs: Vec<*mut libc::c_char> = envp
        .iter()
        .map(|e| e.as_ptr() as *mut libc::c_char)
        .collect();
    env_ptrs.push(std::ptr::null_mut());

    let mut pid: libc::pid_t = 0;
    // SAFETY: `path` is a valid NUL-terminated C string; `argv_ptrs`/`env_ptrs`
    // are NULL-terminated arrays of valid pointers that outlive the call. NULL
    // file-actions/attr args request default behaviour.
    let rc = unsafe {
        libc::posix_spawn(
            &mut pid,
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            argv_ptrs.as_ptr(),
            env_ptrs.as_ptr(),
        )
    };
    if rc == 0 {
        Ok(pid)
    } else {
        Err(SyscallError::new("posix_spawn", rc))
    }
}

/// Send `signal` to `pid` (a positive PID, not a process group).
pub fn kill(pid: libc::pid_t, signal: libc::c_int) -> Result<(), SyscallError> {
    // SAFETY: `kill` has no memory-safety preconditions for a scalar pid/signal.
    let rc = unsafe { libc::kill(pid, signal) };
    if rc == 0 {
        Ok(())
    } else {
        Err(SyscallError::last_os_error("kill"))
    }
}

/// Return the current process ID.
pub fn getpid() -> libc::pid_t {
    // SAFETY: `getpid` takes no arguments and cannot fail.
    unsafe { libc::getpid() }
}
