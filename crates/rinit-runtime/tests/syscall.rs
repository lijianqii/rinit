//! Host-side integration tests for the syscall layer (plan §4.2).
//!
//! These exercise real `posix_spawn`/`waitpid`/`kill` on the host (no aarch64
//! needed). Mount tests are intentionally absent: they require `CAP_SYS_ADMIN`
//! and would fail in an unprivileged CI/container.

use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::sync::Mutex;

use rinit_core::unit::sandbox::Sandbox;
use rinit_runtime::syscall::wait::ChildExit;
use rinit_runtime::syscall::{fs, process, wait};

/// `wait::reap_children()` drains *all* children, so tests that rely on it must
/// not run concurrently with each other (libtest runs tests in parallel).
static REAP_LOCK: Mutex<()> = Mutex::new(());

fn cs(s: &str) -> CString {
    CString::new(s).unwrap()
}

/// Spawn `/bin/sh -c "exit 7"` and confirm we observe exit code 7.
#[test]
fn spawn_and_reap_exit_code() {
    let _guard = REAP_LOCK.lock().unwrap();
    let argv = [cs("sh"), cs("-c"), cs("exit 7")];
    let pid = process::posix_spawn(c"/bin/sh", &argv, &[]).expect("posix_spawn");

    let mut code = None;
    for _ in 0..100 {
        for ev in wait::reap_children() {
            if let ChildExit::Exited { pid: p, code: c } = ev
                && p == pid
            {
                code = Some(c);
            }
        }
        if code.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(code, Some(7), "expected exit code 7 from child {pid}");
}

/// A child killed by a signal must be reported as `Signaled`.
#[test]
fn reap_signaled_child() {
    let _guard = REAP_LOCK.lock().unwrap();
    let argv = [cs("sh"), cs("-c"), cs("sleep 30")];
    let pid = process::posix_spawn(c"/bin/sh", &argv, &[]).expect("posix_spawn");

    // Give sh a moment to exec and install its sleep.
    std::thread::sleep(std::time::Duration::from_millis(50));
    process::kill(pid, libc::SIGKILL).expect("kill");

    let mut killed = false;
    for _ in 0..100 {
        for ev in wait::reap_children() {
            if let ChildExit::Signaled { pid: p, signal } = ev
                && p == pid
            {
                assert_eq!(signal, libc::SIGKILL);
                killed = true;
            }
        }
        if killed {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(killed, "expected child {pid} to be reported as signaled");
}

/// `kill` on a non-existent PID reports `ESRCH` without panicking.
#[test]
fn kill_missing_pid_reports_esrch() {
    // PID 0x7fff_fffe is essentially guaranteed not to exist.
    match process::kill(0x7fff_fffe, libc::SIGTERM) {
        Ok(()) => panic!("expected kill to fail for a missing pid"),
        Err(e) => assert_eq!(e.errno(), libc::ESRCH),
    }
}

#[test]
fn getpid_returns_positive() {
    assert!(process::getpid() > 0);
}

#[test]
fn cstr_roundtrip_sanity() {
    // Guards the helper used throughout; `/bin/sh` must be a valid C string.
    assert_eq!(c"/bin/sh".to_bytes(), b"/bin/sh");
}

/// A spawned service must become a session leader (plan §1.3(6)).
///
/// This is not cosmetic. Services like `getty` call `setsid()` themselves and
/// die if they are already a process-group leader but not a session leader, so
/// rinit has to give them the session *first*. A session leader is also its
/// process group's leader, which is what makes a stop signal the whole tree
/// rather than rinit itself.
#[test]
fn spawn_with_session_makes_the_child_a_session_leader() {
    let _guard = REAP_LOCK.lock().unwrap();
    let argv = [cs("sh"), cs("-c"), cs("sleep 30")];
    let pid = process::posix_spawn_with(
        c"/bin/sh",
        &argv,
        &[],
        process::SpawnOptions::with_session(),
    )
    .expect("posix_spawn");

    let pgid = process::getpgid(pid).expect("getpgid");
    assert_eq!(
        pgid, pid,
        "the child must lead its own process group, got pgid {pgid} for pid {pid}"
    );
    assert_ne!(
        pgid,
        process::getpid(),
        "the group must not be rinit's own group"
    );
    // SAFETY: `getsid` takes a scalar pid and cannot fault.
    assert_eq!(
        unsafe { libc::getsid(pid) },
        pid,
        "the child must be a session leader, or getty would die on its own setsid"
    );

    process::kill(pid, libc::SIGKILL).expect("kill");
    drain_until_reaped(pid);
}

/// Signalling the group must reach grandchildren the service forked, which a
/// signal to the direct child would miss.
#[test]
fn kill_process_group_reaches_grandchildren() {
    let _guard = REAP_LOCK.lock().unwrap();
    // The shell starts a long sleep as a child of its own, so the sleep is a
    // grandchild of rinit and shares the shell's process group.
    let script = "sleep 30 & echo $! ; wait";
    let argv = [cs("sh"), cs("-c"), cs(script)];
    let pid = process::posix_spawn_with(
        c"/bin/sh",
        &argv,
        &[],
        process::SpawnOptions::with_session(),
    )
    .expect("posix_spawn");

    // Wait for the backgrounded sleep to exist inside the group.
    std::thread::sleep(std::time::Duration::from_millis(200));
    let pgid = process::getpgid(pid).expect("getpgid");
    assert_eq!(pgid, pid);

    // Signalling the group kills the shell *and* its backgrounded child.
    process::kill_process_group(pid, libc::SIGKILL).expect("killpg");
    drain_until_reaped(pid);

    assert!(
        !process::kill(pgid, 0).is_ok(),
        "the process group must be empty once both are dead"
    );
}

/// Signalling a group that has already exited must report ESRCH, not panic.
#[test]
fn kill_process_group_reports_esrch_when_gone() {
    let _guard = REAP_LOCK.lock().unwrap();
    let argv = [cs("sh"), cs("-c"), cs("exit 0")];
    let pid = process::posix_spawn_with(
        c"/bin/sh",
        &argv,
        &[],
        process::SpawnOptions::with_session(),
    )
    .expect("posix_spawn");
    drain_until_reaped(pid);

    let err = process::kill_process_group(pid, libc::SIGTERM).expect_err("the group is empty");
    assert_eq!(err.errno(), libc::ESRCH);
}

/// Without the option, the child stays in *our* process group - which is
/// exactly why the option exists: a stop that signals the group would
/// otherwise signal rinit itself.
///
/// The group is read via `getpgid(0)` rather than compared to the pid, because
/// libtest runs each test on its own thread and a thread may lead its own
/// group; the pid is simply not the group id.
#[test]
fn spawn_without_the_option_shares_our_group() {
    let _guard = REAP_LOCK.lock().unwrap();
    let argv = [cs("sh"), cs("-c"), cs("sleep 30")];
    let pid = process::posix_spawn(c"/bin/sh", &argv, &[]).expect("posix_spawn");
    let our_pgid = process::getpgid(0).expect("getpgid(0)");
    let child_pgid = process::getpgid(pid).expect("getpgid");
    assert_eq!(
        child_pgid, our_pgid,
        "the default is to inherit the caller's group"
    );
    process::kill(pid, libc::SIGKILL).expect("kill");
    drain_until_reaped(pid);
}

/// Reap every child until `pid` is gone, so a test never leaves a zombie or a
/// live process behind for the next one.
fn drain_until_reaped(target: libc::pid_t) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let mut found = false;
        for ev in wait::reap_children() {
            match ev {
                ChildExit::Exited { pid, .. } | ChildExit::Signaled { pid, .. }
                    if pid == target =>
                {
                    found = true;
                }
                _ => {}
            }
        }
        if found || std::time::Instant::now() > deadline {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// `poll_child` is the single-child form of the reaper, used by the callers
/// that wait for one specific pid (a `oneshot` unit, a conflicting service that
/// must stop first). A live child must read as "not yet", not as an error.
#[test]
fn poll_child_reports_a_running_child_as_still_running() {
    let _guard = REAP_LOCK.lock().unwrap();
    let argv = [cs("sh"), cs("-c"), cs("sleep 30")];
    let pid = process::posix_spawn(c"/bin/sh", &argv, &[]).expect("posix_spawn");

    assert!(
        matches!(wait::poll_child(pid), Ok(None)),
        "a child that has not exited is `None`, not a failure"
    );

    process::kill(pid, libc::SIGKILL).expect("kill");
    drain_until_reaped(pid);
}

/// The exit code must survive the single-child path exactly as it does the
/// drain path.
#[test]
fn poll_child_reports_a_successful_exit_code() {
    let _guard = REAP_LOCK.lock().unwrap();
    let argv = [cs("sh"), cs("-c"), cs("exit 4")];
    let pid = process::posix_spawn(c"/bin/sh", &argv, &[]).expect("posix_spawn");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match wait::poll_child(pid) {
            Ok(Some(ChildExit::Exited { code, .. })) => {
                assert_eq!(code, 4, "the exit code must be reported as-is");
                return;
            }
            Ok(Some(ChildExit::Signaled { signal, .. })) => {
                panic!("the child was signalled ({signal}) instead of exiting");
            }
            Ok(None) => {}
            Err(e) => panic!("poll_child failed: {e}"),
        }
        assert!(
            std::time::Instant::now() < deadline,
            "child {pid} never exited"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Death by signal is the other terminal state, and the stopper relies on
/// seeing it (`SIGKILL` at `TimeoutStopSec`).
#[test]
fn poll_child_reports_death_by_signal() {
    let _guard = REAP_LOCK.lock().unwrap();
    let argv = [cs("sh"), cs("-c"), cs("sleep 30")];
    let pid = process::posix_spawn(c"/bin/sh", &argv, &[]).expect("posix_spawn");
    process::kill(pid, libc::SIGKILL).expect("kill");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match wait::poll_child(pid) {
            Ok(Some(ChildExit::Signaled { signal, .. })) => {
                assert_eq!(signal, libc::SIGKILL);
                return;
            }
            Ok(Some(ChildExit::Exited { code, .. })) => {
                panic!("a SIGKILLed child must not report exit code {code}");
            }
            Ok(None) => {}
            Err(e) => panic!("poll_child failed: {e}"),
        }
        assert!(
            std::time::Instant::now() < deadline,
            "child {pid} never died"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Polling a child that is already gone is `ECHILD`, which the waiter must be
/// able to tell apart from "still running" - otherwise a stolen exit status
/// would look like a timeout.
#[test]
fn poll_child_on_a_reaped_child_is_echild() {
    let _guard = REAP_LOCK.lock().unwrap();
    let argv = [cs("sh"), cs("-c"), cs("exit 0")];
    let pid = process::posix_spawn(c"/bin/sh", &argv, &[]).expect("posix_spawn");
    drain_until_reaped(pid);

    let err = wait::poll_child(pid).expect_err("the child is already reaped");
    assert_eq!(err.errno(), libc::ECHILD);
}

/// `wait_child` blocks for the one child it was given, and says `ECHILD` for
/// one that was already reaped - the difference between "gone" and "never was".
#[test]
fn wait_child_blocks_for_its_child_and_reports_echild_afterwards() {
    let _guard = REAP_LOCK.lock().unwrap();
    let argv = [cs("sh"), cs("-c"), cs("sleep 0.05; exit 4")];
    let pid = process::posix_spawn(c"/bin/sh", &argv, &[]).expect("posix_spawn");

    assert_eq!(
        wait::wait_child(pid).expect("wait"),
        ChildExit::Exited { pid, code: 4 }
    );
    let err = wait::wait_child(pid).expect_err("the child is already reaped");
    assert_eq!(err.errno(), libc::ECHILD);
}

// --- §7.4: the privilege drop ---------------------------------------------
//
// The host can only prove part of this: an unprivileged test process may not
// become another user, so "a service really runs as nobody" is asserted in the
// guest instead (`scripts/system-test.sh`). What *is* provable here is the
// machinery around the drop: that a sandboxed child runs, that
// `NoNewPrivileges=` reaches it, that the working directory is applied before
// `exec`, and - the part that matters most - that a step which cannot be
// applied fails the spawn closed, by name, leaving no zombie behind.

/// Run `command` through `/bin/sh -c` under `sandbox`, with its stdout
/// captured, and return `(output, exit code)`.
fn run_captured(
    command: &str,
    sandbox: &Sandbox,
    working_directory: Option<&str>,
) -> (String, i32) {
    let (read_end, write_end) = fs::pipe().expect("pipe");
    let argv = [cs("sh"), cs("-c"), cs(command)];
    let directory = working_directory.map(cs);
    let pid = process::spawn_sandboxed(
        c"/bin/sh",
        &argv,
        &[],
        process::SpawnOptions::default().with_stdio(write_end.as_raw_fd()),
        directory.as_deref(),
        sandbox,
        None,
    )
    .expect("sandboxed spawn");

    // rinit's own copy of the write end has to go, or the read below waits for
    // an end-of-file that can never arrive.
    drop(write_end);

    let mut output = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        match fs::read(read_end.as_raw_fd(), &mut buf) {
            Ok(0) => break,
            Ok(n) => output.extend_from_slice(&buf[..n]),
            Err(e) => panic!("reading the child's output failed: {e}"),
        }
    }

    let code = match wait::wait_child(pid).expect("wait for the child") {
        ChildExit::Exited { code, .. } => code,
        other => panic!("expected an exit, got {other:?}"),
    };
    (String::from_utf8_lossy(&output).into_owned(), code)
}

/// The whole point of the `fork`+`exec` path: it runs the command, and the
/// child's exit status comes back the way any other child's does.
#[test]
fn a_sandboxed_spawn_runs_the_command() {
    let _guard = REAP_LOCK.lock().unwrap();
    let (output, code) = run_captured("echo hello; exit 9", &Sandbox::inherit(), None);
    assert_eq!(output, "hello\n");
    assert_eq!(code, 9);
}

/// `NoNewPrivileges=true` is only useful if it is actually set in the child,
/// and a child that was not asked for it must not have it - otherwise the
/// assertion below would pass for the wrong reason.
#[test]
fn no_new_privileges_reaches_the_child() {
    let _guard = REAP_LOCK.lock().unwrap();
    let probe = "test \"$(grep ^NoNewPrivs: /proc/self/status | cut -f2)\" = 1";

    let (_, with_flag) = run_captured(
        probe,
        &Sandbox {
            no_new_privileges: true,
            ..Sandbox::inherit()
        },
        None,
    );
    assert_eq!(with_flag, 0, "the child should report NoNewPrivs: 1");

    // The baseline is the test process's own flag, not a hardcoded 0: a test
    // runner that already denies privilege escalation would otherwise make this
    // assertion about the runner rather than about rinit.
    let runner_has_it = std::fs::read_to_string("/proc/self/status").is_ok_and(|status| {
        status
            .lines()
            .any(|l| l.starts_with("NoNewPrivs:") && l.ends_with('1'))
    });
    let (_, without_flag) = run_captured(probe, &Sandbox::inherit(), None);
    assert_eq!(
        without_flag == 0,
        runner_has_it,
        "the child must inherit the flag only when the sandbox does not set it"
    );
}

/// A sandboxed child ends up as the user it was given. On the host this can only
/// be the user we already are - which still exercises `setgid`/`setuid` in the
/// child - and the guest test covers the case that needs root.
#[test]
fn a_sandboxed_child_runs_as_the_user_it_was_given() {
    let _guard = REAP_LOCK.lock().unwrap();
    let uid = process::geteuid();
    let command = format!("test \"$(grep ^Uid: /proc/self/status | cut -f2)\" = {uid}");
    let (_, code) = run_captured(
        &command,
        &Sandbox {
            uid,
            gid: unsafe { libc::getgid() },
            ..Sandbox::inherit()
        },
        None,
    );
    assert_eq!(code, 0, "the child should report uid {uid}");
}

/// `WorkingDirectory=` is applied in the child, before `exec`.
#[test]
fn the_working_directory_is_applied_before_exec() {
    let _guard = REAP_LOCK.lock().unwrap();
    let (output, code) = run_captured("pwd", &Sandbox::inherit(), Some("/etc"));
    assert_eq!(code, 0);
    assert_eq!(output.trim_end(), "/etc");
}

/// A working directory that does not exist must not start the service somewhere
/// else: plan §7.4 makes it fatal.
#[test]
fn a_missing_working_directory_fails_the_spawn_closed() {
    let _guard = REAP_LOCK.lock().unwrap();
    let argv = [cs("sh"), cs("-c"), cs("true")];
    let err = process::spawn_sandboxed(
        c"/bin/sh",
        &argv,
        &[],
        process::SpawnOptions::default(),
        Some(c"/rinit-there-is-no-such-directory"),
        &Sandbox::inherit(),
        None,
    )
    .expect_err("a missing WorkingDirectory must not spawn");
    assert_eq!(err.op(), "chdir");
    assert_eq!(err.errno(), libc::ENOENT);
    assert!(
        wait::reap_children().is_empty(),
        "the child that could not start must have been reaped"
    );
}

/// A command that does not exist is reported as `execve`'s `ENOENT`, which is
/// the failure plan §1.1 contracts as "start failed" rather than as a silent
/// short-lived process.
#[test]
fn a_missing_command_is_reported_as_execve_enoent() {
    let _guard = REAP_LOCK.lock().unwrap();
    let argv = [cs("rinit-no-such-binary")];
    let err = process::spawn_sandboxed(
        c"/rinit-no-such-binary",
        &argv,
        &[],
        process::SpawnOptions::default(),
        None,
        &Sandbox::inherit(),
        None,
    )
    .expect_err("a missing command must not spawn");
    assert_eq!(err.op(), "execve");
    assert_eq!(err.errno(), libc::ENOENT);
    assert!(
        wait::reap_children().is_empty(),
        "the child that could not be exec'd must have been reaped"
    );
}

/// A step rinit is not allowed to carry out fails the spawn, and names the step.
///
/// As an unprivileged process, `setgroups` is the reachable one (`EPERM`): this
/// is the fail-closed rule of plan §7.4 verified from the outside. As root there
/// is no step an unprivileged test could be refused, so the case is asserted in
/// the guest instead.
#[test]
fn a_step_that_cannot_be_applied_fails_the_spawn_closed() {
    let _guard = REAP_LOCK.lock().unwrap();
    if process::geteuid() == 0 {
        return;
    }
    let argv = [cs("sh"), cs("-c"), cs("true")];
    let err = process::spawn_sandboxed(
        c"/bin/sh",
        &argv,
        &[],
        process::SpawnOptions::default(),
        None,
        &Sandbox {
            groups: Some(Vec::new()),
            ..Sandbox::inherit()
        },
        None,
    )
    .expect_err("an unprivileged rinit cannot set the group list");
    assert_eq!(err.op(), "setgroups");
    assert_eq!(err.errno(), libc::EPERM);
    assert!(
        wait::reap_children().is_empty(),
        "the child that could not drop privileges must have been reaped"
    );
}
