//! Job queue and target-state merging (plan §1.3(11), §1.3(13)).
//!
//! A Job is one requested *change* of a unit's target state. The queue exists
//! to separate two things that would otherwise tangle:
//!
//! - **target state** - what someone asked for (`Started` / `Stopped`);
//! - **actual state** - where the unit actually is ([`UnitState`]).
//!
//! The difference between them is what produces action (plan §1.3(13)), and
//! keeping the request separate from reality is what stops two sources racing:
//! a dependency pulling a unit in while `rctl stop` pulls it down must produce
//! one decision, not two.
//!
//! The merge rule is **priority first, timestamp second** (plan §1.3(11)):
//! `shutdown stop > user stop > user start > dependency start`. That ordering
//! is the whole point - taking the newest request instead would let a late
//! `start` resurrect a unit during shutdown.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::UnitName;
use super::state::{Effect, FailedReason, UnitState};

/// A monotonic timestamp in milliseconds since rinit started.
///
/// Passed in rather than read from a clock so the whole module stays pure and
/// therefore testable; the caller supplies `monotonic_now()`.
pub type Millis = u64;

/// Where a request came from, which decides who wins a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RequestSource {
    /// A dependency pulled the unit in (`Requires=`/`Wants=`). Lowest.
    Dependency,
    /// A user asked (`rctl start`). Outranks dependencies.
    UserStart,
    /// A user asked (`rctl stop`). Outranks a user start.
    UserStop,
    /// Shutdown. Outranks everything - a unit must not come back up while the
    /// system is going down.
    ShutdownStop,
}

/// What a unit is being asked to become.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobTarget {
    /// The unit should be running.
    Started,
    /// The unit should not be running.
    Stopped,
}

/// One requested target-state change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// The unit this job is about.
    pub unit: UnitName,
    /// The desired end state.
    pub target: JobTarget,
    /// Who asked, and therefore who wins a conflict.
    pub source: RequestSource,
    /// When it was requested.
    pub at: Millis,
}

impl Job {
    /// Build a job.
    pub fn new(unit: UnitName, target: JobTarget, source: RequestSource, at: Millis) -> Self {
        Job {
            unit,
            target,
            source,
            at,
        }
    }

    /// Whether this job should replace `other` for the same unit.
    ///
    /// Plan §1.3(11): "优先级优先于时间戳" - a higher-priority source wins even
    /// if the other request is newer. Only within one source does the newest
    /// request win.
    pub fn supersedes(&self, other: &Job) -> bool {
        match self.source.cmp(&other.source) {
            core::cmp::Ordering::Greater => true,
            core::cmp::Ordering::Less => false,
            // Same source: the later request wins, so an identical timestamp
            // is a no-op rather than a coin flip.
            core::cmp::Ordering::Equal => self.at > other.at,
        }
    }
}

/// The merged target state for one unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergedTarget {
    /// The state being asked for.
    pub target: JobTarget,
    /// The request that decided it.
    pub source: RequestSource,
    /// When that request arrived.
    ///
    /// Kept because the merge rule needs it: within one source the newest
    /// request wins (plan §1.3(11)), so without the timestamp a second
    /// `Started` from the same source could not be told from the first, and
    /// an older request would be able to overwrite a newer one.
    pub at: Millis,
}

/// Per-unit merged targets.
///
/// The merge keeps only the request that currently holds each unit's target.
/// A losing request has no future effect: the comparison is against the
/// current holder, so an older/lower-priority job that lost cannot become
/// relevant later. Keeping a separate "pending jobs" vector would therefore
/// grow without bound while changing no decision.
#[derive(Debug, Clone, Default)]
pub struct JobQueue {
    merged: Vec<(UnitName, MergedTarget)>,
}

impl JobQueue {
    /// An empty queue.
    pub const fn new() -> Self {
        JobQueue { merged: Vec::new() }
    }

    /// Number of units with an outstanding target.
    pub fn len(&self) -> usize {
        self.merged.len()
    }

    /// Whether no unit has an outstanding target.
    pub fn is_empty(&self) -> bool {
        self.merged.is_empty()
    }

    /// Submit a job, merging it into the unit's target state.
    ///
    /// Returns `true` if this request *won* the merge - i.e. it is now the one
    /// holding the unit's target. A losing request is not retained: the
    /// comparison is always against the current holder, so an older or
    /// lower-priority job cannot become relevant later.
    ///
    /// Note that winning does not imply the *value* changed: a second
    /// `Started` from a later user request wins the comparison but leaves the
    /// target at `Started`. Callers care about authority, not change, because
    /// authority is what a later `ShutdownStop` has to override.
    pub fn submit(&mut self, job: Job) -> bool {
        match self.merged.iter_mut().find(|(u, _)| *u == job.unit) {
            Some((_, current)) => {
                // Compare against the request that currently holds the target,
                // not against the target itself: two `Started` jobs from
                // different sources still have to pick a winner so that a
                // later `ShutdownStop` knows what it is overriding.
                let holder = Job::new(job.unit.clone(), current.target, current.source, current.at);
                if job.supersedes(&holder) {
                    *current = MergedTarget {
                        target: job.target,
                        source: job.source,
                        at: job.at,
                    };
                    true
                } else {
                    false
                }
            }
            None => {
                self.merged.push((
                    job.unit.clone(),
                    MergedTarget {
                        target: job.target,
                        source: job.source,
                        at: job.at,
                    },
                ));
                true
            }
        }
    }

    /// The merged target for `unit`, if one was requested.
    pub fn target_of(&self, unit: &UnitName) -> Option<MergedTarget> {
        self.merged.iter().find(|(u, _)| u == unit).map(|(_, t)| *t)
    }

    /// All merged targets, sorted by unit for determinism.
    pub fn targets(&self) -> Vec<(UnitName, MergedTarget)> {
        let mut out = self.merged.clone();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Drop every job for `unit` and clear its target.
    ///
    /// Used when a unit is reaped, so a finished unit does not keep a stale
    /// `Started` target that would re-spawn it on the next reconcile.
    pub fn forget(&mut self, unit: &UnitName) {
        self.merged.retain(|(u, _)| u != unit);
    }

    /// Decide what to do about `unit` right now, given where it actually is.
    ///
    /// This is plan §1.3(13)'s action-derivation rule: the difference between
    /// target and actual state yields an [`Effect`], or `None` when they
    /// already agree.
    ///
    /// Returns `None` when the unit has no target at all - nothing was asked,
    /// so nothing should happen.
    pub fn reconcile(&self, unit: &UnitName, actual: UnitState) -> Option<Effect> {
        let merged = self.target_of(unit)?;

        // This is the *action* half of plan §1.3(13): target versus actual
        // yields an effect. The resulting state is owned by the state machine
        // and the manager; returning a second copy of it here was how the two
        // started to drift.
        match merged.target {
            JobTarget::Started => match actual {
                UnitState::Active | UnitState::ActiveExited => Some(Effect::None),
                UnitState::RestartWait => Some(Effect::CancelBackoffAndStart),
                UnitState::Failed(_) => Some(Effect::ClearStartLimitAndStart),
                UnitState::Deactivating => None,
                UnitState::Inactive => Some(Effect::SpawnProcess),
                UnitState::Starting => Some(Effect::None),
            },

            JobTarget::Stopped => match actual {
                UnitState::Inactive | UnitState::ActiveExited | UnitState::Failed(_) => {
                    Some(Effect::None)
                }
                UnitState::RestartWait => Some(Effect::CancelBackoffAndStop),
                UnitState::Active | UnitState::Starting => Some(Effect::SendStopSignal),
                UnitState::Deactivating => None,
            },
        }
    }
}

/// The name `rctl` prints for a failed unit, including the reason.
pub fn describe_failure(state: UnitState) -> String {
    match state {
        UnitState::Failed(FailedReason::StartLimitHit) => "failed (start-limit hit)".to_string(),
        UnitState::Failed(FailedReason::StartFailure) => "failed".to_string(),
        other => other.as_str().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> UnitName {
        UnitName::parse(s).expect("valid name")
    }

    #[test]
    fn priority_outranks_timestamp() {
        // A shutdown stop arrives first, a user start second. Plan §1.3(11):
        // priority wins, so the unit stays stopped even though start is newer.
        let stop = Job::new(
            name("a.service"),
            JobTarget::Stopped,
            RequestSource::ShutdownStop,
            10,
        );
        let start = Job::new(
            name("a.service"),
            JobTarget::Started,
            RequestSource::UserStart,
            20,
        );

        let mut q = JobQueue::new();
        q.submit(stop);
        q.submit(start);

        assert_eq!(
            q.target_of(&name("a.service")).map(|t| t.target),
            Some(JobTarget::Stopped),
            "shutdown must not be undone by a later user start"
        );
    }

    #[test]
    fn the_full_priority_order_holds() {
        let order = [
            RequestSource::ShutdownStop,
            RequestSource::UserStop,
            RequestSource::UserStart,
            RequestSource::Dependency,
        ];
        for window in order.windows(2) {
            let (higher, lower) = (window[0], window[1]);
            let mut q = JobQueue::new();
            // The lower-priority request is *newer* and submitted first.
            q.submit(Job::new(name("a.service"), JobTarget::Started, lower, 100));
            q.submit(Job::new(name("a.service"), JobTarget::Stopped, higher, 1));
            assert_eq!(
                q.target_of(&name("a.service")).map(|t| t.target),
                Some(JobTarget::Stopped),
                "{higher:?} must beat the newer {lower:?}"
            );
        }
    }

    #[test]
    fn within_one_source_the_newest_request_wins() {
        let mut q = JobQueue::new();
        q.submit(Job::new(
            name("a.service"),
            JobTarget::Started,
            RequestSource::UserStart,
            1,
        ));
        // Same source, newer: a later stop wins.
        q.submit(Job::new(
            name("a.service"),
            JobTarget::Stopped,
            RequestSource::UserStart,
            2,
        ));
        assert_eq!(
            q.target_of(&name("a.service")).map(|t| t.target),
            Some(JobTarget::Stopped)
        );

        // And an *older* request from the same source does not undo it.
        q.submit(Job::new(
            name("a.service"),
            JobTarget::Started,
            RequestSource::UserStart,
            1,
        ));
        assert_eq!(
            q.target_of(&name("a.service")).map(|t| t.target),
            Some(JobTarget::Stopped)
        );
    }

    /// A unit pulled in by several dependencies is started once, not once per
    /// reference - that is the whole point of merging.
    #[test]
    fn duplicate_dependency_requests_merge_into_one() {
        let mut q = JobQueue::new();
        for at in 1..=5 {
            assert!(q.submit(Job::new(
                name("db.service"),
                JobTarget::Started,
                RequestSource::Dependency,
                at,
            )));
        }
        assert_eq!(
            q.target_of(&name("db.service")).map(|t| t.target),
            Some(JobTarget::Started)
        );
    }

    /// `submit` reports whether this request became authoritative, not whether
    /// the value moved: a newer `Started` after a `Started` wins the comparison
    /// but leaves the target where it was.
    #[test]
    fn submit_reports_whether_this_request_won() {
        let mut q = JobQueue::new();
        assert!(q.submit(Job::new(
            name("a.service"),
            JobTarget::Started,
            RequestSource::UserStart,
            1,
        )));
        // Same source, newer: wins, and the target stays Started.
        assert!(q.submit(Job::new(
            name("a.service"),
            JobTarget::Started,
            RequestSource::UserStart,
            2,
        )));
        assert_eq!(
            q.target_of(&name("a.service")).map(|t| t.target),
            Some(JobTarget::Started)
        );
        // An *older* request from the same source loses the comparison.
        assert!(!q.submit(Job::new(
            name("a.service"),
            JobTarget::Stopped,
            RequestSource::UserStart,
            1,
        )));
        // A higher-priority request wins regardless of timestamp.
        assert!(q.submit(Job::new(
            name("a.service"),
            JobTarget::Stopped,
            RequestSource::ShutdownStop,
            0,
        )));
    }

    // --- reconciliation (plan §1.3(13)) --------------------------------

    /// A unit with no target is left alone; this is how a unit nobody asked
    /// for stays down.
    #[test]
    fn no_target_means_no_action() {
        let q = JobQueue::new();
        assert_eq!(q.reconcile(&name("a.service"), UnitState::Inactive), None);
    }

    #[test]
    fn start_spawns_an_inactive_unit() {
        let mut q = JobQueue::new();
        q.submit(Job::new(
            name("a.service"),
            JobTarget::Started,
            RequestSource::Dependency,
            0,
        ));
        assert_eq!(
            q.reconcile(&name("a.service"), UnitState::Inactive),
            Some(Effect::SpawnProcess)
        );
    }

    /// A start against an in-progress start waits for it; it does not spawn a
    /// second process or re-run readiness bookkeeping.
    #[test]
    fn start_against_a_starting_unit_waits() {
        let mut q = JobQueue::new();
        q.submit(Job::new(
            name("a.service"),
            JobTarget::Started,
            RequestSource::UserStart,
            0,
        ));
        assert_eq!(
            q.reconcile(&name("a.service"), UnitState::Starting),
            Some(Effect::None)
        );
    }

    /// Plan §1.3(13): a start against an already-active unit is satisfied -
    /// including `Active(Exited)`, so a completed oneshot is never re-run.
    #[test]
    fn start_against_an_active_unit_is_satisfied() {
        let mut q = JobQueue::new();
        q.submit(Job::new(
            name("a.service"),
            JobTarget::Started,
            RequestSource::Dependency,
            0,
        ));
        for state in [UnitState::Active, UnitState::ActiveExited] {
            assert_eq!(
                q.reconcile(&name("a.service"), state),
                Some(Effect::None),
                "{state} is already running"
            );
        }
    }

    /// A manual start cancels a pending backoff instead of waiting for it.
    #[test]
    fn start_cancels_a_pending_backoff() {
        let mut q = JobQueue::new();
        q.submit(Job::new(
            name("a.service"),
            JobTarget::Started,
            RequestSource::UserStart,
            0,
        ));
        assert_eq!(
            q.reconcile(&name("a.service"), UnitState::RestartWait),
            Some(Effect::CancelBackoffAndStart)
        );
    }

    /// Plan §1.3(13): starting a failed unit clears the breaker, which is what
    /// makes `rctl start` an escape hatch from a tripped limit.
    #[test]
    fn start_clears_a_tripped_breaker() {
        let mut q = JobQueue::new();
        q.submit(Job::new(
            name("a.service"),
            JobTarget::Started,
            RequestSource::UserStart,
            0,
        ));
        for reason in [FailedReason::StartFailure, FailedReason::StartLimitHit] {
            assert_eq!(
                q.reconcile(&name("a.service"), UnitState::Failed(reason)),
                Some(Effect::ClearStartLimitAndStart),
                "{reason:?}"
            );
        }
    }

    /// A stop in progress is not cancelled by a start; the start waits.
    #[test]
    fn start_during_deactivation_waits_rather_than_racing() {
        let mut q = JobQueue::new();
        q.submit(Job::new(
            name("a.service"),
            JobTarget::Started,
            RequestSource::UserStart,
            0,
        ));
        assert_eq!(
            q.reconcile(&name("a.service"), UnitState::Deactivating),
            None,
            "the queued start waits for the stop to finish"
        );
    }

    #[test]
    fn stop_signals_a_running_unit() {
        let mut q = JobQueue::new();
        q.submit(Job::new(
            name("a.service"),
            JobTarget::Stopped,
            RequestSource::UserStop,
            0,
        ));
        for state in [UnitState::Active, UnitState::Starting] {
            assert_eq!(
                q.reconcile(&name("a.service"), state),
                Some(Effect::SendStopSignal)
            );
        }
    }

    /// Plan §1.3(3): stopping a completed oneshot skips `Deactivating`.
    #[test]
    fn stop_skips_deactivating_for_an_oneshot() {
        let mut q = JobQueue::new();
        q.submit(Job::new(
            name("a.service"),
            JobTarget::Stopped,
            RequestSource::UserStop,
            0,
        ));
        assert_eq!(
            q.reconcile(&name("a.service"), UnitState::ActiveExited),
            Some(Effect::None)
        );
    }

    /// A stop beats a pending restart.
    #[test]
    fn stop_cancels_a_pending_restart() {
        let mut q = JobQueue::new();
        q.submit(Job::new(
            name("a.service"),
            JobTarget::Stopped,
            RequestSource::UserStop,
            0,
        ));
        assert_eq!(
            q.reconcile(&name("a.service"), UnitState::RestartWait),
            Some(Effect::CancelBackoffAndStop)
        );
    }

    #[test]
    fn shutdown_stop_cancels_a_pending_restart_too() {
        let mut q = JobQueue::new();
        q.submit(Job::new(
            name("a.service"),
            JobTarget::Stopped,
            RequestSource::ShutdownStop,
            0,
        ));
        assert_eq!(
            q.reconcile(&name("a.service"), UnitState::RestartWait),
            Some(Effect::CancelBackoffAndStop)
        );
    }

    // --- bookkeeping ---------------------------------------------------

    #[test]
    fn forgetting_a_unit_clears_its_jobs() {
        let mut q = JobQueue::new();
        q.submit(Job::new(
            name("a.service"),
            JobTarget::Started,
            RequestSource::UserStart,
            0,
        ));
        assert!(!q.is_empty());
        q.forget(&name("a.service"));
        assert!(q.is_empty());
        assert_eq!(q.target_of(&name("a.service")), None);
    }

    #[test]
    fn targets_are_listed_deterministically() {
        let mut q = JobQueue::new();
        for n in ["c.service", "a.service", "b.service"] {
            q.submit(Job::new(
                name(n),
                JobTarget::Started,
                RequestSource::Dependency,
                0,
            ));
        }
        let targets = q.targets();
        let listed: Vec<&str> = targets.iter().map(|(u, _)| u.canonical()).collect();
        assert_eq!(listed, ["a.service", "b.service", "c.service"]);
    }

    /// A dependent whose required unit failed must not be started.
    #[test]
    fn a_losing_start_does_not_replace_the_target() {
        let mut q = JobQueue::new();
        q.submit(Job::new(
            name("a.service"),
            JobTarget::Stopped,
            RequestSource::ShutdownStop,
            0,
        ));
        q.submit(Job::new(
            name("a.service"),
            JobTarget::Started,
            RequestSource::Dependency,
            1,
        ));
        assert_eq!(q.len(), 1, "only the authoritative target is kept");
        assert_eq!(
            q.target_of(&name("a.service")).map(|t| t.target),
            Some(JobTarget::Stopped)
        );
    }
}

#[cfg(test)]
mod proptests {
    //! Plan §4.4 asks for a property test over the Job queue: any sequence of
    //! requests must converge on one consistent target, with no dependence on
    //! the order they were submitted in beyond the documented rules.

    use proptest::prelude::*;

    use super::*;

    fn name(s: &str) -> UnitName {
        UnitName::parse(s).expect("valid name")
    }

    fn from_code(code: u8) -> RequestSource {
        match code % 4 {
            0 => RequestSource::Dependency,
            1 => RequestSource::UserStart,
            2 => RequestSource::UserStop,
            _ => RequestSource::ShutdownStop,
        }
    }

    proptest! {
        /// The merged target is always the highest-priority request, ties going
        /// to the earlier one - exactly what `Job::supersedes` defines.
        #[test]
        fn the_merged_target_is_the_highest_priority_request(
            requests in proptest::collection::vec((0u8..4, any::<u64>(), any::<bool>()), 0..40)
        ) {
            let mut q = JobQueue::new();
            let mut winner: Option<(RequestSource, u64, JobTarget)> = None;

            for (source, at, started) in requests {
                let source = from_code(source);
                let target = if started { JobTarget::Started } else { JobTarget::Stopped };
                q.submit(Job::new(name("a.service"), target, source, at));

                let candidate = (source, at, target);
                winner = Some(match winner {
                    // Strictly greater: an equal (source, at) keeps the earlier
                    // request, matching `supersedes` returning false on a tie.
                    Some(best) if best.0 > candidate.0 || (best.0 == candidate.0 && best.1 >= candidate.1) => best,
                    _ => candidate,
                });
            }

            let merged = q.target_of(&name("a.service"));
            let actual = merged.map(|m| (m.source, m.at, m.target));
            prop_assert_eq!(actual, winner);
        }

        /// Submitting a request never leaves the queue without a target, and
        /// the target is always one of the two values - a convergence check
        /// that also guards against a panic on any input.
        #[test]
        fn a_request_always_yields_a_target(
            requests in proptest::collection::vec((0u8..4, any::<u64>(), any::<bool>()), 1..20)
        ) {
            let mut q = JobQueue::new();
            for (source, at, started) in requests {
                let target = if started { JobTarget::Started } else { JobTarget::Stopped };
                q.submit(Job::new(name("a.service"), target, from_code(source), at));
            }
            prop_assert!(q.target_of(&name("a.service")).is_some());
        }
    }
}
