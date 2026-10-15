//! Startup-time synchronisation (plan §1.3(3), §3 M1).
//!
//! The full lifecycle state machine is M2. M1 only needs one thing from it: a
//! unit's dependents must not start before it is *ready*, and "ready" is not
//! the same for every `Type=` (plan §1.3(3)):
//!
//! | `Type=`    | ready when                                        |
//! |------------|---------------------------------------------------|
//! | `simple`   | the process has been exec'd                       |
//! | `oneshot`  | the command has exited with code 0 (`Active(Exited)`) |
//! | `notify`   | `READY=1` arrives - deferred to M2                 |
//!
//! Getting `oneshot` wrong is not cosmetic. An `After=db-init.service` unit
//! that starts before the database finished initialising fails in a way that
//! looks random, because the failure surfaces much later in whatever depends on
//! it.
//!
//! Waiting is done with `waitpid` on the specific pid rather than by draining
//! the global `SIGCHLD` queue, so a concurrent reaper cannot steal the exit
//! status this code depends on.

use std::time::Duration;

use crate::log::rlog;
use crate::service::ServiceSet;
use rinit_core::ServiceKind;

/// How often to poll while waiting for a child to exit.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// What a unit needed before the next startup level could begin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// Ready as soon as it was exec'd (`simple`, `notify`, and targets).
    Immediate,
    /// Ready once the command exits 0 (`oneshot`).
    OnExitSuccess,
}

/// The readiness rule implied by a unit's `Type=`.
pub const fn readiness_of(kind: ServiceKind) -> Readiness {
    match kind {
        ServiceKind::Simple => Readiness::Immediate,
        ServiceKind::Oneshot => Readiness::OnExitSuccess,
        // M1 has no sd_notify protocol yet, so a `notify` unit is treated as
        // ready on exec. That is a known simplification, not an oversight: the
        // `READY=1` protocol and `TimeoutStartSec` enforcement arrive with the
        // state machine in M2 (plan §1.3(3)).
        ServiceKind::Notify => Readiness::Immediate,
    }
}

/// Block until `pid` exits, returning its exit code.
///
/// Only safe when `pid` was spawned by this process and is not being reaped
/// concurrently.
///
/// # Errors
/// Returns `Err(())` when `timeout` elapses first. A child that exits on a
/// signal is reported as `Ok(None)`: it did not succeed, but it did terminate,
/// so the caller can distinguish "hung" from "died".
pub fn wait_for_exit(pid: libc::pid_t, timeout: Duration) -> Result<Option<i32>, ()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: `status` is a valid out-parameter. `WNOHANG` keeps this a
        // poll rather than a block, so the timeout is honoured.
        let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if rc == pid {
            return Ok(if libc::WIFEXITED(status) {
                Some(libc::WEXITSTATUS(status))
            } else {
                None
            });
        }
        if rc == -1 {
            // ECHILD: someone else already reaped it, or it never existed.
            return Err(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(());
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Wait for a batch of `oneshot` units, then forget them from supervision.
///
/// Each entry is `(unit name, pid, TimeoutStartSec)`. A unit that exits
/// non-zero, dies on a signal, or times out is reported as a failure; the rest
/// are still allowed to finish, because one broken initializer must not leave
/// the remaining children unreaped.
///
/// # Errors
/// Returns the names of the units that did not succeed, having already logged
/// the detail.
pub fn await_oneshots(
    services: &mut ServiceSet,
    batch: &[(String, libc::pid_t, u64)],
) -> Vec<String> {
    let mut failed = Vec::new();

    for (name, pid, timeout_sec) in batch {
        // Plan §1.1: `TimeoutStartSec=0` means wait forever.
        let timeout = if *timeout_sec == 0 {
            Duration::from_secs(u32::MAX as u64)
        } else {
            Duration::from_secs(*timeout_sec)
        };

        match wait_for_exit(*pid, timeout) {
            Ok(Some(0)) => {
                rlog!("{name} completed (Active(Exited))");
            }
            Ok(Some(code)) => {
                rlog!("error: {name} exited {code}");
                failed.push(name.clone());
            }
            Ok(None) => {
                rlog!("error: {name} was killed by a signal");
                failed.push(name.clone());
            }
            Err(()) => {
                // Distinguish "hung" from "vanished" by asking supervision.
                if services.contains(*pid) {
                    rlog!("error: {name} timed out after {timeout_sec}s");
                } else {
                    rlog!("error: {name} could not be waited for");
                }
                failed.push(name.clone());
            }
        }
        // Either way the child is gone (or hopeless), so stop tracking it and
        // leave shutdown with nothing to signal.
        services.forget(*pid);
    }

    failed
}

#[cfg(test)]
mod tests {
    use rinit_runtime::syscall::process;

    use super::*;

    #[test]
    fn oneshot_waits_for_exit_and_simple_does_not() {
        assert_eq!(readiness_of(ServiceKind::Oneshot), Readiness::OnExitSuccess);
        assert_eq!(readiness_of(ServiceKind::Simple), Readiness::Immediate);
        assert_eq!(readiness_of(ServiceKind::Notify), Readiness::Immediate);
    }

    #[test]
    fn waiting_reports_a_successful_exit_code() {
        let pid = crate::service::spawn_service(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "exit 0".to_string(),
        ])
        .expect("spawn");
        assert_eq!(wait_for_exit(pid, Duration::from_secs(10)), Ok(Some(0)));
    }

    #[test]
    fn waiting_reports_a_nonzero_exit_code() {
        let pid = crate::service::spawn_service(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "exit 3".to_string(),
        ])
        .expect("spawn");
        assert_eq!(wait_for_exit(pid, Duration::from_secs(10)), Ok(Some(3)));
    }

    /// A child killed by a signal is `Ok(None)`: it terminated, but not
    /// successfully, which the caller must treat as a failure.
    #[test]
    fn waiting_reports_a_signalled_child_as_unsuccessful() {
        let pid = crate::service::spawn_service(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 30".to_string(),
        ])
        .expect("spawn");
        std::thread::sleep(Duration::from_millis(50));
        process::kill(pid, libc::SIGKILL).expect("kill");
        assert_eq!(wait_for_exit(pid, Duration::from_secs(10)), Ok(None));
    }

    /// A timeout must be reported as such, not mistaken for an exit.
    #[test]
    fn waiting_times_out_on_a_hung_child() {
        let pid = crate::service::spawn_service(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 30".to_string(),
        ])
        .expect("spawn");
        assert_eq!(wait_for_exit(pid, Duration::from_millis(100)), Err(()));

        // Clean up so the child does not leak.
        process::kill(pid, libc::SIGKILL).expect("kill");
        let _ = wait_for_exit(pid, Duration::from_secs(5));
    }

    #[test]
    fn awaiting_oneshots_forgets_them_afterwards() {
        let mut services = ServiceSet::new();
        let pid = services
            .spawn(&[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "exit 0".to_string(),
            ])
            .expect("spawn");

        let failed = await_oneshots(&mut services, &[("init.service".to_string(), pid, 10)]);
        assert!(failed.is_empty(), "{failed:?}");
        assert!(services.pids().is_empty(), "the child must be forgotten");
    }

    /// A failing initializer must be named, and must not leave the others
    /// unreaped.
    #[test]
    fn awaiting_oneshots_reports_failures_and_keeps_going() {
        let mut services = ServiceSet::new();
        let bad = services
            .spawn(&[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "exit 1".to_string(),
            ])
            .expect("spawn");
        let good = services
            .spawn(&[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "exit 0".to_string(),
            ])
            .expect("spawn");

        let failed = await_oneshots(
            &mut services,
            &[
                ("bad.service".to_string(), bad, 10),
                ("good.service".to_string(), good, 10),
            ],
        );
        assert_eq!(failed, ["bad.service"]);
        assert!(services.pids().is_empty(), "both must be reaped");
    }

    /// `TimeoutStartSec=0` means wait forever, not wait zero seconds.
    #[test]
    fn a_zero_timeout_does_not_fail_immediately() {
        let mut services = ServiceSet::new();
        let pid = services.spawn(&["/bin/true".to_string()]).expect("spawn");
        let failed = await_oneshots(&mut services, &[("t.service".to_string(), pid, 0)]);
        assert!(failed.is_empty(), "0 must mean 'no timeout': {failed:?}");
    }

    /// A hung `oneshot` must time out, be named, and be stopped being tracked:
    /// leaving it in the set would have shutdown try to SIGTERM a dead pid.
    #[test]
    fn a_hung_oneshot_times_out_and_is_named() {
        let mut services = ServiceSet::new();
        let pid = services
            .spawn(&[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "sleep 30".to_string(),
            ])
            .expect("spawn");
        assert!(services.contains(pid));

        // TimeoutStartSec=1, so the wait is bounded and the test is quick.
        let failed = await_oneshots(&mut services, &[("slow.service".to_string(), pid, 1)]);
        assert_eq!(failed, ["slow.service"], "a hung initializer must fail");
        assert!(
            services.pids().is_empty(),
            "a timed-out child must no longer be supervised"
        );

        // Clean up the child itself so it does not linger.
        process::kill(pid, libc::SIGKILL).ok();
        let _ = wait_for_exit(pid, Duration::from_secs(5));
    }
}
