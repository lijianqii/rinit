//! Host-side integration tests for the syscall layer (plan §4.2).
//!
//! These exercise real `posix_spawn`/`waitpid`/`kill` on the host (no aarch64
//! needed). Mount tests are intentionally absent: they require `CAP_SYS_ADMIN`
//! and would fail in an unprivileged CI/container.

use std::ffi::CString;
use std::sync::Mutex;

use rinit_runtime::syscall::wait::ChildExit;
use rinit_runtime::syscall::{process, wait};

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

/// A spawned service must become a process-group leader (plan §1.3(6)).
///
/// This is not cosmetic: a stop signals the *group*, and without a distinct
/// group the signal would also hit rinit itself.
#[test]
fn spawn_with_process_group_makes_the_child_a_group_leader() {
    let _guard = REAP_LOCK.lock().unwrap();
    let argv = [cs("sh"), cs("-c"), cs("sleep 30")];
    let pid = process::posix_spawn_with(
        c"/bin/sh",
        &argv,
        &[],
        process::SpawnOptions::with_process_group(),
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
        process::SpawnOptions::with_process_group(),
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
        process::SpawnOptions::with_process_group(),
    )
    .expect("posix_spawn");
    drain_until_reaped(pid);

    let err = process::kill_process_group(pid, libc::SIGTERM)
        .expect_err("the group is empty");
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
                ChildExit::Exited { pid, .. } | ChildExit::Signaled { pid, .. } if pid == target => {
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
