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
/// requires each service to be signalable as a whole tree: stopping it then
/// signals `kill(-pgid, ...)`, which reaches children the service forked
/// itself. Signalling only the direct child would leave those grandchildren
/// running and holding the port the next start wants.
#[derive(Debug, Clone, Copy, Default)]
pub struct SpawnOptions {
    /// Start the child in a new *session* (`setsid`).
    pub new_session: bool,
}

impl SpawnOptions {
    /// Spawn into a fresh session, so the whole tree can be signalled.
    ///
    /// A new session also gives a new process group with `pgid == pid`, so
    /// `kill(-pgid, ...)` behaves exactly as it would for a plain process-group
    /// leader - but `setsid` is what services actually expect. `getty` is the
    /// reason this is not merely `setpgid`: it calls `setsid()` itself and
    /// *dies* if it is already a process-group leader (`setsid` fails with
    /// `EPERM` unless the caller is not a group leader), which is exactly what
    /// a bare `setpgid` would make it. As a session leader, `getty`'s own
    /// `setsid()` fails harmlessly and it carries on.
    pub const fn with_session() -> Self {
        SpawnOptions { new_session: true }
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
    posix_spawn_full(path, argv, envp, options, None)
}

/// [`posix_spawn_with`] plus a working directory for the child.
///
/// `WorkingDirectory=` (plan §1.1) is applied in the child between `fork` and
/// `exec` through `posix_spawn_file_actions_addchdir_np`, so the child never
/// runs in the wrong directory. A directory that does not exist makes
/// `posix_spawn` fail, which is the fail-closed behaviour plan §7.4 asks for.
pub fn posix_spawn_full(
    path: &CStr,
    argv: &[CString],
    envp: &[CString],
    options: SpawnOptions,
    working_directory: Option<&CStr>,
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

    // POSIX_SPAWN_SETSID asks the child to `setsid()`, which glibc performs
    // before `exec`. Building attrs by hand (rather than the libc
    // `posix_spawnattr_t` wrapper, which glibc exposes but musl does not) keeps
    // this file buildable on both targets.
    let mut attrs: libc::posix_spawnattr_t = unsafe { std::mem::zeroed() };
    let use_attrs = options.new_session;
    if use_attrs {
        // SAFETY: a zeroed attr struct with a flags field is the documented
        // starting point for `posix_spawnattr_init`; it is only read back
        // inside this call.
        if unsafe { libc::posix_spawnattr_init(&mut attrs) } != 0 {
            return Err(SyscallError::last_os_error("posix_spawnattr_init"));
        }
        let mut flags: libc::c_short = 0;
        // SAFETY: `posix_spawnattr_getflags` takes the attributes by const
        // pointer and writes the flags through the pointer it is given; both
        // are valid locals.
        let rc = unsafe { libc::posix_spawnattr_getflags(&attrs, &mut flags) };
        if rc != 0 {
            // SAFETY: `attrs` was initialised above.
            unsafe { libc::posix_spawnattr_destroy(&mut attrs) };
            return Err(SyscallError::new("posix_spawnattr_getflags", rc));
        }
        flags |= libc::POSIX_SPAWN_SETSID as libc::c_short;
        // SAFETY: as above.
        let rc = unsafe { libc::posix_spawnattr_setflags(&mut attrs, flags) };
        if rc != 0 {
            // SAFETY: `attrs` was initialised above.
            unsafe { libc::posix_spawnattr_destroy(&mut attrs) };
            return Err(SyscallError::new("posix_spawnattr_setflags", rc));
        }
    }
    let attr_ptr = if use_attrs {
        &attrs as *const libc::posix_spawnattr_t
    } else {
        std::ptr::null()
    };

    let mut file_actions: libc::posix_spawn_file_actions_t = unsafe { std::mem::zeroed() };
    let use_file_actions = working_directory.is_some();
    if let Some(cwd) = working_directory {
        // SAFETY: a zeroed file-actions struct is the documented starting point
        // for `posix_spawn_file_actions_init`; `cwd` is a valid C string.
        if unsafe { libc::posix_spawn_file_actions_init(&mut file_actions) } != 0 {
            if use_attrs {
                // SAFETY: `attrs` was initialised above.
                unsafe { libc::posix_spawnattr_destroy(&mut attrs) };
            }
            return Err(SyscallError::last_os_error("posix_spawn_file_actions_init"));
        }
        let rc =
            unsafe { libc::posix_spawn_file_actions_addchdir_np(&mut file_actions, cwd.as_ptr()) };
        if rc != 0 {
            // SAFETY: both structs were initialised above.
            unsafe {
                libc::posix_spawn_file_actions_destroy(&mut file_actions);
                if use_attrs {
                    libc::posix_spawnattr_destroy(&mut attrs);
                }
            }
            return Err(SyscallError::new(
                "posix_spawn_file_actions_addchdir_np",
                rc,
            ));
        }
    }
    let file_actions_ptr = if use_file_actions {
        &file_actions as *const libc::posix_spawn_file_actions_t
    } else {
        std::ptr::null()
    };

    let mut pid: libc::pid_t = 0;
    // SAFETY: `path` is a valid NUL-terminated C string; `argv_ptrs`/`env_ptrs`
    // are NULL-terminated arrays of valid pointers that outlive the call.
    // `attr_ptr`/`file_actions_ptr` are either null or initialised structs.
    let rc = unsafe {
        libc::posix_spawn(
            &mut pid,
            path.as_ptr(),
            file_actions_ptr,
            attr_ptr,
            argv_ptrs.as_ptr(),
            env_ptrs.as_ptr(),
        )
    };

    // SAFETY: both structs were initialised above and are not used afterwards.
    unsafe {
        if use_file_actions {
            libc::posix_spawn_file_actions_destroy(&mut file_actions);
        }
        if use_attrs {
            libc::posix_spawnattr_destroy(&mut attrs);
        }
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

/// Send `signal` to every process in `pid`'s process group.///
/// `pid` is expected to lead its own process group, as created by
/// [`SpawnOptions::with_session`]. This is how a stop reaches services
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
