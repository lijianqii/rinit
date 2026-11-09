//! Process creation and signalling (plan §2.3).
//!
//! M0 uses `posix_spawn` for the hardcoded test service. The controlled
//! `fork`+`exec` path (for `User=`, capabilities, `WorkingDirectory=` and
//! cgroup limits) runs between `fork` and `exec` under the async-signal-safe
//! constraint documented in plan §2.1⑤: the child calls only `libc`, allocates
//! nothing and takes no locks.

use std::ffi::{CStr, CString};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rinit_core::unit::sandbox::{Sandbox, SandboxStep};

use crate::error::SyscallError;
use crate::syscall::{cred, fs, ioctl_request};

/// The attributes a spawned service gets.
///
/// Kept as a struct rather than a bare `posix_spawn` call because plan §1.3(6)
/// requires each service to be signalable as a whole tree: stopping it then
/// signals `kill(-pgid, ...)`, which reaches children the service forked
/// itself. Signalling only the direct child would leave those grandchildren
/// running and holding the port the next start wants.
/// `ioctl(2)` request that makes the file behind the descriptor the calling
/// process's controlling terminal (plan §7.11).
///
/// `_IO('T', 0x0E)` from `include/uapi/asm-generic/ioctls.h`. `libc` does not
/// export it for Linux - only for the BSDs and Fuchsia - so it is spelled out
/// here, and [`the_terminal_ioctls_are_the_ones_the_kernel_header_defines`]
/// recomputes it from the `_IO` layout rather than trusting the literal.
pub const TIOCSCTTY: libc::c_ulong = 0x540E;

/// `ioctl(2)` request that hangs up the terminal: the queues are flushed and the
/// session using it is signalled (plan §7.11's `TTYReset=`).
///
/// `_IO('T', 0x37)`, same header, same reason as [`TIOCSCTTY`].
pub const TIOCVHANGUP: libc::c_ulong = 0x5437;

/// The descriptor a socket-activated service finds its socket on
/// (plan §7.9).
///
/// A fixed number rather than a range: the plan asks for one listening socket per
/// unit, and fd 3 is where every socket-activation protocol puts it.
/// `LISTEN_FDS=1` in the child's environment says how many are there.
pub const LISTEN_FD: libc::c_int = 3;

/// A controlling terminal to hand a child (plan §7.11).
///
/// `TTYPath=` plus `TTYReset=`: the device to open, and whether to hang it up
/// first so a respawned console service starts on a clean terminal.
#[derive(Debug, Clone, Copy)]
pub struct ChildTty<'a> {
    /// The device, e.g. `/dev/ttyAMA0`.
    pub path: &'a CStr,
    /// Whether to [`TIOCVHANGUP`] it when taking it over.
    pub reset: bool,
}

/// The child-side setup that borrows a path rather than owning a flag.
///
/// Kept together because both are "a file the child opens and acts on after the
/// fork, before it drops anything": the unit's `cgroup.procs` (plan §7.3) and its
/// controlling terminal (plan §7.11). They also arrive together - a service that
/// asks for either leaves the `posix_spawn` fast path, because neither can be
/// done without running code in the child.
#[derive(Debug, Clone, Copy, Default)]
pub struct ChildPaths<'a> {
    /// The unit's `cgroup.procs`, when the unit asked for cgroup limits.
    pub cgroup_procs: Option<&'a CStr>,
    /// The controlling terminal to take over, when the unit asked for one.
    pub tty: Option<ChildTty<'a>>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SpawnOptions {
    /// Start the child in a new *session* (`setsid`).
    pub new_session: bool,
    /// The listening socket to hand the child on [`LISTEN_FD`] (plan §7.9).
    ///
    /// This is only usable through [`spawn_sandboxed`]: the child must also
    /// write its own pid into `LISTEN_PID`, which `posix_spawn` cannot do
    /// before exec. [`posix_spawn_full`] rejects the option rather than
    /// starting a service that cannot find its socket.
    pub listen_fd: Option<libc::c_int>,
    /// Redirect the child's `stdout` and `stderr` onto this descriptor.
    ///
    /// Used to capture service output for `rctl log` (plan §1.3(7)). `None`
    /// leaves both inherited, which is what the tests and the auxiliary
    /// commands use.
    pub stdio: Option<libc::c_int>,
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
        SpawnOptions {
            new_session: true,
            listen_fd: None,
            stdio: None,
        }
    }

    /// Hand the child a listening socket on fd 3 (plan §7.9).
    pub const fn with_listen_fd(mut self, fd: libc::c_int) -> Self {
        self.listen_fd = Some(fd);
        self
    }

    /// Redirect the child's output onto `fd`.
    pub const fn with_stdio(mut self, fd: libc::c_int) -> Self {
        self.stdio = Some(fd);
        self
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
    let mut argv_ptrs: Vec<*mut libc::c_char> = Vec::with_capacity(argv.len() + 1);
    argv_ptrs.extend(argv.iter().map(|a| a.as_ptr() as *mut libc::c_char));
    argv_ptrs.push(std::ptr::null_mut());

    let mut env_ptrs: Vec<*mut libc::c_char> = Vec::with_capacity(envp.len() + 1);
    env_ptrs.extend(envp.iter().map(|e| e.as_ptr() as *mut libc::c_char));
    env_ptrs.push(std::ptr::null_mut());

    if options.listen_fd.is_some() {
        // A listening socket also needs the child's real pid in `LISTEN_PID`,
        // which `posix_spawn` cannot know before the exec. Only the controlled
        // fork path can satisfy the sd_listen_fds contract.
        return Err(SyscallError::new("posix_spawn", libc::EINVAL));
    }

    // Attributes are always used: even without `setsid`, the child must start
    // with default signal dispositions. POSIX_SPAWN_SETSID asks the child to
    // `setsid()`, which glibc performs before `exec`. Building attrs by hand
    // (rather than the libc `posix_spawnattr_t` wrapper, which glibc exposes
    // but musl does not) keeps this file buildable on both targets.
    let mut attrs: libc::posix_spawnattr_t = unsafe { std::mem::zeroed() };
    let use_attrs = true;
    if use_attrs {
        // SAFETY: a zeroed attr struct with a flags field is the documented
        // starting point for `posix_spawnattr_init`; it is only read back
        // inside this call.
        let rc = unsafe { libc::posix_spawnattr_init(&mut attrs) };
        if rc != 0 {
            return Err(SyscallError::new("posix_spawnattr_init", rc));
        }
        let mut flags: libc::c_short = 0;
        if options.new_session {
            flags |= libc::POSIX_SPAWN_SETSID as libc::c_short;
        }

        // `SIG_IGN` survives exec; reset the whole set to default and clear the
        // inherited mask, just as the controlled fork path does.
        // SAFETY: both sets are valid locals initialised by the libc helpers.
        let mut defaults: libc::sigset_t = unsafe { std::mem::zeroed() };
        let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigfillset(&mut defaults);
            libc::sigdelset(&mut defaults, libc::SIGKILL);
            libc::sigdelset(&mut defaults, libc::SIGSTOP);
            libc::sigemptyset(&mut mask);
        }
        // SAFETY: `attrs` is initialized, and both sets outlive the calls.
        let rc = unsafe { libc::posix_spawnattr_setsigdefault(&mut attrs, &defaults) };
        if rc != 0 {
            // SAFETY: `attrs` was initialised above.
            unsafe { libc::posix_spawnattr_destroy(&mut attrs) };
            return Err(SyscallError::new("posix_spawnattr_setsigdefault", rc));
        }
        // SAFETY: as above.
        let rc = unsafe { libc::posix_spawnattr_setsigmask(&mut attrs, &mask) };
        if rc != 0 {
            // SAFETY: `attrs` was initialised above.
            unsafe { libc::posix_spawnattr_destroy(&mut attrs) };
            return Err(SyscallError::new("posix_spawnattr_setsigmask", rc));
        }
        flags |= libc::POSIX_SPAWN_SETSIGDEF as libc::c_short
            | libc::POSIX_SPAWN_SETSIGMASK as libc::c_short;
        // SAFETY: as above.
        let rc = unsafe { libc::posix_spawnattr_setflags(&mut attrs, flags) };
        if rc != 0 {
            // SAFETY: `attrs` was initialised above.
            unsafe { libc::posix_spawnattr_destroy(&mut attrs) };
            return Err(SyscallError::new("posix_spawnattr_setflags", rc));
        }
    }
    let attr_ptr = &attrs as *const libc::posix_spawnattr_t;

    let mut file_actions: libc::posix_spawn_file_actions_t = unsafe { std::mem::zeroed() };
    let use_file_actions = working_directory.is_some() || options.stdio.is_some();
    if use_file_actions {
        // SAFETY: a zeroed file-actions struct is the documented starting point
        // for `posix_spawn_file_actions_init`.
        let rc = unsafe { libc::posix_spawn_file_actions_init(&mut file_actions) };
        if rc != 0 {
            // SAFETY: `attrs` was initialised above.
            unsafe { libc::posix_spawnattr_destroy(&mut attrs) };
            return Err(SyscallError::new("posix_spawn_file_actions_init", rc));
        }
        if let Some(cwd) = working_directory {
            let rc = unsafe {
                libc::posix_spawn_file_actions_addchdir_np(&mut file_actions, cwd.as_ptr())
            };
            if rc != 0 {
                return fail_file_actions(
                    &mut file_actions,
                    use_attrs,
                    &mut attrs,
                    "posix_spawn_file_actions_addchdir_np",
                    rc,
                );
            }
        }
        // `dup2` clears O_CLOEXEC on the *new* descriptor, so the child gets a
        // usable stdout/stderr while rinit's own copy of the pipe stays
        // close-on-exec.
        if let Some(fd) = options.stdio {
            for target in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
                let rc = unsafe {
                    libc::posix_spawn_file_actions_adddup2(&mut file_actions, fd, target)
                };
                if rc != 0 {
                    return fail_file_actions(
                        &mut file_actions,
                        use_attrs,
                        &mut attrs,
                        "posix_spawn_file_actions_adddup2",
                        rc,
                    );
                }
            }
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

/// Tear down the spawn attribute/action structs and report why.
///
/// Shared by every failure inside the file-actions setup, so a partial setup
/// never leaks an initialised `posix_spawn_file_actions_t`.
fn fail_file_actions(
    file_actions: &mut libc::posix_spawn_file_actions_t,
    use_attrs: bool,
    attrs: &mut libc::posix_spawnattr_t,
    op: &'static str,
    rc: libc::c_int,
) -> Result<libc::pid_t, SyscallError> {
    // SAFETY: both structs were initialised by the caller before any of the
    // calls that can land here.
    unsafe {
        libc::posix_spawn_file_actions_destroy(file_actions);
        if use_attrs {
            libc::posix_spawnattr_destroy(attrs);
        }
    }
    Err(SyscallError::new(op, rc))
}

/// Spawn `path` with a cgroup join and a privilege drop applied between `fork`
/// and `exec` (plan §7.3, §7.4).
///
/// `posix_spawn` cannot express this, and that is the whole reason the plan
/// routes any unit that needs it through a controlled `fork`+`exec` (§2.3):
/// there is no way to run `setuid`, `setgroups`, `prctl` or a write to the
/// unit's `cgroup.procs` in the child, after the `fork` and before the `exec`,
/// through the `posix_spawn` interface.
///
/// `cgroup_procs` is the path of the unit's `cgroup.procs` (`None` for a unit
/// with no limits), and the child joins it *before* the identity steps below -
/// see [`cgroup::join_from_child`] for why the child is the side that writes it.
///
/// rinit's async runtime is current-thread, but blocking-pool threads exist
/// (mounts, reload), so the child does not rely on the process being
/// single-threaded. It calls only async-signal-safe `libc`, allocates nothing
/// and takes no locks, which is the POSIX requirement for a child of a
/// multi-threaded process. Everything
/// the child needs - the argument vectors, the working directory, the sandbox
/// steps, the cgroup path - is built in the parent and lives in the address space
/// the child is sharing.
///
/// A step that fails in the child is reported back over a close-on-exec pipe,
/// so the caller gets a [`SyscallError`] naming the step (`setuid failed:
/// EPERM`) instead of a pid that dies a moment later for reasons nobody can
/// see. The child is reaped here in that case, so a failed spawn leaves no
/// zombie behind.
///
/// # Errors
/// `fork` itself failing; a `setsid`/`dup2`/`chdir` failure in the child; a
/// cgroup join or sandbox step failing in the child; `execve` failing (e.g.
/// `ENOENT` for a command that does not exist, which plan §1.1 contracts as a
/// start failure).
/// A forked child whose exec confirmation has not been read yet.
///
/// The fork and all child-side setup have already happened; only the report
/// pipe remains. `wait_blocking` preserves the old synchronous behaviour, while
/// the main start flow hands the wait to a blocking task so PID 1 keeps running.
pub struct PendingSpawn {
    pid: libc::pid_t,
    report: OwnedFd,
    completed: bool,
}

impl PendingSpawn {
    /// The child's pid, valid before the report is consumed.
    pub const fn pid(&self) -> libc::pid_t {
        self.pid
    }

    /// Release the child without reading its report.
    ///
    /// The child keeps running; a setup failure is observed as exit 127 by the
    /// normal reaper. Used for auxiliary commands whose exit status is already
    /// the only thing the caller consumes.
    pub fn detach(mut self) -> libc::pid_t {
        self.completed = true;
        self.pid
    }

    /// Kill the child and stop tracking it without waiting for the report.
    ///
    /// Used when the parent discovers that the unit disappeared between fork
    /// and registration. The child is left as a zombie for the global reaper;
    /// blocking here for up to five seconds would defeat the asynchronous
    /// spawn path.
    pub fn abandon(mut self) {
        kill_unreaped_child(self.pid);
        self.completed = true;
    }

    /// Wait for the report, killing and reaping the child if it never execs.
    ///
    /// `cancel` is checked between short poll slices; a flow that is cancelled
    /// while a child is stuck before `execve` must not leave that child behind.
    pub fn wait_blocking(mut self, cancel: &AtomicBool) -> Result<libc::pid_t, SyscallError> {
        let report = read_report(self.report.as_raw_fd(), cancel);
        self.completed = true;
        match report {
            Ok(ChildReport::Exec) => Ok(self.pid),
            Ok(ChildReport::Failed(step, errno)) => {
                reap_failed_spawn(self.pid);
                Err(SyscallError::new(step.op(), errno))
            }
            Ok(ChildReport::DiedBeforeExec) => {
                reap_failed_spawn(self.pid);
                Err(SyscallError::new(
                    "child setup (process died before exec)",
                    libc::EIO,
                ))
            }
            Err(err) => {
                // Timeout and cancellation both need the unconfirmed child
                // gone; `reap_failed_spawn` kills it if it is still alive.
                reap_failed_spawn(self.pid);
                Err(err)
            }
        }
    }
}

impl Drop for PendingSpawn {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        // Nobody consumed the confirmation: the safest outcome is that the
        // child never becomes an untracked service. The helper checks that the
        // pid is still our unreaped child before sending a signal, so a pid
        // already reaped by the global reaper cannot be reused and killed.
        reap_failed_spawn(self.pid);
    }
}

/// Fork and return the pending confirmation, without waiting for it.
pub fn spawn_sandboxed_begin(
    path: &CStr,
    argv: &[CString],
    envp: &[CString],
    options: SpawnOptions,
    working_directory: Option<&CStr>,
    sandbox: &Sandbox,
    paths: ChildPaths<'_>,
) -> Result<PendingSpawn, SyscallError> {
    // The child knows its pid only after fork, but `sd_listen_fds()` checks
    // `LISTEN_PID` against its own pid. Reserve an environment slot in the
    // parent; the child fills a private COW buffer with "LISTEN_PID=<pid>"
    // before exec. No allocation happens between fork and exec.
    let mut listen_pid_buf = [0u8; 32];
    let provides_listen_pid = options.listen_fd.is_some();
    let mut env_ptrs: Vec<*const libc::c_char> =
        Vec::with_capacity(envp.len() + 1 + usize::from(provides_listen_pid));
    env_ptrs.extend(
        envp.iter()
            // Only replace `LISTEN_PID` when this child is actually being handed a
            // socket. A service that sets the variable for its own reasons must
            // keep it on the ordinary path.
            .filter(|entry| !provides_listen_pid || !entry.to_bytes().starts_with(b"LISTEN_PID="))
            .map(|entry| entry.as_ptr()),
    );
    let listen_pid = if provides_listen_pid {
        let pointer = listen_pid_buf.as_mut_ptr().cast::<libc::c_char>();
        env_ptrs.push(pointer);
        Some(pointer.cast::<u8>())
    } else {
        None
    };
    env_ptrs.push(std::ptr::null());

    let setup = ChildSetup {
        path,
        argv: null_terminated(argv),
        envp: env_ptrs,
        options,
        working_directory,
        sandbox,
        steps: sandbox.steps(),
        paths,
        listen_pid,
    };

    // Close-on-exec, which is what makes the marker plus EOF mean "the child
    // reached `execve`": the child's copy of the write end disappears on a
    // successful exec, and a failure can still be reported through it.
    let (report_read, report_write) = fs::pipe()?;

    // SAFETY: `fork` takes no arguments. The child only calls `libc` directly
    // (see `child_exec`) and never returns, so no destructor of this frame runs
    // in it.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(SyscallError::last_os_error("fork"));
    }
    if pid == 0 {
        child_exec(&setup, report_read.as_raw_fd(), report_write.as_raw_fd());
    }

    // The parent's copy of the write end has to go, or the report read would
    // wait for a copy that never closes.
    drop(report_write);
    Ok(PendingSpawn {
        pid,
        report: report_read,
        completed: false,
    })
}

/// Spawn and wait for the exec confirmation.
///
/// Kept for callers that cannot yield (tests, auxiliary commands). The main
/// service start uses [`spawn_sandboxed_begin`] and waits off the event loop.
pub fn spawn_sandboxed(
    path: &CStr,
    argv: &[CString],
    envp: &[CString],
    options: SpawnOptions,
    working_directory: Option<&CStr>,
    sandbox: &Sandbox,
    paths: ChildPaths<'_>,
) -> Result<libc::pid_t, SyscallError> {
    let cancel = AtomicBool::new(false);
    spawn_sandboxed_begin(path, argv, envp, options, working_directory, sandbox, paths)?
        .wait_blocking(&cancel)
}

/// Everything the work of a child needs, prepared in the parent before the
/// `fork`.
///
/// The child allocates nothing, so the argument vectors and the pointer arrays
/// built from them have to exist before it starts: this struct is that, and it
/// lives in the stack frame the child borrows a copy of.
struct ChildSetup<'a> {
    path: &'a CStr,
    argv: Vec<*const libc::c_char>,
    envp: Vec<*const libc::c_char>,
    options: SpawnOptions,
    working_directory: Option<&'a CStr>,
    sandbox: &'a Sandbox,
    /// The privilege-drop steps, already ordered. `Sandbox::steps` builds a
    /// `Vec`; doing that in the child would be an allocation between `fork` and
    /// `exec`, which is exactly what this struct exists to avoid.
    steps: Vec<SandboxStep>,
    /// The files the child opens and acts on after the fork (plan §7.3, §7.11).
    paths: ChildPaths<'a>,
    /// A parent-owned buffer the child fills with `LISTEN_PID=<pid>` before
    /// exec, when it was given a listening socket.
    listen_pid: Option<*mut u8>,
}

/// Reset signal dispositions and the signal mask for a child that is about to
/// `exec`.
///
/// `SIG_IGN` is inherited across `exec`, while handled signals are reset by the
/// kernel. rinit ignores several signals for its own lifecycle, so a child must
/// start from `SIG_DFL` or a service would silently inherit an init's matrix.
/// Only async-signal-safe libc calls are used.
fn reset_child_signals() {
    // SAFETY: `action` is zeroed then filled with valid fields; `sigemptyset`
    // initialises the mask.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = libc::SIG_DFL;
    action.sa_flags = 0;
    // SAFETY: `action.sa_mask` is valid storage for `sigemptyset`.
    unsafe { libc::sigemptyset(&mut action.sa_mask) };
    for signal in 1..=64 {
        if signal == libc::SIGKILL || signal == libc::SIGSTOP {
            continue;
        }
        // SAFETY: `action` is initialized; an invalid signal number is simply
        // ignored, and SIGKILL/SIGSTOP are never passed.
        unsafe {
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }

    // SAFETY: a zeroed signal set is initialised by `sigemptyset`; passing it
    // as SIG_SETMASK with no old-set pointer is the documented form.
    let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut mask);
        libc::sigprocmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
    }
}

/// Fill `buffer` with `LISTEN_PID=<getpid()>` without allocating.
///
/// The buffer is reserved in the parent and its pointer is part of the child's
/// environment vector; after `fork` the child owns a private copy, so writing
/// it cannot affect the parent.
fn write_listen_pid(buffer: *mut u8) {
    const PREFIX: &[u8] = b"LISTEN_PID=";
    // SAFETY: `getpid` has no preconditions.
    let mut pid = unsafe { libc::getpid() } as u64;
    let mut digits = [0u8; 20];
    let mut count = 0usize;
    for slot in &mut digits {
        *slot = b'0' + (pid % 10) as u8;
        count += 1;
        pid /= 10;
        if pid == 0 {
            break;
        }
    }
    // SAFETY: the caller reserved at least PREFIX + 20 digits + NUL bytes in
    // `buffer`, and the writes stay inside that reservation.
    unsafe {
        std::ptr::copy_nonoverlapping(PREFIX.as_ptr(), buffer, PREFIX.len());
        let mut offset = PREFIX.len();
        for digit in digits.iter().take(count).rev() {
            *buffer.add(offset) = *digit;
            offset += 1;
        }
        *buffer.add(offset) = 0;
    }
}

/// The work of a child created by [`spawn_sandboxed`], which never returns.
///
/// Every failure ends in `_exit(127)` (plan §1.1's contract for a command that
/// could not be executed) after reporting the failing step to the parent.
fn child_exec(setup: &ChildSetup<'_>, report_read: libc::c_int, report_write: libc::c_int) -> ! {
    // Only the write end has anything to do in the child; holding the read end
    // open would not hurt, but there is no reason to.
    // SAFETY: both descriptors are valid, and this one is not used again.
    unsafe { libc::close(report_read) };

    // `SIG_IGN` survives exec. rinit ignores SIGPIPE/SIGHUP/SIGWINCH and
    // friends, so without this every service would inherit an init's signal
    // matrix instead of the one its shell or runtime expects. Handlers are
    // reset by exec itself, but the ignored set is not; resetting every
    // disposition also closes the fork-to-exec window where a service-directed
    // signal would run rinit's handler.
    reset_child_signals();
    if let Some(buffer) = setup.listen_pid {
        write_listen_pid(buffer);
    }

    if setup.options.new_session {
        // SAFETY: `setsid` takes no arguments. The child is not a process-group
        // leader (its group is the parent's), so this cannot fail with `EPERM`.
        if unsafe { libc::setsid() } < 0 {
            die(report_write, ChildStep::Setsid);
        }
    }

    // Plan §7.9: the listening socket, on the number socket-activated services
    // look at. Before the identity steps below, both because `dup2` is what clears
    // the flag that would otherwise close it at `exec`, and because a unit that
    // drops privileges may not be able to do it afterwards.
    if let Some(fd) = setup.options.listen_fd {
        if fd == LISTEN_FD {
            // `dup2(fd, fd)` is a no-op and would leave `FD_CLOEXEC` set on the
            // descriptor, so the service would lose it at exec. Clear the flag
            // explicitly in that one case.
            // SAFETY: `fd` is the listening socket the parent opened; the
            // third argument is a plain flag value.
            if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } < 0 {
                die(report_write, ChildStep::Dup2);
            }
        } else {
            // SAFETY: `fd` is a listening socket the parent opened and holds open
            // for the child's sake; `dup2` closes the target first and cannot
            // fail here except out of descriptors.
            if unsafe { libc::dup2(fd, LISTEN_FD) } < 0 {
                die(report_write, ChildStep::Dup2);
            }
        }
        // systemd passes the listening socket to the service in blocking mode.
        // The parent's watcher only needs readiness, so clearing O_NONBLOCK on
        // the shared open-file description is safe and keeps a normal
        // `accept()` loop from failing with EAGAIN after the first connection.
        let flags = unsafe { libc::fcntl(LISTEN_FD, libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(LISTEN_FD, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0
        {
            die(report_write, ChildStep::Dup2);
        }
    }

    if let Some(fd) = setup.options.stdio {
        // `dup2` clears `O_CLOEXEC` on the new descriptor, so the child keeps a
        // usable stdout/stderr while rinit's own copy stays close-on-exec.
        for target in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
            // SAFETY: both descriptors are valid; `dup2` closes `target` first.
            if unsafe { libc::dup2(fd, target) } < 0 {
                die(report_write, ChildStep::Dup2);
            }
        }
    }

    // Plan §7.11: a controlling terminal, if the unit asked for one. After
    // `setsid` - only a session leader with no controlling terminal may take
    // one - and after the `stdio` redirection above, which this deliberately
    // overrides: a service with a `TTYPath=` talks to the terminal, not to the
    // log ring. Before the identity steps below, which need root to open a
    // device node.
    if let Some(tty) = &setup.paths.tty {
        setup_tty(tty, report_write);
    }

    if let Some(directory) = setup.working_directory {
        // Before the drop, deliberately: a directory the service's own user
        // cannot enter is still the directory it was configured to start in,
        // and plan §7.4 only requires that failing to get there is fatal.
        // SAFETY: `directory` is a valid, NUL-terminated path.
        if unsafe { libc::chdir(directory.as_ptr()) } < 0 {
            die(report_write, ChildStep::Chdir);
        }
    }

    // Plan §7.3: join the unit's cgroup first, and before the identity steps
    // below in particular - `cgroup.procs` is root-owned, so a child that had
    // already become the service's user could no longer write it. Doing it here
    // is also what makes the placement exact: nothing the service does (and
    // nothing it forks) can happen before this, since the child does not reach
    // `execve` until this returns.
    if let Some(procs) = setup.paths.cgroup_procs {
        // SAFETY: `getpid` takes no arguments and never fails.
        let pid = unsafe { libc::getpid() };
        if let Err(errno) = crate::syscall::cgroup::join_from_child(procs, pid) {
            die_reporting(report_write, ChildStep::JoinCgroup, errno);
        }
    }

    // Plan §7.4's order, from the plan that `rinit_core::unit::sandbox`
    // produced: the steps are already in it.
    for &step in &setup.steps {
        let outcome = match step {
            SandboxStep::NoNewPrivileges => cred::set_no_new_privs(),
            SandboxStep::KeepCapabilities => cred::keep_capabilities(),
            // `steps` only emits these when the field behind them is `Some`, and
            // the fallback is the strict reading on purpose: a capability
            // request that somehow went missing drops capabilities rather than
            // keeping them.
            SandboxStep::SetInheritable => {
                cred::set_inheritable(setup.sandbox.ambient.as_deref().unwrap_or(&[]))
            }
            SandboxStep::SetBoundingSet => {
                cred::set_bounding_set(setup.sandbox.bounding_set.as_deref().unwrap_or(&[]))
            }
            SandboxStep::SetGroup => cred::set_gid(setup.sandbox.gid),
            SandboxStep::SetGroups => {
                cred::set_groups(setup.sandbox.groups.as_deref().unwrap_or(&[]))
            }
            SandboxStep::SetUser => cred::set_uid(setup.sandbox.uid),
            SandboxStep::RaiseAmbient => {
                cred::set_ambient(setup.sandbox.ambient.as_deref().unwrap_or(&[]))
            }
        };
        if let Err(err) = outcome {
            die_reporting_step(report_write, step, err.errno());
        }
    }

    // All child-side setup succeeded. Tell the parent that the next call is
    // `execve`, so a later EOF can be told apart from a death during setup.
    // SAFETY: the report pipe is the parent/child protocol, and one byte is an
    // atomic pipe write.
    write_report(report_write, &[READY_BYTE]);

    // SAFETY: `path` and both arrays are valid, NUL-terminated, and live in the
    // memory this child inherited from the parent. `execve` does not return on
    // success.
    unsafe {
        libc::execve(
            setup.path.as_ptr(),
            setup.argv.as_ptr(),
            setup.envp.as_ptr(),
        )
    };
    die(report_write, ChildStep::Exec);
}

/// Open `tty.path` and make it the child's controlling terminal.
///
/// Never returns on failure: a `TTYPath=` that cannot be taken over is a failed
/// start, because a `getty`-like service with no controlling terminal cannot run
/// a login shell at all - and finding that out later, from a service that started
/// and misbehaved, is worse than a named failure here.
///
/// The reset is best effort, deliberately: it is a virtual hangup, and a device
/// that does not implement it still works as a terminal. Defaulting `TTYReset=`
/// to on must not be able to fail a start on its own.
fn setup_tty(tty: &ChildTty<'_>, report_write: libc::c_int) {
    // `O_NOCTTY` so the open itself does not acquire the terminal implicitly:
    // taking it over is the explicit `TIOCSCTTY` below, which is also what
    // makes "this failed" and "this was never attempted" tell apart.
    //
    // `O_NONBLOCK` matters for a serial console with no carrier: a blocking
    // `open` can wait indefinitely, and this call happens between `fork` and
    // `exec` while PID 1 is synchronously waiting for the spawn report. The
    // flag is cleared on the open file description below, so the service gets
    // the blocking terminal it expects.
    // SAFETY: `path` is a valid, NUL-terminated path.
    let fd = unsafe {
        libc::open(
            tty.path.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        die(report_write, ChildStep::OpenTty);
    }
    if tty.reset {
        // SAFETY: `fd` is a terminal descriptor we just opened, and this ioctl
        // takes no argument.
        unsafe { libc::ioctl(fd, ioctl_request(TIOCVHANGUP)) };
    }
    // SAFETY: as above; `TIOCSCTTY` takes an `int` argument, and this kernel
    // accepts 0 for "take it over unconditionally".
    if unsafe { libc::ioctl(fd, ioctl_request(TIOCSCTTY), 0) } < 0 {
        die(report_write, ChildStep::SetControllingTerminal);
    }
    for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        // SAFETY: both descriptors are valid; `dup2` closes `target` first.
        if unsafe { libc::dup2(fd, target) } < 0 {
            die(report_write, ChildStep::Dup2);
        }
    }
    // The duplicated descriptors share the open file description, so the flag
    // change applies to every standard descriptor. Done on `fd` even when it is
    // 0/1/2 (the child inherited a closed standard descriptor, so `open`
    // returned one of those numbers).
    // SAFETY: `fd` is the descriptor opened above.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        die(report_write, ChildStep::OpenTty);
    }
    // SAFETY: as above; clearing a status flag on our own descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        die(report_write, ChildStep::OpenTty);
    }
    if fd > libc::STDERR_FILENO {
        // SAFETY: `fd` is ours and no longer needed: it is 0, 1 and 2 now.
        unsafe { libc::close(fd) };
    }
}

/// Write the child report without letting a detached parent's closed pipe kill
/// the child with SIGPIPE.
///
/// Blocking SIGPIPE and then restoring the mask is not enough: a failed write
/// leaves SIGPIPE pending, and restoring the mask delivers it immediately. The
/// child would then die before `execve` even though the report failure is
/// expected when `PendingSpawn::detach` has already closed the read end. Ignore
/// the signal for the duration of the write and restore the previous action
/// afterwards; ignored signals are never made pending.
fn write_report(fd: libc::c_int, bytes: &[u8]) {
    // SAFETY: `action` and `previous` are valid storage for sigaction, and the
    // signal mask is untouched. SIGPIPE has no handler state to preserve.
    unsafe {
        let mut action = std::mem::zeroed::<libc::sigaction>();
        action.sa_sigaction = libc::SIG_IGN;
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        let mut previous = std::mem::zeroed::<libc::sigaction>();
        libc::sigaction(libc::SIGPIPE, &action, &mut previous);
        libc::write(fd, bytes.as_ptr().cast(), bytes.len());
        libc::sigaction(libc::SIGPIPE, &previous, std::ptr::null_mut());
    }
}

/// Report the current `errno` for `step` and exit with 127.
fn die(report_write: libc::c_int, step: ChildStep) -> ! {
    // SAFETY: nothing was called since the failing call but `errno` readers.
    let errno = std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO);
    die_reporting(report_write, step, errno)
}

/// Report an explicit `errno` for `step` and exit with 127.
fn die_reporting(report_write: libc::c_int, step: ChildStep, errno: i32) -> ! {
    let report = u64::from(step as u8) | (u64::from(errno as u32) << 8);
    let bytes = report.to_le_bytes();
    // SAFETY: `bytes` is a valid 8-byte buffer. A pipe write of this size is
    // atomic and cannot come up short; if it fails (the parent is gone), the
    // `_exit` below still ends the child, and the parent sees end-of-file.
    write_report(report_write, &bytes);
    // SAFETY: `_exit` never returns and runs no destructors and no `atexit`
    // handlers - which is exactly what a process that exists only between
    // `fork` and `exec` needs: it must not touch what it shares with its parent
    // beyond what it already did.
    unsafe { libc::_exit(127) }
}

/// The byte the child writes immediately before `execve`.
///
/// It is outside the [`ChildStep`] code range, so the first byte of the pipe
/// unambiguously says whether the child finished setup.
const READY_BYTE: u8 = 0xff;

/// What happened to a child between `fork` and `exec`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildReport {
    /// The child wrote [`READY_BYTE`], then closed the pipe: it is now running
    /// the command.
    Exec,
    /// A named setup step failed; the child reported its `errno` and exited.
    Failed(ChildStep, i32),
    /// The pipe closed before the ready marker: the child died during setup.
    DiedBeforeExec,
}

/// Read a child's report from the close-on-exec pipe.
///
/// The protocol is: a failed setup step writes an 8-byte `(step, errno)`
/// report; a completed setup writes [`READY_BYTE`] and then either `execve`s
/// (closing the pipe) or writes its `execve` failure report. A pipe that closes
/// before the marker therefore means the child died during setup, which is a
/// failed start rather than a successful one.
///
/// The remaining, unavoidable race is a signal delivered between the marker
/// write and `execve`: EOF can only prove setup completed, not that the image
/// was loaded. Closing that window needs an exec event (`ptrace`) or a helper
/// process, which is not worth the cost for an init that already owns the
/// status of its children.
fn read_report(fd: libc::c_int, cancel: &AtomicBool) -> Result<ChildReport, SyscallError> {
    read_report_with_deadline(fd, cancel, Instant::now() + Duration::from_secs(5))
}

/// The deadline is explicit so tests can exercise the timeout path without
/// waiting for the production five-second budget.
fn read_report_with_deadline(
    fd: libc::c_int,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<ChildReport, SyscallError> {
    let mut first = [0u8; 1];
    if read_until(fd, &mut first, deadline, "child report", cancel)? == 0 {
        return Ok(ChildReport::DiedBeforeExec);
    }
    if first[0] == READY_BYTE {
        let mut buf = [0u8; REPORT_LEN];
        let filled = read_until(fd, &mut buf, deadline, "child report", cancel)?;
        if filled == 0 {
            return Ok(ChildReport::Exec);
        }
        // A complete report after the marker is an `execve` failure; a short
        // one means the child died while reporting it.
        return match decode_report(&buf) {
            Some((step, errno)) if filled == REPORT_LEN => Ok(ChildReport::Failed(step, errno)),
            _ => Ok(ChildReport::DiedBeforeExec),
        };
    }

    let mut buf = [0u8; REPORT_LEN];
    buf[0] = first[0];
    let Some(rest) = buf.get_mut(1..) else {
        return Ok(ChildReport::DiedBeforeExec);
    };
    let filled = read_until(fd, rest, deadline, "child report", cancel)?;
    if filled < REPORT_LEN - 1 {
        return Ok(ChildReport::DiedBeforeExec);
    }
    match decode_report(&buf) {
        Some((step, errno)) => Ok(ChildReport::Failed(step, errno)),
        None => Ok(ChildReport::DiedBeforeExec),
    }
}

/// Read up to `buf.len()` bytes, returning how many arrived before EOF.
///
/// A timeout or a read error is reported; EOF is a normal return with whatever
/// was read, because a child that dies mid-report is a distinct outcome from
/// one that died before writing anything.
fn read_until(
    fd: libc::c_int,
    buf: &mut [u8],
    deadline: Instant,
    op: &'static str,
    cancel: &AtomicBool,
) -> Result<usize, SyscallError> {
    let mut filled = 0usize;
    while filled < buf.len() {
        if cancel.load(Ordering::Relaxed) {
            return Err(SyscallError::new(op, libc::ECANCELED));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(SyscallError::new(op, libc::ETIMEDOUT));
        }
        // Poll in short slices so a cancellation is noticed while a child is
        // stuck in setup, rather than only at the five-second deadline.
        let slice = remaining.min(Duration::from_millis(10));
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        // SAFETY: `descriptor` is one valid pollfd for the child report pipe.
        let ready = unsafe {
            libc::poll(
                &mut descriptor,
                1,
                slice.as_millis().min(i32::MAX as u128) as i32,
            )
        };
        if ready < 0 {
            let error = SyscallError::last_os_error("poll");
            if error.errno() == libc::EINTR {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            continue;
        }
        let Some(destination) = buf.get_mut(filled..) else {
            break;
        };
        // SAFETY: `destination` is a valid writable slice of the given length.
        let rc = unsafe { libc::read(fd, destination.as_mut_ptr().cast(), destination.len()) };
        if rc < 0 {
            let err = SyscallError::last_os_error("read");
            if err.errno() == libc::EINTR {
                continue;
            }
            return Err(err);
        }
        if rc == 0 {
            return Ok(filled);
        }
        filled += rc as usize;
    }
    Ok(filled)
}

/// Decode an 8-byte `(step, errno)` report.
fn decode_report(buf: &[u8; REPORT_LEN]) -> Option<(ChildStep, i32)> {
    let report = u64::from_le_bytes(*buf);
    let code = (report & 0xff) as u8;
    let errno = ((report >> 8) & 0xffff_ffff) as u32 as i32;
    // An unknown code cannot come from this build's child, so it is a bug worth
    // reporting rather than labelling `execve`: the previous `_ => Exec`
    // fallback did exactly that, and every step added after it silently
    // reported itself as a failed `exec`.
    ChildStep::from_code(code).map(|step| (step, errno))
}

/// Kill a child only if `waitpid` still identifies it as our unreaped child.
///
/// `abandon` leaves reaping to the main loop; it must not signal a pid that a
/// concurrent reaper has already reaped and the kernel has reused.
fn kill_unreaped_child(pid: libc::pid_t) {
    let mut status = 0;
    // SAFETY: `status` is valid storage; WNOHANG keeps this non-blocking.
    let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if rc == 0 {
        // SAFETY: waitpid(WNOHANG) returned 0, so this is still our child.
        let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

/// Reap a child that failed before `exec`.
///
/// It has already `_exit`ed, so this is not a wait in any meaningful sense; the
/// point is that the caller's error does not leave a corpse behind for the main
/// loop's reaper to find later.
fn reap_failed_spawn(pid: libc::pid_t) {
    let mut status = 0;
    // If it is still alive (for example, stuck before exec), kill it. If it has
    // already exited, waitpid reaps it without ever sending a signal to a pid
    // that might have been reused.
    // SAFETY: `status` is valid storage and `pid` is the child we forked.
    let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if rc == pid || (rc < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
    {
        return;
    }
    if rc == 0 {
        // SAFETY: this is our unreaped child (waitpid said it is still alive).
        let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        // SAFETY: `status` is valid storage and `pid` is our child.
        let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if rc == pid
            || (rc < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
        {
            return;
        }
        if Instant::now() >= deadline {
            // The main loop will reap it if it ever leaves the kernel.
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The NULL-terminated `char *` array `execve` wants.
///
/// Built in the parent on purpose: between `fork` and `exec` the child
/// allocates nothing, so every pointer it uses must already be in the address
/// space it inherited.
fn null_terminated(strings: &[CString]) -> Vec<*const libc::c_char> {
    let mut pointers: Vec<*const libc::c_char> = Vec::with_capacity(strings.len() + 1);
    pointers.extend(strings.iter().map(|s| s.as_ptr()));
    pointers.push(std::ptr::null());
    pointers
}

/// The number of bytes in a failure report: a step code and an `errno`.
const REPORT_LEN: usize = 8;

/// What a child was doing when it failed, as sent to the parent.
///
/// The discriminants travel over the report pipe, so they are part of the
/// parent/child wire and must not be renumbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ChildStep {
    Chdir = 0,
    Setsid = 1,
    Dup2 = 2,
    Exec = 3,
    NoNewPrivileges = 4,
    SetGroup = 5,
    SetGroups = 6,
    SetUser = 7,
    KeepCapabilities = 8,
    SetInheritable = 9,
    SetBoundingSet = 10,
    RaiseAmbient = 11,
    /// Plan §7.3's placement step: the child writes its own pid into the unit's
    /// `cgroup.procs`.
    JoinCgroup = 12,
    /// Plan §7.11: opening the unit's `TTYPath=`.
    OpenTty = 13,
    /// Plan §7.11: `ioctl(TIOCSCTTY)` on it.
    SetControllingTerminal = 14,
}

impl ChildStep {
    /// The `op` name the parent's [`SyscallError`] carries.
    const fn op(self) -> &'static str {
        match self {
            ChildStep::Chdir => "chdir",
            ChildStep::Setsid => "setsid",
            ChildStep::Dup2 => "dup2",
            ChildStep::Exec => "execve",
            ChildStep::NoNewPrivileges => "prctl(PR_SET_NO_NEW_PRIVS)",
            ChildStep::SetGroup => "setgid",
            ChildStep::SetGroups => "setgroups",
            ChildStep::SetUser => "setuid",
            ChildStep::KeepCapabilities => "prctl(PR_SET_KEEPCAPS)",
            ChildStep::SetInheritable => "capset",
            ChildStep::SetBoundingSet => "prctl(PR_CAPBSET_DROP)",
            ChildStep::RaiseAmbient => "prctl(PR_CAP_AMBIENT)",
            ChildStep::JoinCgroup => "cgroup.procs",
            ChildStep::OpenTty => "open(TTYPath)",
            ChildStep::SetControllingTerminal => "ioctl(TIOCSCTTY)",
        }
    }

    /// The step a report's code names, in the plan's terms.
    const fn from_step(step: SandboxStep) -> Self {
        match step {
            SandboxStep::NoNewPrivileges => ChildStep::NoNewPrivileges,
            SandboxStep::KeepCapabilities => ChildStep::KeepCapabilities,
            SandboxStep::SetInheritable => ChildStep::SetInheritable,
            SandboxStep::SetBoundingSet => ChildStep::SetBoundingSet,
            SandboxStep::SetGroup => ChildStep::SetGroup,
            SandboxStep::SetGroups => ChildStep::SetGroups,
            SandboxStep::SetUser => ChildStep::SetUser,
            SandboxStep::RaiseAmbient => ChildStep::RaiseAmbient,
        }
    }

    /// The step a report code names, or `Exec` for a code this build does not
    /// know - which can only be a child from a different build, and is best
    /// reported as a command that did not start.
    const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(ChildStep::Chdir),
            1 => Some(ChildStep::Setsid),
            2 => Some(ChildStep::Dup2),
            // A failed `execve` *is* reported; a successful one is the child
            // closing the pipe, which is why the parent treats end-of-file as
            // "the child exec'd".
            3 => Some(ChildStep::Exec),
            4 => Some(ChildStep::NoNewPrivileges),
            5 => Some(ChildStep::SetGroup),
            6 => Some(ChildStep::SetGroups),
            7 => Some(ChildStep::SetUser),
            8 => Some(ChildStep::KeepCapabilities),
            9 => Some(ChildStep::SetInheritable),
            10 => Some(ChildStep::SetBoundingSet),
            11 => Some(ChildStep::RaiseAmbient),
            12 => Some(ChildStep::JoinCgroup),
            13 => Some(ChildStep::OpenTty),
            14 => Some(ChildStep::SetControllingTerminal),
            _ => None,
        }
    }
}

/// Bring the failure of a *sandbox step* back to [`ChildStep`].
///
/// The two enums exist for different reasons - one is the plan's vocabulary,
/// the other is the wire format - and this is the one place they meet.
fn die_reporting_step(report_write: libc::c_int, step: SandboxStep, errno: i32) -> ! {
    die_reporting(report_write, ChildStep::from_step(step), errno)
}

/// Send `signal` to `pid` (a positive PID, not a process group).
///
/// This is also the fallback for a process that is *not* a group leader: a
/// `Type=forking` daemon (plan §7.10) inherits its process group from whatever
/// forked it, so [`kill_process_group`] can address a group that no longer
/// exists while the daemon itself is very much alive.
pub fn kill(pid: libc::pid_t, signal: libc::c_int) -> Result<(), SyscallError> {
    if pid <= 1 {
        return Err(SyscallError::new("kill", libc::EINVAL));
    }
    // SAFETY: `kill` has no memory-safety preconditions for a scalar pid/signal.
    let rc = unsafe { libc::kill(pid, signal) };
    if rc == 0 {
        Ok(())
    } else {
        Err(SyscallError::last_os_error("kill"))
    }
}

/// Send `signal` to every process in `pid`'s process group.
/// `pid` is expected to lead its own process group, as created by
/// [`SpawnOptions::with_session`]. This is how a stop reaches services
/// that fork their own children (plan §1.3(6)): signalling the group delivers
/// to all of them, where `kill(pid, ...)` would only reach the direct child.
///
/// # Errors
/// Returns `ESRCH` if the group is already empty, which is the normal outcome
/// when the service exited between the check and the signal.
pub fn kill_process_group(pid: libc::pid_t, signal: libc::c_int) -> Result<(), SyscallError> {
    if pid <= 1 {
        return Err(SyscallError::new("killpg", libc::EINVAL));
    }
    // A negative pid addresses a group. Only use it when the pid really leads
    // its group; a forking daemon may have inherited another group, and
    // `kill(-pid)` would then address the wrong processes. Fall back to the
    // single pid, which is the unit's own daemon.
    // SAFETY: `getpgid` only reads process metadata.
    let group = unsafe { libc::getpgid(pid) };
    if group < 0 {
        return Err(SyscallError::last_os_error("getpgid"));
    }
    let target = if group == pid { -pid } else { pid };
    // SAFETY: `target` is a pid or a process-group id; no memory-safety
    // preconditions apply to scalar arguments.
    let rc = unsafe { libc::kill(target, signal) };
    if rc == 0 {
        Ok(())
    } else {
        Err(SyscallError::last_os_error("killpg"))
    }
}

/// Signal a process group by its id.
///
/// Unlike [`kill_process_group`], this does not ask the kernel for the group of
/// a live member: the caller recorded the group while its leader existed. That
/// matters after the leader has been reaped - the group can still hold the
/// service's children, and `getpgid(leader)` would then fail with `ESRCH`.
///
/// A process-group id cannot be reused while any process still belongs to the
/// group, so a recorded id is safe to signal until it is observed empty.
pub fn kill_group(pgid: libc::pid_t, signal: libc::c_int) -> Result<(), SyscallError> {
    if pgid <= 1 {
        return Err(SyscallError::new("killpg", libc::EINVAL));
    }
    // SAFETY: a negative pid addresses the process group; scalar arguments.
    let rc = unsafe { libc::kill(-pgid, signal) };
    if rc == 0 {
        Ok(())
    } else {
        Err(SyscallError::last_os_error("killpg"))
    }
}

/// Whether any process still belongs to `pgid`.
pub fn group_alive(pgid: libc::pid_t) -> bool {
    if pgid <= 1 {
        return false;
    }
    // SAFETY: signal 0 performs the permission/existence check and delivers
    // nothing.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

/// Whether any process other than `except` belongs to `pgid`.
///
/// Used after a service's leader has been reaped: the leader may still be a
/// zombie at this point and therefore still counted by [`group_alive`], while
/// the question is whether *children* remain. `/proc/<pid>/stat` field 5 is the
/// process group; `comm` can contain spaces and parentheses, so the fields are
/// read after the last `)`.
pub fn group_has_other_members(pgid: libc::pid_t, except: libc::pid_t) -> bool {
    if pgid <= 1 {
        return false;
    }
    // A group with no members is the common case after the leader exits.
    // `kill(-pgid, 0)` answers that with one syscall; only ESRCH proves the
    // whole group is gone, so EPERM and other errors still take the /proc
    // scan below.
    // SAFETY: a negative pid with signal 0 only probes the process group.
    if unsafe { libc::kill(-pgid, 0) } != 0
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    {
        return false;
    }
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        if pid == except {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let mut fields = stat.get(close + 1..).unwrap_or_default().split_whitespace();
        let _state = fields.next();
        let _ppid = fields.next();
        if fields
            .next()
            .and_then(|group| group.parse::<libc::pid_t>().ok())
            == Some(pgid)
        {
            return true;
        }
    }
    false
}

/// Return the current process ID.
pub fn getpid() -> libc::pid_t {
    // SAFETY: `getpid` takes no arguments and cannot fail.
    unsafe { libc::getpid() }
}

/// Return the effective user ID.
///
/// Used to tell who "the owner" of the control socket is when rinit runs
/// without privileges (`--dev`), where root is not the answer.
pub fn geteuid() -> u32 {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    unsafe { libc::geteuid() }
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

#[cfg(test)]
mod report_tests {
    use super::*;

    /// Write `bytes` to a fresh pipe and return the read end with the write end
    /// already closed, which is what the child's exit does.
    fn pipe_with(bytes: &[u8]) -> std::os::fd::OwnedFd {
        let (read_end, write_end) = fs::pipe().expect("pipe");
        if !bytes.is_empty() {
            // SAFETY: `write_end` is a valid descriptor and `bytes` is a valid
            // buffer of the given length.
            let rc = unsafe {
                libc::write(
                    write_end.as_raw_fd(),
                    bytes.as_ptr().cast::<libc::c_void>(),
                    bytes.len(),
                )
            };
            assert_eq!(rc, bytes.len() as isize, "test pipe write");
        }
        drop(write_end);
        read_end
    }

    fn report_bytes(step: ChildStep, errno: i32) -> [u8; REPORT_LEN] {
        let report = u64::from(step as u8) | (u64::from(errno as u32) << 8);
        report.to_le_bytes()
    }

    #[test]
    fn the_ready_marker_is_not_a_step_code() {
        assert!(ChildStep::from_code(READY_BYTE).is_none());
    }

    /// Closing the pipe before the marker means the child never reached
    /// `execve`; treating that as success is exactly the bug the marker fixes.
    #[test]
    fn eof_before_the_marker_is_a_setup_death() {
        let read_end = pipe_with(&[]);
        assert_eq!(
            read_report(read_end.as_raw_fd(), &AtomicBool::new(false)).expect("read"),
            ChildReport::DiedBeforeExec
        );
    }

    /// The marker followed by EOF is the success case.
    #[test]
    fn marker_then_eof_is_exec() {
        let read_end = pipe_with(&[READY_BYTE]);
        assert_eq!(
            read_report(read_end.as_raw_fd(), &AtomicBool::new(false)).expect("read"),
            ChildReport::Exec
        );
    }

    /// A failed setup step still reports its name and errno.
    #[test]
    fn a_failure_report_names_the_step() {
        let bytes = report_bytes(ChildStep::Chdir, libc::EACCES);
        let read_end = pipe_with(&bytes);
        assert_eq!(
            read_report(read_end.as_raw_fd(), &AtomicBool::new(false)).expect("read"),
            ChildReport::Failed(ChildStep::Chdir, libc::EACCES)
        );
    }

    /// `execve` itself can fail after the marker; its report must win over the
    /// earlier success signal.
    #[test]
    fn a_failure_report_after_the_marker_is_a_failed_exec() {
        let report = report_bytes(ChildStep::Exec, libc::ENOENT);
        let mut bytes = Vec::new();
        bytes.push(READY_BYTE);
        bytes.extend_from_slice(&report);
        let read_end = pipe_with(&bytes);
        assert_eq!(
            read_report(read_end.as_raw_fd(), &AtomicBool::new(false)).expect("read"),
            ChildReport::Failed(ChildStep::Exec, libc::ENOENT)
        );
    }

    /// A signal between the marker and the rest of a report is a death, not a
    /// success and not a fabricated step.
    #[test]
    fn a_short_report_is_a_setup_death() {
        let read_end = pipe_with(&[READY_BYTE, 0, 1]);
        assert_eq!(
            read_report(read_end.as_raw_fd(), &AtomicBool::new(false)).expect("read"),
            ChildReport::DiedBeforeExec
        );
        let bytes = report_bytes(ChildStep::SetUser, libc::EPERM);
        let read_end = pipe_with(&bytes[..3]);
        assert_eq!(
            read_report(read_end.as_raw_fd(), &AtomicBool::new(false)).expect("read"),
            ChildReport::DiedBeforeExec
        );
    }

    /// Cancellation must be observed between poll slices, not only at the
    /// five-second deadline: a stop that arrives while a child is stuck before
    /// `execve` has to release the flow promptly.
    #[test]
    fn a_cancelled_report_stops_without_waiting_for_the_deadline() {
        let read_end = pipe_with(&[]);
        let error = read_report_with_deadline(
            read_end.as_raw_fd(),
            &AtomicBool::new(true),
            Instant::now() + Duration::from_secs(5),
        )
        .expect_err("a cancelled read must not succeed");
        assert_eq!(error.errno(), libc::ECANCELED);
    }

    /// A report that never arrives is a timeout, not a quiet success or a
    /// process that stays stuck forever.
    #[test]
    fn a_report_without_data_times_out_at_its_deadline() {
        let (read_end, _write_end) = fs::pipe().expect("pipe");
        let error = read_report_with_deadline(
            read_end.as_raw_fd(),
            &AtomicBool::new(false),
            Instant::now() + Duration::from_millis(10),
        )
        .expect_err("an empty report must time out");
        assert_eq!(error.errno(), libc::ETIMEDOUT);
    }
}
