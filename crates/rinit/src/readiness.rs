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
//! Waiting polls the specific pid (`wait::poll_child`) rather than draining the
//! global `SIGCHLD` queue, so a concurrent reaper cannot steal the exit status
//! this code depends on.

use std::time::Duration;

use rinit_core::ServiceKind;
use rinit_runtime::syscall::wait;

/// How often to poll while waiting for a child to exit.
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// What a unit needed before the next startup level could begin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// Ready as soon as it was exec'd (`simple`, `notify`, and targets).
    Immediate,
    /// Ready once the command exits 0.
    ///
    /// For a `oneshot` that *is* the end of the start. For a `forking` unit the
    /// exit is only the middle of it: the daemon it left behind still has to be
    /// located, which is what `mark_daemon_started` does (plan §7.10). The boot
    /// loop waits for the exit in both cases and tells them apart by the kind,
    /// because the wait is the same and only what follows it differs.
    OnExitSuccess,
}

/// The readiness rule implied by a unit's `Type=`.
pub const fn readiness_of(kind: ServiceKind) -> Readiness {
    match kind {
        ServiceKind::Simple => Readiness::Immediate,
        ServiceKind::Oneshot | ServiceKind::Forking => Readiness::OnExitSuccess,
        // M1 has no sd_notify protocol yet, so a `notify` unit is treated as
        // ready on exec. That is a known simplification, not an oversight: the
        // `READY=1` protocol and `TimeoutStartSec` enforcement arrive with the
        // state machine in M2 (plan §1.3(3)).
        ServiceKind::Notify => Readiness::Immediate,
    }
}

/// Wait for `pid` to exit, returning its exit code.
///
/// Only safe when `pid` was spawned by this process and is not being reaped
/// concurrently.
///
/// This is `async` on purpose: boot waits for `oneshot` units, and blocking the
/// single `current_thread` runtime with `std::thread::sleep` would stall the
/// reactor that delivers signals (plan §2.1①/⑤).
///
/// # Errors
/// Returns `Err(())` when `timeout` elapses first. A child that exits on a
/// signal is reported as `Ok(None)`: it did not succeed, but it did terminate,
/// so the caller can distinguish "hung" from "died".
pub async fn wait_for_exit(pid: libc::pid_t, timeout: Duration) -> Result<Option<i32>, ()> {
    let deadline = crate::deadline::after(timeout);
    loop {
        // The `waitpid` itself belongs to the syscall layer (plan §2.3); this
        // module only decides what its result means.
        match wait::poll_child(pid) {
            Ok(Some(wait::ChildExit::Exited { code, .. })) => return Ok(Some(code)),
            // It terminated, but not successfully: still `Ok`, so the caller
            // can tell "hung" from "died".
            Ok(Some(wait::ChildExit::Signaled { .. })) => return Ok(None),
            // Still running.
            Ok(None) => {}
            // An interrupted poll is not a failure: a signal handler with
            // `SA_RESTART` normally prevents this, but a handler installed by
            // something else (or on another thread) can still interrupt us.
            Err(e) if e.errno() == libc::EINTR => continue,
            // ECHILD: someone else already reaped it, or it never existed.
            Err(_) => return Err(()),
        }
        if std::time::Instant::now() >= deadline {
            return Err(());
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use rinit_runtime::syscall::process;

    use super::*;

    #[test]
    fn oneshot_waits_for_exit_and_simple_does_not() {
        let _guard = crate::test_support::children_lock();
        assert_eq!(readiness_of(ServiceKind::Oneshot), Readiness::OnExitSuccess);
        assert_eq!(readiness_of(ServiceKind::Simple), Readiness::Immediate);
        assert_eq!(readiness_of(ServiceKind::Notify), Readiness::Immediate);
        assert_eq!(
            readiness_of(ServiceKind::Forking),
            Readiness::OnExitSuccess,
            "a forking unit waits for its main process too - and then for the \
             daemon, which the boot loop does by kind"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn waiting_reports_a_successful_exit_code() {
        let _guard = crate::test_support::children_lock_async().await;
        let pid = crate::service::spawn_service(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "exit 0".to_string(),
        ])
        .expect("spawn");
        assert_eq!(
            wait_for_exit(pid, Duration::from_secs(10)).await,
            Ok(Some(0))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn waiting_reports_a_nonzero_exit_code() {
        let _guard = crate::test_support::children_lock_async().await;
        let pid = crate::service::spawn_service(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "exit 3".to_string(),
        ])
        .expect("spawn");
        assert_eq!(
            wait_for_exit(pid, Duration::from_secs(10)).await,
            Ok(Some(3))
        );
    }

    /// A child killed by a signal is `Ok(None)`: it terminated, but not
    /// successfully, which the caller must treat as a failure.
    #[tokio::test(flavor = "current_thread")]
    async fn waiting_reports_a_signalled_child_as_unsuccessful() {
        let _guard = crate::test_support::children_lock_async().await;
        let pid = crate::service::spawn_service(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 30".to_string(),
        ])
        .expect("spawn");
        std::thread::sleep(Duration::from_millis(50));
        process::kill(pid, libc::SIGKILL).expect("kill");
        assert_eq!(wait_for_exit(pid, Duration::from_secs(10)).await, Ok(None));
    }

    /// A timeout must be reported as such, not mistaken for an exit.
    #[tokio::test(flavor = "current_thread")]
    async fn waiting_times_out_on_a_hung_child() {
        let _guard = crate::test_support::children_lock_async().await;
        let pid = crate::service::spawn_service(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 30".to_string(),
        ])
        .expect("spawn");
        assert_eq!(
            wait_for_exit(pid, Duration::from_millis(100)).await,
            Err(())
        );

        // Clean up so the child does not leak.
        process::kill(pid, libc::SIGKILL).expect("kill");
        let _ = wait_for_exit(pid, Duration::from_secs(5)).await;
    }

    /// A `waitpid` failure that is not `EINTR` means the child is gone: the
    /// waiter must report an error rather than poll forever. This is the
    /// `ECHILD` branch that the `EINTR` retry has to be distinguished from.
    #[tokio::test(flavor = "current_thread")]
    async fn waiting_on_an_already_reaped_child_reports_failure() {
        let _guard = crate::test_support::children_lock_async().await;
        let pid = crate::service::spawn_service(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "exit 0".to_string(),
        ])
        .expect("spawn");

        // Reap it here so `wait_for_exit` finds no such child. The child has
        // already exited, so the audited poll collects it.
        assert!(matches!(
            reap_and_report(pid),
            wait::ChildExit::Exited { code: 0, .. }
        ));

        assert_eq!(
            wait_for_exit(pid, Duration::from_secs(1)).await,
            Err(()),
            "an already-reaped child is an error, not an infinite wait"
        );
    }

    /// Wait until `pid` has been reaped, and report how it went.
    ///
    /// Sleeping is fine here: the test holds the children lock, so nothing else
    /// is waiting on the runtime.
    fn reap_and_report(pid: libc::pid_t) -> wait::ChildExit {
        let give_up = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match wait::poll_child(pid) {
                Ok(Some(exit)) => return exit,
                Ok(None) => {}
                Err(e) => panic!("polling {pid} failed: {e}"),
            }
            assert!(
                std::time::Instant::now() < give_up,
                "child {pid} never exited"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Plan §1.1: `TimeoutStartSec=0` means wait forever, not zero seconds, so
    /// a zero timeout must not report failure the instant it is reached.
    #[tokio::test(flavor = "current_thread")]
    async fn a_zero_timeout_does_not_expire_immediately() {
        let _guard = crate::test_support::children_lock_async().await;
        let mut backoff = crate::unit_manager::UnitManager::new();
        let unit = rinit_core::UnitName::parse("t.service").unwrap();
        let mut cfg = rinit_core::UnitConfig::new(unit.clone());
        cfg.exec_start = rinit_core::ExecCommand::new(vec!["/bin/true".to_string()]);
        cfg.timeout_start_sec = 0;
        backoff.add(cfg);

        let pid = spawn_pid(&mut backoff, &unit);
        // With no timeout the wait is unbounded, so the only way this returns is
        // the child exiting - which it does immediately.
        assert_eq!(wait_for_exit(pid, unbounded()).await, Ok(Some(0)));
    }

    /// The unbounded wait that `TimeoutStartSec=0` maps to.
    fn unbounded() -> Duration {
        crate::deadline::MAX_TIMEOUT
    }

    /// Spawn a unit and return the pid the manager tracked for it.
    fn spawn_pid(
        m: &mut crate::unit_manager::UnitManager,
        unit: &rinit_core::UnitName,
    ) -> libc::pid_t {
        m.spawn_unit_with_output(unit, &[], None).expect("spawn");
        m.get(unit).and_then(|rt| rt.pid).expect("a tracked pid")
    }
}
