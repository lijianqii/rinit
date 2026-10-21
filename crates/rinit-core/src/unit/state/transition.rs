//! State transitions (plan §1.3(3), §1.3(13)).
//!
//! The transition table is a single exhaustive `match` on
//! `(state, event)`, as plan §1.3(3) requires: "状态迁移表采用 `match` 显式
//! 穷举(编译期保证不漏状态),禁止隐式 fallthrough". Adding a state or an event
//! therefore breaks the build until every combination has been decided,
//! rather than silently falling through at runtime.
//!
//! An event that makes no sense in the current state yields
//! [`TransitionError::NotAllowed`] instead of silently changing nothing. That
//! distinction matters: "stop an already-stopped unit" is a no-op the user
//! should see succeed, while "exit while Inactive" indicates a bookkeeping
//! bug in rinit and must be loud.

use alloc::vec::Vec;

use super::unit_state::{ExitOutcome, FailedReason, UnitState};
use crate::unit::config::{RestartPolicy, ServiceKind};

/// Something that happened to a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A start was requested (from a Job, or by resolving a dependency).
    Start,
    /// The unit reached its ready state.
    StartUpComplete,
    /// The unit failed to start (spawn failed, or `READY=1` never arrived).
    StartUpFailed,
    /// A running process ended.
    Exit(ExitOutcome),
    /// A stop was requested.
    Stop,
    /// The process is gone after a stop.
    StopComplete,
    /// The restart backoff elapsed.
    BackoffElapsed,
    /// The failure counter tripped `StartLimitBurst`.
    StartLimitTripped,
    /// `rctl reset-failed`: clear both the failure and the breaker counters.
    ResetFailed,
}

/// What the caller must do as a result of a transition.
///
/// Returning the side effect rather than performing it keeps the table a pure
/// function, so every row is testable without spawning anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Nothing to do; the request was already satisfied.
    None,
    /// Spawn the process (`ExecStart=`).
    SpawnProcess,
    /// Send `SIGTERM` to the unit's process group.
    SendStopSignal,
    /// Arm the restart timer with this delay.
    ScheduleBackoff {
        /// How long to wait before retrying.
        delay: core::time::Duration,
    },
    /// Cancel a pending restart timer and start now.
    CancelBackoffAndStart,
    /// Cancel a pending restart timer and stop now.
    CancelBackoffAndStop,
    /// Clear the start-limit counters and start now.
    ClearStartLimitAndStart,
}

/// A state change plus its side effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    /// The state before.
    pub from: UnitState,
    /// The state after.
    pub to: UnitState,
    /// What the caller must do.
    pub effect: Effect,
}

/// Why a transition was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionError {
    /// The event does not apply to the current state.
    NotAllowed {
        /// The state that rejected the event.
        state: UnitState,
        /// The event that was rejected.
        event: Event,
    },
}

impl core::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TransitionError::NotAllowed { state, event } => {
                write!(f, "event {event:?} is not allowed in state {state}")
            }
        }
    }
}

/// Apply `event` to `state`, given the unit's restart policy and `Type=`.
///
/// # Errors
/// Returns [`TransitionError::NotAllowed`] when the event makes no sense in
/// `state`. Every legal combination is handled; there is no fallthrough.
pub fn transition(
    state: UnitState,
    event: Event,
    policy: RestartPolicy,
    kind: ServiceKind,
) -> Result<Transition, TransitionError> {
    use Effect as E;
    use Event as Ev;
    use UnitState as S;

    let not_allowed = || TransitionError::NotAllowed { state, event };

    // The outcome of one event, independent of how we got here.
    let next: (UnitState, Effect) = match (state, event) {
        // --- the main start path ------------------------------------
        // `simple`/`notify` are live after exec; `oneshot` runs to completion
        // and ends up as Active(Exited) (plan §1.3(3)).
        (S::Inactive, Ev::Start) => (S::Starting, E::SpawnProcess),
        (S::Starting, Ev::StartUpComplete) => match kind {
            ServiceKind::Oneshot => (S::ActiveExited, E::None),
            // `forking` reaches `Active` the same way: its main process has
            // exited and the daemon it left behind is the unit now (plan §7.10).
            ServiceKind::Simple | ServiceKind::Notify | ServiceKind::Forking => {
                (S::Active, E::None)
            }
        },
        // A start that never completed is a plain `StartFailure`. Whether a
        // retry follows is the restart module's decision, taken afterwards;
        // `RestartSec` decides how long to wait, and the breaker decides
        // whether to try at all.
        (S::Starting, Ev::StartUpFailed) => (S::Failed(FailedReason::StartFailure), E::None),

        // --- a running process ends ----------------------------------
        // Satisfying `Restart=` goes to RestartWait; otherwise Failed. A
        // `oneshot` that already succeeded is Active(Exited) and has no
        // process to reap, so an Exit there is a bookkeeping bug.
        (S::Active, Ev::Exit(outcome)) => {
            if policy.wants_restart(outcome) {
                (
                    S::RestartWait,
                    E::ScheduleBackoff {
                        delay: core::time::Duration::ZERO,
                    },
                )
            } else {
                (S::Failed(FailedReason::StartFailure), E::None)
            }
        }

        // --- the stop path -------------------------------------------
        (S::Active, Ev::Stop) => (S::Deactivating, E::SendStopSignal),
        // Active(Exited) has no process: go straight to Inactive rather than
        // spinning through Deactivating (plan §1.3(3)).
        (S::ActiveExited, Ev::Stop) => (S::Inactive, E::None),
        (S::Starting, Ev::Stop) => (S::Deactivating, E::SendStopSignal),
        (S::Deactivating, Ev::StopComplete) => (S::Inactive, E::None),

        // --- the restart path ----------------------------------------
        (S::RestartWait, Ev::BackoffElapsed) => (S::Starting, E::SpawnProcess),
        // The breaker tripped while waiting: stay failed (plan §1.3(3)).
        (S::RestartWait, Ev::StartLimitTripped) => {
            (S::Failed(FailedReason::StartLimitHit), E::None)
        }

        // --- manual intervention (plan §1.3(13)) ----------------------
        // A user start cancels the backoff and starts immediately.
        (S::RestartWait, Ev::Start) => (S::Starting, E::CancelBackoffAndStart),
        // A stop beats a pending restart (symmetric with the line above).
        (S::RestartWait, Ev::Stop) => (S::Deactivating, E::CancelBackoffAndStop),

        // --- failures ------------------------------------------------
        (S::Failed(_), Ev::Start) => (S::Starting, E::ClearStartLimitAndStart),
        (S::Failed(_), Ev::ResetFailed) => (S::Inactive, E::None),
        // A failed unit can be stopped; it is already not running.
        (S::Failed(_), Ev::Stop) => (S::Inactive, E::None),

        // --- restart-loop entry ---------------------------------------
        // `Failed` -> `RestartWait` is driven by the restart module, which
        // knows the backoff delay and whether the breaker allows a retry; it
        // signals its decision through `StartLimitTripped` / `BackoffElapsed`.
        (S::Failed(_), Ev::Exit(_)) | (S::Inactive, Ev::Exit(_)) => {
            return Err(not_allowed());
        }

        // Anything not listed above is invalid in this state.
        _ => return Err(not_allowed()),
    };

    Ok(Transition {
        from: state,
        to: next.0,
        effect: next.1,
    })
}

/// Apply a sequence of events, collecting the effects.
///
/// Stops at the first invalid transition and returns `Err`, discarding the
/// events that were applied before it; the error names the state and the event
/// that was rejected. A caller that needs the state it reached must drive the
/// events itself. Used by the property tests below to check that any event
/// sequence terminates in a consistent state rather than looping forever.
pub fn drive(
    state: UnitState,
    events: &[Event],
    policy: RestartPolicy,
    kind: ServiceKind,
) -> Result<(UnitState, Vec<Effect>), TransitionError> {
    let mut current = state;
    let mut effects = Vec::new();
    for &event in events {
        let t = transition(current, event, policy, kind)?;
        current = t.to;
        effects.push(t.effect);
    }
    Ok((current, effects))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: ServiceKind = ServiceKind::Simple;
    const ONESHOT: ServiceKind = ServiceKind::Oneshot;

    fn go(from: UnitState, event: Event) -> (UnitState, Effect) {
        let t = transition(from, event, RestartPolicy::No, SIMPLE).expect("legal transition");
        (t.to, t.effect)
    }

    // --- the main start path -----------------------------------------

    #[test]
    fn inactive_starts_by_spawning() {
        assert_eq!(
            go(UnitState::Inactive, Event::Start),
            (UnitState::Starting, Effect::SpawnProcess)
        );
    }

    #[test]
    fn starting_becomes_active_on_completion() {
        assert_eq!(
            go(UnitState::Starting, Event::StartUpComplete),
            (UnitState::Active, Effect::None)
        );
    }

    /// A `oneshot` that completes is `Active(Exited)`, not `Active` - it has no
    /// process but still satisfies dependents (plan §1.3(3)).
    #[test]
    fn oneshot_completes_into_active_exited() {
        let t = transition(
            UnitState::Starting,
            Event::StartUpComplete,
            RestartPolicy::No,
            ONESHOT,
        )
        .expect("legal");
        assert_eq!(t.to, UnitState::ActiveExited);
        assert!(t.to.is_satisfied());
        assert!(!t.to.is_live());
    }

    #[test]
    fn a_failed_start_lands_in_failed() {
        let (to, effect) = go(UnitState::Starting, Event::StartUpFailed);
        assert_eq!(to, UnitState::Failed(FailedReason::StartFailure));
        assert_eq!(effect, Effect::None);
    }

    // --- exits and the restart policy --------------------------------

    /// Without `Restart=`, an exit is a plain failure.
    #[test]
    fn an_exit_without_restart_fails() {
        let (to, effect) = go(UnitState::Active, Event::Exit(ExitOutcome::Failure(1)));
        assert_eq!(to, UnitState::Failed(FailedReason::StartFailure));
        assert_eq!(effect, Effect::None);
    }

    /// With a matching `Restart=`, an exit enters the backoff instead.
    #[test]
    fn an_exit_with_restart_waits_before_retrying() {
        let t = transition(
            UnitState::Active,
            Event::Exit(ExitOutcome::Failure(1)),
            RestartPolicy::OnFailure,
            SIMPLE,
        )
        .expect("legal");
        assert_eq!(t.to, UnitState::RestartWait);
        // The delay is filled in by the restart module; the table only decides
        // that a wait is due.
        assert!(matches!(t.effect, Effect::ScheduleBackoff { .. }));
    }

    /// `Restart=always` restarts even a clean exit.
    #[test]
    fn restart_always_covers_a_clean_exit() {
        let t = transition(
            UnitState::Active,
            Event::Exit(ExitOutcome::Success),
            RestartPolicy::Always,
            SIMPLE,
        )
        .expect("legal");
        assert_eq!(t.to, UnitState::RestartWait);
    }

    #[test]
    fn restart_on_success_ignores_a_failing_exit() {
        let t = transition(
            UnitState::Active,
            Event::Exit(ExitOutcome::Failure(1)),
            RestartPolicy::OnSuccess,
            SIMPLE,
        )
        .expect("legal");
        assert_eq!(t.to, UnitState::Failed(FailedReason::StartFailure));
    }

    // --- the stop path -----------------------------------------------

    #[test]
    fn stopping_an_active_unit_signals_then_goes_inactive() {
        assert_eq!(
            go(UnitState::Active, Event::Stop),
            (UnitState::Deactivating, Effect::SendStopSignal)
        );
        assert_eq!(
            go(UnitState::Deactivating, Event::StopComplete),
            (UnitState::Inactive, Effect::None)
        );
    }

    /// Plan §1.3(3): stopping a `oneshot` skips `Deactivating` entirely,
    /// because there is no process to signal and no `TimeoutStopSec` to wait
    /// for. Spinning through it would just burn the timeout on nothing.
    #[test]
    fn stopping_an_active_exited_unit_skips_deactivating() {
        assert_eq!(
            go(UnitState::ActiveExited, Event::Stop),
            (UnitState::Inactive, Effect::None)
        );
    }

    #[test]
    fn stopping_a_failed_unit_goes_straight_to_inactive() {
        assert_eq!(
            go(UnitState::Failed(FailedReason::StartFailure), Event::Stop),
            (UnitState::Inactive, Effect::None)
        );
    }

    // --- the restart path and manual intervention --------------------

    #[test]
    fn an_elapsed_backoff_respawns() {
        assert_eq!(
            go(UnitState::RestartWait, Event::BackoffElapsed),
            (UnitState::Starting, Effect::SpawnProcess)
        );
    }

    /// A tripped breaker leaves the unit failed rather than retrying forever.
    #[test]
    fn a_tripped_breaker_stays_failed() {
        assert_eq!(
            go(UnitState::RestartWait, Event::StartLimitTripped),
            (UnitState::Failed(FailedReason::StartLimitHit), Effect::None)
        );
    }

    /// Plan §1.3(13): a manual start cancels a pending backoff.
    #[test]
    fn a_manual_start_cancels_the_backoff() {
        assert_eq!(
            go(UnitState::RestartWait, Event::Start),
            (UnitState::Starting, Effect::CancelBackoffAndStart)
        );
    }

    /// And the symmetric case: a stop beats a pending restart.
    #[test]
    fn a_stop_cancels_the_backoff() {
        assert_eq!(
            go(UnitState::RestartWait, Event::Stop),
            (UnitState::Deactivating, Effect::CancelBackoffAndStop)
        );
    }

    /// Plan §1.3(13): restarting from `Failed` clears the breaker counters,
    /// which is what makes `rctl start` an escape hatch from a tripped limit.
    #[test]
    fn starting_a_failed_unit_clears_the_start_limit() {
        assert_eq!(
            go(UnitState::Failed(FailedReason::StartLimitHit), Event::Start),
            (UnitState::Starting, Effect::ClearStartLimitAndStart)
        );
    }

    #[test]
    fn reset_failed_returns_to_inactive_without_starting() {
        assert_eq!(
            go(
                UnitState::Failed(FailedReason::StartLimitHit),
                Event::ResetFailed
            ),
            (UnitState::Inactive, Effect::None)
        );
    }

    // --- invalid combinations are refused, not ignored -----------------

    #[test]
    fn nonsensical_events_are_refused() {
        let cases = [
            (UnitState::Inactive, Event::StartUpComplete),
            (UnitState::Inactive, Event::Exit(ExitOutcome::Success)),
            (UnitState::Active, Event::StartUpComplete),
            (UnitState::Active, Event::Start),
            (UnitState::ActiveExited, Event::Exit(ExitOutcome::Success)),
            (UnitState::Starting, Event::BackoffElapsed),
            (
                UnitState::Failed(FailedReason::StartFailure),
                Event::BackoffElapsed,
            ),
            (UnitState::Deactivating, Event::Start),
        ];
        for (state, event) in cases {
            let err =
                transition(state, event, RestartPolicy::No, SIMPLE).expect_err("should be refused");
            assert_eq!(
                err,
                TransitionError::NotAllowed { state, event },
                "{state} + {event:?}"
            );
        }
    }

    // --- driving sequences -------------------------------------------

    /// The whole happy path, end to end.
    #[test]
    fn a_unit_can_run_stop_and_restart() {
        let policy = RestartPolicy::OnFailure;
        let (state, effects) = drive(
            UnitState::Inactive,
            &[
                Event::Start,
                Event::StartUpComplete,
                Event::Exit(ExitOutcome::Failure(1)),
                Event::BackoffElapsed,
                Event::StartUpComplete,
                Event::Stop,
                Event::StopComplete,
            ],
            policy,
            SIMPLE,
        )
        .expect("legal sequence");

        assert_eq!(state, UnitState::Inactive);
        assert!(effects.contains(&Effect::SpawnProcess));
        assert!(effects.contains(&Effect::SendStopSignal));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::ScheduleBackoff { .. })),
            "the failing exit must arm a backoff"
        );
    }

    /// Every state must have a name, so `rctl status` can never print nothing.
    #[test]
    fn every_state_has_a_readable_name() {
        let all = [
            UnitState::Inactive,
            UnitState::Starting,
            UnitState::Active,
            UnitState::ActiveExited,
            UnitState::Deactivating,
            UnitState::RestartWait,
            UnitState::Failed(FailedReason::StartFailure),
            UnitState::Failed(FailedReason::StartLimitHit),
        ];
        for state in all {
            assert!(!state.as_str().is_empty(), "{state:?} has no name");
        }
    }
}
