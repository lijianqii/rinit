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
