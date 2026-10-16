//! Process creation and signalling (plan §2.3).
//!
//! M0 uses `posix_spawn` for the hardcoded test service. The controlled
//! `fork`+`exec` path (for `User=`, capabilities, `WorkingDirectory=` and
//! cgroup limits) arrives in M5 and must obey the single-threaded-fork
//! constraint documented in plan §2.1⑤.

use std::ffi::{CStr, CString};
use std::os::fd::AsRawFd;

use rinit_core::unit::sandbox::{Sandbox, SandboxStep};

use crate::error::SyscallError;
use crate::syscall::{cred, fs, wait};

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
            stdio: None,
        }
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
    let use_file_actions = working_directory.is_some() || options.stdio.is_some();
    if use_file_actions {
        // SAFETY: a zeroed file-actions struct is the documented starting point
        // for `posix_spawn_file_actions_init`.
        if unsafe { libc::posix_spawn_file_actions_init(&mut file_actions) } != 0 {
            if use_attrs {
                // SAFETY: `attrs` was initialised above.
                unsafe { libc::posix_spawnattr_destroy(&mut attrs) };
            }
            return Err(SyscallError::last_os_error("posix_spawn_file_actions_init"));
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
/// rinit is single-threaded by design (plan §2.1⑤), so the child inherits no
/// other thread's locks; it still calls only `libc` and allocates nothing,
/// because between `fork` and `exec` nothing else is safe to assume. Everything
/// the child needs - the argument vectors, the working directory, the sandbox,
/// the cgroup path - is built in the parent and lives in the address space the
/// child is sharing.
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
pub fn spawn_sandboxed(
    path: &CStr,
    argv: &[CString],
    envp: &[CString],
    options: SpawnOptions,
    working_directory: Option<&CStr>,
    sandbox: &Sandbox,
    paths: ChildPaths<'_>,
) -> Result<libc::pid_t, SyscallError> {
    let setup = ChildSetup {
        path,
        argv: null_terminated(argv),
        envp: null_terminated(envp),
        options,
        working_directory,
        sandbox,
        paths,
    };

    // Close-on-exec, which is what makes "empty read" mean "the child exec'd":
    // the child's copy of the write end disappears the moment `execve`
    // succeeds, and until then a failure can be reported through it.
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

    // The parent's copy of the write end has to go, or the read below would
    // block until every copy is closed - which, for a child that exec'd, would
    // be never.
    drop(report_write);

    let report = read_report(report_read.as_raw_fd());
    match report {
        // End-of-file with nothing written: the child reached `execve`.
        Ok(None) => Ok(pid),
        Ok(Some((step, errno))) => {
            reap_failed_spawn(pid);
            Err(SyscallError::new(step.op(), errno))
        }
        Err(err) => {
            reap_failed_spawn(pid);
            Err(err)
        }
    }
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
    /// The files the child opens and acts on after the fork (plan §7.3, §7.11).
    paths: ChildPaths<'a>,
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

    if setup.options.new_session {
        // SAFETY: `setsid` takes no arguments. The child is not a process-group
        // leader (its group is the parent's), so this cannot fail with `EPERM`.
        if unsafe { libc::setsid() } < 0 {
            die(report_write, ChildStep::Setsid);
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
    for step in setup.sandbox.steps() {
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
    // SAFETY: `path` is a valid, NUL-terminated path.
    let fd = unsafe { libc::open(tty.path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if fd < 0 {
        die(report_write, ChildStep::OpenTty);
    }
    if tty.reset {
        // SAFETY: `fd` is a terminal descriptor we just opened, and this ioctl
        // takes no argument.
        unsafe { libc::ioctl(fd, TIOCVHANGUP) };
    }
    // SAFETY: as above; `TIOCSCTTY` takes an `int` argument, and this kernel
    // accepts 0 for "take it over unconditionally".
    if unsafe { libc::ioctl(fd, TIOCSCTTY, 0) } < 0 {
        die(report_write, ChildStep::SetControllingTerminal);
    }
    for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        // SAFETY: both descriptors are valid; `dup2` closes `target` first.
        if unsafe { libc::dup2(fd, target) } < 0 {
            die(report_write, ChildStep::Dup2);
        }
    }
    if fd > libc::STDERR_FILENO {
        // SAFETY: `fd` is ours and no longer needed: it is 0, 1 and 2 now.
        unsafe { libc::close(fd) };
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
    unsafe { libc::write(report_write, bytes.as_ptr().cast(), bytes.len()) };
    // SAFETY: `_exit` never returns and runs no destructors and no `atexit`
    // handlers - which is exactly what a process that exists only between
    // `fork` and `exec` needs: it must not touch what it shares with its parent
    // beyond what it already did.
    unsafe { libc::_exit(127) }
}

/// Read a child's failure report: `Ok(None)` is end-of-file, which means the
/// child `exec`'d.
///
/// A short read means the child died while reporting - a signal between the
/// `write` and the `_exit` - and is reported as a child that never reached
/// `exec`, with `EIO` standing in for an `errno` the child never sent.
fn read_report(fd: libc::c_int) -> Result<Option<(ChildStep, i32)>, SyscallError> {
    let mut buf = [0u8; REPORT_LEN];
    let mut filled = 0usize;
    while filled < REPORT_LEN {
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
            break;
        }
        filled += rc as usize;
    }

    if filled == 0 {
        return Ok(None);
    }
    if filled < REPORT_LEN {
        return Ok(Some((ChildStep::Exec, libc::EIO)));
    }
    let report = u64::from_le_bytes(buf);
    let code = (report & 0xff) as u8;
    let errno = ((report >> 8) & 0xffff_ffff) as u32 as i32;
    // An unknown code cannot come from this build's child, so it is a bug worth
    // reporting rather than labelling `execve`: the previous `_ => Exec` fallback
    // did exactly that, and every step added after it silently reported itself as
    // a failed `exec`.
    let step =
        ChildStep::from_code(code).ok_or_else(|| SyscallError::new("child report", libc::EIO))?;
    Ok(Some((step, errno)))
}

/// Reap a child that failed before `exec`.
///
/// It has already `_exit`ed, so this is not a wait in any meaningful sense; the
/// point is that the caller's error does not leave a corpse behind for the main
/// loop's reaper to find later.
fn reap_failed_spawn(pid: libc::pid_t) {
    // ECHILD here would mean the main loop's reaper got there first, which is
    // the outcome this is trying to produce anyway.
    let _ = wait::wait_child(pid);
}

/// The NULL-terminated `char *` array `execve` wants.
///
/// Built in the parent on purpose: between `fork` and `exec` the child
/// allocates nothing, so every pointer it uses must already be in the address
/// space it inherited.
fn null_terminated(strings: &[CString]) -> Vec<*const libc::c_char> {
    let mut pointers: Vec<*const libc::c_char> = strings.iter().map(|s| s.as_ptr()).collect();
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
