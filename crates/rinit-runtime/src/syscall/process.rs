//! Process creation and signalling (plan §2.3).
//!
//! M0 uses `posix_spawn` for the hardcoded test service. The controlled
//! `fork`+`exec` path (for `User=`, capabilities, etc.) arrives in M5 and must
//! obey the single-threaded-fork constraint documented in plan §2.1⑤.

use std::ffi::{CStr, CString};

use crate::error::SyscallError;

/// The attributes a spawned service gets.
///
/// Kept as a struct rather than a bare `posix_spawn` call because plan §1.3(6)
/// requires each service to become a *process-group leader*: stopping it then
/// signals `kill(-pgid, ...)`, which reaches children the service forked
/// itself. Signalling only the direct child would leave those grandchildren
/// running and holding the port the next start wants.
#[derive(Debug, Clone, Copy, Default)]
pub struct SpawnOptions {
    /// Create a new process group with the child as leader.
    pub new_process_group: bool,
}

impl SpawnOptions {
    /// Spawn into a fresh process group, so the whole tree can be signalled.
    pub const fn with_process_group() -> Self {
        SpawnOptions {
            new_process_group: true,
        }
    }
}

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
    posix_spawn_with(path, argv, envp, SpawnOptions::default())
}

/// [`posix_spawn`] with explicit options.
pub fn posix_spawn_with(
    path: &CStr,
    argv: &[CString],
    envp: &[CString],
    options: SpawnOptions,
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

    // POSIX_SPAWN_SETPGROUP is the portable way to ask for a new process
    // group; the child then also becomes a session leader's child with pgid ==
    // its own pid. Building attrs by hand (rather than the libc
    // `posix_spawnattr_t` wrapper, which glibc exposes but musl does not)
    // keeps this file buildable on both targets.
    let mut attrs: libc::posix_spawnattr_t = unsafe { std::mem::zeroed() };
    if options.new_process_group {
        // SAFETY: a zeroed attr struct with a flags field is the documented
        // starting point for `posix_spawnattr_init`; it is only read back
        // inside this call.
        if unsafe { libc::posix_spawnattr_init(&mut attrs) } != 0 {
            return Err(SyscallError::last_os_error("posix_spawnattr_init"));
        }
        let mut flags: libc::c_short = 0;
        // SAFETY: `posix_spawnattr_getflags` writes through the pointer it is
        // given, and we pass a valid local.
        unsafe { libc::posix_spawnattr_getflags(&mut attrs, &mut flags) };
        flags |= libc::POSIX_SPAWN_SETPGROUP as libc::c_short;
        // SAFETY: as above.
        unsafe { libc::posix_spawnattr_setflags(&mut attrs, flags) };
    }
    let attr_ptr = if options.new_process_group {
        &attrs as *const libc::posix_spawnattr_t
    } else {
        std::ptr::null()
    };

    let mut pid: libc::pid_t = 0;
    // SAFETY: `path` is a valid NUL-terminated C string; `argv_ptrs`/`env_ptrs`
    // are NULL-terminated arrays of valid pointers that outlive the call.
    // `attr_ptr` is either null (default behaviour) or an initialised
    // `posix_spawnattr_t`.
    let rc = unsafe {
        libc::posix_spawn(
            &mut pid,
            path.as_ptr(),
            std::ptr::null(),
            attr_ptr,
            argv_ptrs.as_ptr(),
            env_ptrs.as_ptr(),
        )
    };

    if options.new_process_group {
        // SAFETY: `attrs` was initialised above and is not used afterwards.
        unsafe { libc::posix_spawnattr_destroy(&mut attrs) };
    }

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

/// Send `signal` to every process in `pid`'s process group.
///
/// `pid` is expected to be a group leader created by
/// [`SpawnOptions::with_process_group`]. This is how a stop reaches services
/// that fork their own children (plan §1.3(6)): signalling the group delivers
/// to all of them, where `kill(pid, ...)` would only reach the direct child.
///
/// # Errors
/// Returns `ESRCH` if the group is already empty, which is the normal outcome
/// when the service exited between the check and the signal.
pub fn kill_process_group(pid: libc::pid_t, signal: libc::c_int) -> Result<(), SyscallError> {
    // SAFETY: a negative pid addresses a process group; no memory-safety
    // preconditions apply to a scalar argument.
    let rc = unsafe { libc::kill(-pid, signal) };
    if rc == 0 {
        Ok(())
    } else {
        Err(SyscallError::last_os_error("killpg"))
    }
}

/// Return the current process ID.
pub fn getpid() -> libc::pid_t {
    // SAFETY: `getpid` takes no arguments and cannot fail.
    unsafe { libc::getpid() }
}

/// Return `pid`'s process group, if it is alive.
///
/// # Errors
/// Returns the errno from `getpgid`, `ESRCH` when the process is gone.
pub fn getpgid(pid: libc::pid_t) -> Result<libc::pid_t, SyscallError> {
    // SAFETY: a scalar pid argument; the return value is read before use.
    let rc = unsafe { libc::getpgid(pid) };
    if rc >= 0 {
        Ok(rc)
    } else {
        Err(SyscallError::last_os_error("getpgid"))
    }
}
