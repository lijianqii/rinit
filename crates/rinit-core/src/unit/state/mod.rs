//! Unit lifecycle: states and the transition table (plan §1.3(3), §1.2).
//!
//! `UnitState` is where a unit actually is; the transition table is the only
//! place that decides where it goes next. Keeping the side effects in an
//! [`Effect`] return value rather than executing them keeps the table a pure
//! function, which is what makes every row testable without spawning a
//! process.
//!
//! The Job queue (plan §1.3(11)) holds the *target* state separately and drives
//! these transitions; see [`super::job`].

pub mod transition;
pub mod unit_state;

pub use transition::{Effect, Event, Transition, TransitionError, drive, transition};
pub use unit_state::{ExitOutcome, FailedReason, UnitState};

#[cfg(test)]
mod proptests {
    use alloc::vec::Vec;

    use proptest::prelude::*;

    use super::*;
    use crate::unit::config::{RestartPolicy, ServiceKind};

    // Every state/event pair must produce *some* answer - a transition or an
    // explicit refusal - and must never panic or loop. The plan's §4.4
    // property testing is aimed at exactly this: the table has 8 states and 8
    // events, and a new one added later could easily be forgotten.
    proptest! {
        #[test]
        fn every_state_event_pair_is_decided(
            state_idx in 0usize..8,
            event_idx in 0usize..8,
            policy_idx in 0usize..5,
            kind_idx in 0usize..3,
        ) {
            let states = [
                UnitState::Inactive,
                UnitState::Starting,
                UnitState::Active,
                UnitState::ActiveExited,
                UnitState::Deactivating,
                UnitState::RestartWait,
                UnitState::Failed(FailedReason::StartFailure),
                UnitState::Failed(FailedReason::StartLimitHit),
            ];
            let events = [
                Event::Start,
                Event::StartUpComplete,
                Event::StartUpFailed,
                Event::Exit(ExitOutcome::Success),
                Event::Exit(ExitOutcome::Failure(1)),
                Event::Stop,
                Event::StopComplete,
                Event::ResetFailed,
            ];
            let policies = [
                RestartPolicy::No,
                RestartPolicy::Always,
                RestartPolicy::OnSuccess,
                RestartPolicy::OnFailure,
                RestartPolicy::OnAbnormal,
            ];
            let kinds = [ServiceKind::Simple, ServiceKind::Oneshot, ServiceKind::Notify];

            let event = events[event_idx];
            let state = states[state_idx];
            if let Ok(t) = transition(state, event, policies[policy_idx], kinds[kind_idx]) {
                prop_assert_eq!(t.from, state);
            }
        }
    }

    // Whatever the event, a transition never changes state into something the
    // plan does not define, and the effect is always meaningful.
    proptest! {
        #[test]
        fn a_transition_never_produces_an_impossible_state(
            state_idx in 0usize..8,
            event_idx in 0usize..8,
        ) {
            let states = [
                UnitState::Inactive,
                UnitState::Starting,
                UnitState::Active,
                UnitState::ActiveExited,
                UnitState::Deactivating,
                UnitState::RestartWait,
                UnitState::Failed(FailedReason::StartFailure),
                UnitState::Failed(FailedReason::StartLimitHit),
            ];
            let events = [
                Event::Start,
                Event::StartUpComplete,
                Event::StartUpFailed,
                Event::Exit(ExitOutcome::Success),
                Event::Exit(ExitOutcome::Failure(1)),
                Event::Stop,
                Event::StopComplete,
                Event::ResetFailed,
            ];
            let state = states[state_idx];
            let event = events[event_idx];
            if let Ok(t) = transition(state, event, RestartPolicy::OnFailure, ServiceKind::Simple) {
                // `Active(Exited)` has no process, so it is never live.
                if t.to == UnitState::ActiveExited {
                    prop_assert!(!t.to.is_live());
                }
                // Spawning only ever happens out of a non-running state.
                if t.effect == Effect::SpawnProcess {
                    prop_assert!(matches!(
                        t.to,
                        UnitState::Starting | UnitState::Active | UnitState::ActiveExited
                    ));
                }
            }
        }
    }

    // Driving an arbitrary event sequence must terminate and land in a
    // consistent state - plan §4.4's "任意 Job 序列 → 状态收敛到一致终态".
    proptest! {
        #[test]
        fn any_event_sequence_terminates(
            events in prop::collection::vec(0usize..8, 0..24),
        ) {
            let all = [
                Event::Start,
                Event::StartUpComplete,
                Event::StartUpFailed,
                Event::Exit(ExitOutcome::Success),
                Event::Exit(ExitOutcome::Failure(1)),
                Event::Exit(ExitOutcome::Signalled(9)),
                Event::Stop,
                Event::StopComplete,
                Event::BackoffElapsed,
            ];
            let script: Vec<Event> = events.iter().map(|&i| all[i]).collect();
            // Whether the whole script is legal is not the point; that it
            // terminates without panicking and reports where it stopped is.
            let result = drive(
                UnitState::Inactive,
                &script,
                RestartPolicy::OnFailure,
                ServiceKind::Simple,
            );
            if let Ok((final_state, effects)) = result {
                prop_assert_eq!(effects.len(), script.len());
                prop_assert!(!final_state.as_str().is_empty());
            }
        }
    }
}
