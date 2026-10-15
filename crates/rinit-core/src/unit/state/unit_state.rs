//! Unit states (plan §1.3(3)).
//!
//! This is the *actual* state of a unit, as opposed to the Job queue's target
//! state (plan §1.3(11)). The two are deliberately separate: the state machine
//! records what happened, the Job queue records what was asked for, and the
//! difference between them is what drives action (plan §1.3(13)).
//!
//! The distinction that costs the most to get right is `Active(Exited)`. It
//! means a `oneshot` ran and succeeded: there is no process, but the unit *is*
//! active and its dependents may proceed. It is not `Inactive`, because
//! `Inactive` means "never ran" or "was stopped", and treating a completed
//! initializer as inactive would re-run it every time something depends on it.
//!
//! This differs from systemd on purpose: systemd needs `RemainAfterExit=yes`
//! for a `oneshot` to stay active, rinit always does (plan §1.3(3)).

/// Why a unit is in `Failed`.
///
/// The two cases are kept apart because a manual `start` must treat them
/// differently (plan §1.3(13)): a breaker trip needs its counters cleared
/// before retrying, whereas a plain start failure can be retried directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailedReason {
    /// The unit failed to start, or exited and `Restart=` did not apply.
    StartFailure,
    /// `StartLimitBurst` failures inside `StartLimitIntervalSec` tripped the
    /// breaker; the unit will not retry on its own.
    StartLimitHit,
}

/// The actual state of a unit (plan §1.3(3)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnitState {
    /// Not running and not expected to be.
    #[default]
    Inactive,
    /// A start is in progress; `Starting` completion depends on `Type=`.
    Starting,
    /// Running, and ready (a `simple` service is ready at exec, a `notify`
    /// service once `READY=1` arrives).
    Active,
    /// A `oneshot` ran to completion successfully. No process, but still
    /// satisfies `Requires`/`After` dependents (plan §1.3(3)).
    ActiveExited,
    /// A stop is in progress.
    Deactivating,
    /// Waiting out a restart backoff (plan §1.3(4)).
    RestartWait,
    /// Failed, with the reason distinguishing a plain failure from a tripped
    /// breaker.
    Failed(FailedReason),
}

impl UnitState {
    /// Whether the unit counts as "up" for dependency purposes.
    ///
    /// This is the predicate that lets a downstream `Requires=` proceed. It is
    /// exactly `Active` or `Active(Exited)` - notably *not* `Starting`, which
    /// would defeat the whole point of ordering dependencies.
    pub fn is_satisfied(&self) -> bool {
        matches!(self, UnitState::Active | UnitState::ActiveExited)
    }

    /// Whether a process is expected to be running.
    pub fn is_live(&self) -> bool {
        matches!(
            self,
            UnitState::Active | UnitState::Starting | UnitState::RestartWait
        )
    }

    /// Whether a stop can be skipped because there is nothing to stop.
    ///
    /// `Active(Exited)` has no process, so `Deactivating` would spin with
    /// nothing to do; plan §1.3(3) requires the direct jump to `Inactive`.
    pub fn has_nothing_to_stop(&self) -> bool {
        matches!(self, UnitState::Inactive | UnitState::ActiveExited)
    }

    /// The short form `rctl status` shows.
    pub fn as_str(&self) -> &'static str {
        match self {
            UnitState::Inactive => "inactive",
            UnitState::Starting => "starting",
            UnitState::Active => "active",
            UnitState::ActiveExited => "active (exited)",
            UnitState::Deactivating => "deactivating",
            UnitState::RestartWait => "restart-wait",
            UnitState::Failed(FailedReason::StartFailure) => "failed",
            UnitState::Failed(FailedReason::StartLimitHit) => "failed (start-limit)",
        }
    }
}

impl core::fmt::Display for UnitState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a running process ended (plan §1.3(4) `Restart=` semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitOutcome {
    /// Exited with code 0.
    Success,
    /// Exited with a non-zero code.
    Failure(i32),
    /// Killed by a signal.
    Signalled(i32),
    /// Failed to reach the ready state within `TimeoutStartSec`.
    TimedOut,
}

impl ExitOutcome {
    /// Whether `Restart=on-failure` should apply.
    ///
    /// Plan §1.3(4): non-zero exit, a signal, or a timeout. `on-abnormal` is
    /// the same set *minus* the non-zero exit code.
    pub fn is_failure(&self) -> bool {
        !matches!(self, ExitOutcome::Success)
    }

    /// Whether `Restart=on-abnormal` should apply.
    ///
    /// Signal, timeout or watchdog - but a plain non-zero exit does not count,
    /// which is the only difference from `on-failure`.
    pub fn is_abnormal(&self) -> bool {
        matches!(self, ExitOutcome::Signalled(_) | ExitOutcome::TimedOut)
    }
}

use crate::unit::config::RestartPolicy;

impl RestartPolicy {
    /// Whether this policy restarts after `outcome` (plan §1.3(4)).
    ///
    /// The table, verbatim:
    ///
    /// | value         | restarts when                                        |
    /// |---------------|-------------------------------------------------------|
    /// | `no`          | never                                                 |
    /// | `always`      | anything, including exit code 0                       |
    /// | `on-success`  | exit code 0 and not signalled                         |
    /// | `on-failure`  | non-zero ∨ signalled ∨ timeout ∨ watchdog             |
    /// | `on-abnormal` | signalled ∨ timeout ∨ watchdog (not a non-zero exit)  |
    pub fn wants_restart(self, outcome: ExitOutcome) -> bool {
        match self {
            RestartPolicy::No => false,
            RestartPolicy::Always => true,
            // A process killed by a signal has no meaningful exit code, so it
            // is never "success" even if the status byte looked like 0.
            RestartPolicy::OnSuccess => matches!(outcome, ExitOutcome::Success),
            RestartPolicy::OnFailure => outcome.is_failure(),
            RestartPolicy::OnAbnormal => outcome.is_abnormal(),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use super::*;

    #[test]
    fn only_active_states_satisfy_dependents() {
        assert!(UnitState::Active.is_satisfied());
        assert!(UnitState::ActiveExited.is_satisfied());

        for state in [
            UnitState::Inactive,
            UnitState::Starting,
            UnitState::Deactivating,
            UnitState::RestartWait,
            UnitState::Failed(FailedReason::StartFailure),
            UnitState::Failed(FailedReason::StartLimitHit),
        ] {
            assert!(
                !state.is_satisfied(),
                "{state} must not satisfy a dependency"
            );
        }
    }

    /// `Active(Exited)` is the case that makes `oneshot` work at all.
    #[test]
    fn active_exited_is_satisfied_but_has_nothing_to_stop() {
        let state = UnitState::ActiveExited;
        assert!(
            state.is_satisfied(),
            "dependents must be allowed to proceed"
        );
        assert!(!state.is_live(), "no process is running");
        assert!(state.has_nothing_to_stop());
    }

    #[test]
    fn inactive_neither_satisfies_nor_lives() {
        let state = UnitState::Inactive;
        assert!(!state.is_satisfied());
        assert!(!state.is_live());
        assert!(state.has_nothing_to_stop());
    }

    #[test]
    fn starting_is_not_yet_satisfied() {
        // This is what makes After= ordering meaningful: a dependent waits.
        assert!(!UnitState::Starting.is_satisfied());
        assert!(UnitState::Starting.is_live());
    }

    #[test]
    fn failed_states_render_distinctly() {
        let failure = UnitState::Failed(FailedReason::StartFailure);
        let tripped = UnitState::Failed(FailedReason::StartLimitHit);

        assert_ne!(
            failure.to_string(),
            tripped.to_string(),
            "a tripped breaker must be distinguishable from a plain failure"
        );
        assert!(!failure.is_satisfied() && !tripped.is_satisfied());
    }

    // --- Restart= semantics (plan §1.3(4)) -----------------------------

    const OUTCOMES: [ExitOutcome; 4] = [
        ExitOutcome::Success,
        ExitOutcome::Failure(1),
        ExitOutcome::Signalled(9),
        ExitOutcome::TimedOut,
    ];

    #[test]
    fn restart_never_applies_for_no() {
        for outcome in OUTCOMES {
            assert!(!RestartPolicy::No.wants_restart(outcome), "{outcome:?}");
        }
    }

    #[test]
    fn restart_always_applies_for_anything() {
        for outcome in OUTCOMES {
            assert!(RestartPolicy::Always.wants_restart(outcome), "{outcome:?}");
        }
    }

    /// A signalled process has no meaningful exit code, so it is never a
    /// success even though the raw status byte is 0.
    #[test]
    fn on_success_only_applies_to_a_clean_exit() {
        assert!(RestartPolicy::OnSuccess.wants_restart(ExitOutcome::Success));
        assert!(!RestartPolicy::OnSuccess.wants_restart(ExitOutcome::Failure(0)));
        assert!(!RestartPolicy::OnSuccess.wants_restart(ExitOutcome::Signalled(9)));
        assert!(!RestartPolicy::OnSuccess.wants_restart(ExitOutcome::TimedOut));
    }

    /// `on-failure` covers every abnormal ending, including a non-zero exit.
    #[test]
    fn on_failure_covers_everything_but_success() {
        assert!(!RestartPolicy::OnFailure.wants_restart(ExitOutcome::Success));
        assert!(RestartPolicy::OnFailure.wants_restart(ExitOutcome::Failure(1)));
        assert!(RestartPolicy::OnFailure.wants_restart(ExitOutcome::Signalled(9)));
        assert!(RestartPolicy::OnFailure.wants_restart(ExitOutcome::TimedOut));
    }

    /// `on-abnormal` is `on-failure` minus the non-zero exit - the single
    /// difference between them (plan §1.3(4)).
    #[test]
    fn on_abnormal_excludes_a_plain_nonzero_exit() {
        assert!(!RestartPolicy::OnAbnormal.wants_restart(ExitOutcome::Success));
        assert!(
            !RestartPolicy::OnAbnormal.wants_restart(ExitOutcome::Failure(1)),
            "a non-zero exit is not 'abnormal' for on-abnormal"
        );
        assert!(RestartPolicy::OnAbnormal.wants_restart(ExitOutcome::Signalled(9)));
        assert!(RestartPolicy::OnAbnormal.wants_restart(ExitOutcome::TimedOut));
    }

    #[test]
    fn is_failure_and_is_abnormal_differ_only_on_nonzero_exit() {
        for outcome in [
            ExitOutcome::Success,
            ExitOutcome::Signalled(9),
            ExitOutcome::TimedOut,
        ] {
            assert_eq!(
                outcome.is_failure(),
                outcome.is_abnormal(),
                "{outcome:?}: the two sets coincide here"
            );
        }
        assert!(ExitOutcome::Failure(1).is_failure());
        assert!(!ExitOutcome::Failure(1).is_abnormal());
    }
}
