//! Unit manager: drives unit state from real events (plan §3 M2).
//!
//! The pure decision-making already exists in `rinit-core`: the transition
//! table decides where a unit goes, the Job queue decides what was asked for,
//! and the backoff engine decides whether a failure is retried. This module is
//! only the part that has to touch the outside world - spawn, signal, reap -
//! and it holds the state those three need kept between events.
//!
//! Keeping the split sharp is what makes the interesting half testable: every
//! rule about *when* something should happen lives in `rinit-core` and is
//! covered by the 200 tests there, while this file is the thin layer that acts
//! on their answers.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use rinit_core::unit::config::{RestartPolicy, ServiceKind};
use rinit_core::unit::dependency::spawns_process;
use rinit_core::unit::restart::BackoffState;
use rinit_core::{Event, ExitOutcome, FailedReason, UnitState, transition};
use rinit_core::{UnitConfig, UnitName};

use rinit_runtime::RuntimeError;
use rinit_runtime::syscall::{process, wait};

use crate::log::rlog;
use crate::service::spawn_service_in_group;

/// What happened to a unit when a child was reaped.
///
/// The manager applies the change itself - it owns the state, the backoff and
/// the restart timer - so this is a *report* for logging and tests, not an
/// instruction. An earlier version returned an `Action` for the caller to
/// perform, which meant the manager had to describe work it had already done,
/// and the variants drifted out of use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A process exited and no restart was due.
    Exited {
        /// The unit it belonged to.
        unit: UnitName,
        /// Where it ended up.
        state: UnitState,
    },
    /// A restart was scheduled after the backoff delay.
    RestartScheduled {
        /// The unit that will be retried.
        unit: UnitName,
        /// How long to wait.
        delay: Duration,
    },
    /// The breaker tripped; the unit stays failed and will not retry.
    StartLimitTripped {
        /// The unit that was parked.
        unit: UnitName,
        /// How many failures fell inside the window.
        failures: usize,
    },
}

/// Everything rinit knows about one running unit.
#[derive(Debug)]
pub struct UnitRuntime {
    /// The unit's parsed configuration.
    pub config: UnitConfig,
    /// Where it actually is (plan §1.3(3)).
    pub state: UnitState,
    /// Restart bookkeeping (plan §1.3(4)).
    pub backoff: BackoffState,
    /// Its process, if one is running.
    pub pid: Option<libc::pid_t>,
    /// When a pending restart is due.
    pub restart_at: Option<Instant>,
    /// The last outcome, kept for logging and `rctl status`.
    pub last_outcome: Option<ExitOutcome>,
}

impl UnitRuntime {
    fn new(config: UnitConfig) -> Self {
        // Seeding the jitter from the unit's name keeps two services that fail
        // at the same moment from picking the same backoff and coming back up
        // in lockstep (plan §1.3(4)).
        let seed = fnv1a(config.name.canonical().as_bytes());
        let backoff = BackoffState::new(
            config.restart_delay(),
            config.start_limit_burst as u32,
            Duration::from_secs(config.start_limit_interval_sec),
            seed,
        );
        UnitRuntime {
            config,
            state: UnitState::Inactive,
            backoff,
            pid: None,
            restart_at: None,
            last_outcome: None,
        }
    }

    /// Whether this unit should get a process at all.
    fn spawns(&self) -> bool {
        spawns_process(&self.config)
    }

    /// The effective restart policy.
    fn policy(&self) -> RestartPolicy {
        self.config.restart
    }
}

/// A stable 64-bit hash of the unit name, for the jitter seed.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// Tracks every unit and turns events into state changes and [`Outcome`]s.
#[derive(Debug)]
pub struct UnitManager {
    units: BTreeMap<UnitName, UnitRuntime>,
    /// Monotonic origin for the Job queue's timestamps.
    origin: Instant,
}

impl UnitManager {
    /// An empty manager.
    pub fn new() -> Self {
        UnitManager {
            units: BTreeMap::new(),
            origin: Instant::now(),
        }
    }

    /// Milliseconds since the manager was created.
    pub fn now_millis(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }

    /// Register a unit, in `Inactive`.
    pub fn add(&mut self, config: UnitConfig) {
        let name = config.name.clone();
        self.units.insert(name, UnitRuntime::new(config));
    }

    /// Read a unit.
    pub fn get(&self, unit: &UnitName) -> Option<&UnitRuntime> {
        self.units.get(unit)
    }

    /// Mark a `oneshot` as having completed successfully.
    ///
    /// The boot sequence waits for these with a direct `waitpid` (it cannot use
    /// the main loop, which has not started yet), so the manager needs to be
    /// told the outcome instead of deriving it.
    pub fn mark_oneshot_done(&mut self, unit: &UnitName) {
        let now = self.now_millis();
        if let Some(rt) = self.units.get_mut(unit) {
            rt.pid = None;
            rt.state = UnitState::ActiveExited;
            rt.backoff.on_active(now);
        }
    }

    /// How many units currently have a process.
    pub fn running_count(&self) -> usize {
        self.units.values().filter(|rt| rt.pid.is_some()).count()
    }

    /// Every unit with a live process, in name order.
    pub fn units_to_stop(&self) -> Vec<UnitName> {
        self.units
            .values()
            .filter(|rt| rt.pid.is_some())
            .map(|rt| rt.config.name.clone())
            .collect()
    }

    /// Time until the next restart is due, if any.
    pub fn next_restart_in(&self) -> Option<Duration> {
        let now = Instant::now();
        self.units
            .values()
            .filter_map(|rt| rt.restart_at)
            .map(|at| at.saturating_duration_since(now))
            .min()
    }

    /// Spawn a unit and mark it `Starting`.
    ///
    /// # Errors
    /// Propagates a spawn failure; the caller decides whether that is fatal.
    pub fn spawn_unit(&mut self, unit: &UnitName) -> Result<(), RuntimeError> {
        let now = self.now_millis();
        let Some(rt) = self.units.get_mut(unit) else {
            return Ok(());
        };
        if rt.pid.is_some() {
            return Ok(());
        }
        // Targets are process-less; starting one is a bookkeeping no-op.
        if !rt.spawns() {
            rt.state = UnitState::Active;
            return Ok(());
        }

        let argv = rt.config.exec_start.argv().to_vec();
        // A fresh process group, so a stop reaches the whole tree (plan §1.3(6)).
        let pid = spawn_service_in_group(&argv)?;
        rt.pid = Some(pid);
        rt.state = UnitState::Starting;
        rt.restart_at = None;

        // `simple` needs no readiness signal; `oneshot` waits for exit; `notify`
        // waits for READY=1, which M2 models as "exec'd" until the notify
        // socket lands.
        if matches!(rt.config.service_kind, ServiceKind::Simple) {
            rt.state = UnitState::Active;
            rt.backoff.on_active(now);
        }
        rlog!("started {unit} as pid {pid}");
        Ok(())
    }

    /// Signal a unit's process group to stop.
    ///
    /// A pending restart is cancelled: an explicit stop beats an automatic one
    /// (plan §1.3(13)).
    pub fn stop_unit(&mut self, unit: &UnitName) {
        let Some(rt) = self.units.get_mut(unit) else {
            return;
        };
        rt.restart_at = None;

        match rt.pid {
            Some(pid) => match process::kill_process_group(pid, libc::SIGTERM) {
                Ok(()) => {
                    rt.state = UnitState::Deactivating;
                    rlog!("stopping {unit} (pgid {pid})");
                }
                Err(e) if e.errno() == libc::ESRCH => {
                    // Already gone: nothing to signal, so the stop completes
                    // now rather than waiting for a SIGCHLD that has already
                    // been consumed.
                    rt.pid = None;
                    rt.state = UnitState::Inactive;
                }
                Err(e) => rlog!("warning: SIGTERM to {unit} failed: {e}"),
            },
            // No process: an `Active(Exited)` oneshot and anything already
            // down both land straight in Inactive (plan §1.3(3) skips
            // Deactivating when there is nothing to signal).
            None => {
                rt.state = UnitState::Inactive;
            }
        }
    }

    /// Feed a reaped child back in and apply whatever follows.
    ///
    /// Returns what happened, for logging and tests. This is where `Restart=`,
    /// the backoff and the breaker turn into real behaviour (plan §3 M2's
    /// acceptance criterion: "kill 掉服务进程,观察自动重启与熔断").
    ///
    /// Returns `None` for a pid rinit does not own, which is not an error: a
    /// child reaped by someone else, or one already accounted for.
    pub fn on_child_exit(&mut self, pid: libc::pid_t, outcome: ExitOutcome) -> Option<Outcome> {
        let unit = self
            .units
            .iter()
            .find(|(_, rt)| rt.pid == Some(pid))
            .map(|(name, _)| name.clone())?;

        let now = self.now_millis();
        let rt = self.units.get_mut(&unit)?;
        rt.pid = None;
        rt.last_outcome = Some(outcome);

        // A stop already in flight owns the transition; the exit just confirms
        // it, and must not be mistaken for a crash worth restarting.
        if rt.state == UnitState::Deactivating {
            rt.state = UnitState::Inactive;
            return Some(Outcome::Exited {
                unit,
                state: UnitState::Inactive,
            });
        }

        // A `oneshot` that was still starting has not "exited" in the sense
        // the transition table models: its exit *is* its start-up completion.
        // `Exit` is not a legal event from `Starting`, so which event applies
        // depends on where the unit actually was (plan §1.3(3)).
        let starting = rt.state == UnitState::Starting;
        let oneshot = rt.config.service_kind == ServiceKind::Oneshot;

        let event = if oneshot && starting {
            match outcome {
                ExitOutcome::Success => Event::StartUpComplete,
                _ => Event::StartUpFailed,
            }
        } else {
            Event::Exit(outcome)
        };

        match transition(rt.state, event, rt.policy(), rt.config.service_kind) {
            Ok(t) => rt.state = t.to,
            Err(_) => {
                // The exit makes no sense here - already stopped, or reaped
                // twice. Land on Inactive rather than guessing a state the
                // unit could never legitimately reach.
                rt.state = UnitState::Inactive;
                return Some(Outcome::Exited {
                    unit,
                    state: UnitState::Inactive,
                });
            }
        }

        // A oneshot that succeeded is done, not broken: `StartUpComplete`
        // already moved it to Active(Exited).
        if oneshot && matches!(outcome, ExitOutcome::Success) {
            return Some(Outcome::Exited {
                unit,
                state: UnitState::ActiveExited,
            });
        }

        if !rt.policy().wants_restart(outcome) {
            let state = rt.state;
            return Some(Outcome::Exited { unit, state });
        }

        // Restart wanted. Ask the breaker whether it is allowed.
        match rt.backoff.on_failure(now) {
            Some(delay) => {
                rt.state = UnitState::RestartWait;
                rt.restart_at = Some(Instant::now() + delay);
                Some(Outcome::RestartScheduled { unit, delay })
            }
            None => {
                rt.state = UnitState::Failed(FailedReason::StartLimitHit);
                let failures = rt.backoff.failures_in_window();
                Some(Outcome::StartLimitTripped { unit, failures })
            }
        }
    }

    /// Fire any restart whose backoff has elapsed.
    ///
    /// Returns the units due for a restart right now.
    pub fn due_restarts(&mut self) -> Vec<UnitName> {
        let now = Instant::now();
        let mut due = Vec::new();
        for (name, rt) in self.units.iter_mut() {
            if let Some(at) = rt.restart_at
                && at <= now
            {
                rt.restart_at = None;
                due.push(name.clone());
            }
        }
        due
    }

    /// A human-readable summary line per unit, for the boot log and `rctl`.
    pub fn summary(&self) -> Vec<String> {
        self.units
            .values()
            .map(|rt| {
                let pid = match rt.pid {
                    Some(p) => format!(" pid {p}"),
                    None => String::new(),
                };
                format!("{}: {}{pid}", rt.config.name, rt.state)
            })
            .collect()
    }
}

impl Default for UnitManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Reap every exited child and feed the results to the manager.
///
/// Returns what each exit meant, in reap order. A pid rinit does not own is
/// skipped: it was reaped by someone else, or already accounted for.
pub fn reap_into_manager(manager: &mut UnitManager) -> Vec<Outcome> {
    let mut out = Vec::new();
    for ev in wait::reap_children() {
        let (pid, outcome) = match ev {
            wait::ChildExit::Exited { pid, code } => (
                pid,
                if code == 0 {
                    ExitOutcome::Success
                } else {
                    ExitOutcome::Failure(code)
                },
            ),
            wait::ChildExit::Signaled { pid, signal } => (pid, ExitOutcome::Signalled(signal)),
        };
        if let Some(result) = manager.on_child_exit(pid, outcome) {
            rlog!("reaped pid {pid}");
            out.push(result);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(name: &str, restart: RestartPolicy) -> UnitConfig {
        let mut cfg = UnitConfig::new(UnitName::parse(name).unwrap());
        cfg.exec_start =
            rinit_core::ExecCommand::new(vec!["/bin/sleep".to_string(), "30".to_string()]);
        cfg.restart = restart;
        // Keep the backoff short so tests do not actually wait.
        cfg.restart_sec = 0;
        cfg.start_limit_interval_sec = 10;
        cfg.start_limit_burst = 5;
        cfg
    }

    fn manager_with(name: &str, restart: RestartPolicy) -> UnitManager {
        let mut m = UnitManager::new();
        m.add(config(name, restart));
        m
    }

    #[test]
    fn a_new_unit_is_inactive_and_untracked() {
        let m = manager_with("a.service", RestartPolicy::No);
        let rt = m.get(&UnitName::parse("a.service").unwrap()).unwrap();
        assert_eq!(rt.state, UnitState::Inactive);
        assert!(rt.pid.is_none());
        assert!(m.summary()[0].contains("inactive"));
    }

    /// Spawning must actually produce a process the manager knows about.
    #[test]
    fn spawning_a_unit_tracks_its_pid_and_becomes_active() {
        let mut m = manager_with("a.service", RestartPolicy::No);
        let unit = UnitName::parse("a.service").unwrap();
        m.spawn_unit(&unit).expect("spawn");
        let rt = m.get(&unit).unwrap();
        assert!(rt.pid.is_some(), "the pid must be tracked");
        assert_eq!(rt.state, UnitState::Active, "Type=simple is active at exec");

        // Clean up so the test leaves nothing running.
        m.stop_unit(&unit);
        if let Some(pid) = m.get(&unit).unwrap().pid {
            let _ = process::kill_process_group(pid, libc::SIGKILL);
        }
    }

    /// A target has no process but must still reach Active, or its dependents
    /// would wait forever.
    #[test]
    fn starting_a_target_reaches_active_without_a_process() {
        let mut cfg = config("t.target", RestartPolicy::No);
        cfg.kind = rinit_core::UnitType::Target;
        cfg.exec_start = rinit_core::ExecCommand::empty();
        let mut m = UnitManager::new();
        m.add(cfg);
        let unit = UnitName::parse("t.target").unwrap();
        m.spawn_unit(&unit).expect("start");
        let rt = m.get(&unit).unwrap();
        assert_eq!(rt.state, UnitState::Active);
        assert!(rt.pid.is_none(), "a target must not spawn anything");
    }

    /// The core of M2's acceptance: a killed service is restarted.
    #[test]
    fn a_killed_service_schedules_a_restart() {
        let mut m = manager_with("a.service", RestartPolicy::Always);
        let unit = UnitName::parse("a.service").unwrap();
        m.spawn_unit(&unit).expect("spawn");
        let pid = m.get(&unit).unwrap().pid.expect("pid");
        let _ = process::kill(pid, libc::SIGKILL);

        let outcome = m
            .on_child_exit(pid, ExitOutcome::Signalled(libc::SIGKILL))
            .expect("the exit belongs to a unit");
        assert!(
            matches!(outcome, Outcome::RestartScheduled { unit: ref u, .. } if *u == unit),
            "Restart=always must schedule a retry, got {outcome:?}"
        );
        assert_eq!(
            m.get(&unit).unwrap().state,
            UnitState::RestartWait,
            "the unit waits out the backoff"
        );
    }

    /// `Restart=no` means the unit stays down - the difference between a
    /// supervised service and a run-once task.
    #[test]
    fn without_restart_a_dead_service_stays_down() {
        let mut m = manager_with("a.service", RestartPolicy::No);
        let unit = UnitName::parse("a.service").unwrap();
        m.spawn_unit(&unit).expect("spawn");
        let pid = m.get(&unit).unwrap().pid.expect("pid");

        let outcome = m
            .on_child_exit(pid, ExitOutcome::Failure(1))
            .expect("handled");
        assert!(
            matches!(outcome, Outcome::Exited { .. }),
            "Restart=no must not restart, got {outcome:?}"
        );
        assert_eq!(
            m.get(&unit).unwrap().state,
            UnitState::Failed(FailedReason::StartFailure)
        );
    }

    /// After `StartLimitBurst` crashes in the window, the breaker parks the
    /// unit instead of restarting forever.
    #[test]
    fn repeated_crashes_trip_the_breaker() {
        let mut cfg = config("a.service", RestartPolicy::Always);
        cfg.start_limit_burst = 3;
        cfg.start_limit_interval_sec = 600;
        let mut m = UnitManager::new();
        m.add(cfg);
        let unit = UnitName::parse("a.service").unwrap();

        let mut tripped = false;
        for round in 0..6 {
            let pid = spawn_pid(&mut m, &unit);
            let _ = process::kill(pid, libc::SIGKILL);

            let outcome = m
                .on_child_exit(pid, ExitOutcome::Signalled(libc::SIGKILL))
                .expect("handled");

            match outcome {
                Outcome::RestartScheduled { .. } => {
                    // Pretend the backoff elapsed, so the next round respawns.
                    m.units.get_mut(&unit).unwrap().restart_at = None;
                }
                Outcome::StartLimitTripped { .. } => {
                    tripped = true;
                    assert!(
                        round >= 2,
                        "the breaker must allow at least `burst` attempts, tripped at {round}"
                    );
                    break;
                }
                other => panic!("unexpected outcome {other:?}"),
            }
        }

        assert!(tripped, "the breaker must stop the retries");
        assert_eq!(
            m.get(&unit).unwrap().state,
            UnitState::Failed(FailedReason::StartLimitHit),
            "a tripped breaker is a distinct failure kind"
        );
    }

    /// A stop already in flight must not be mistaken for a crash, even when
    /// `Restart=always` - otherwise every shutdown would restart everything.
    #[test]
    fn an_exit_during_stop_does_not_restart() {
        let mut m = manager_with("a.service", RestartPolicy::Always);
        let unit = UnitName::parse("a.service").unwrap();
        m.spawn_unit(&unit).expect("spawn");
        let pid = m.get(&unit).unwrap().pid.expect("pid");

        m.stop_unit(&unit);
        assert_eq!(m.get(&unit).unwrap().state, UnitState::Deactivating);

        let outcome = m
            .on_child_exit(pid, ExitOutcome::Signalled(libc::SIGTERM))
            .expect("handled");
        assert!(
            matches!(outcome, Outcome::Exited { .. }),
            "a deliberate stop is not a crash, got {outcome:?}"
        );
        assert_eq!(m.get(&unit).unwrap().state, UnitState::Inactive);
    }

    /// A clean `oneshot` is a success, not something to restart.
    #[test]
    fn a_successful_oneshot_becomes_active_exited() {
        let mut m = oneshot_manager(RestartPolicy::OnFailure, true);
        let unit = UnitName::parse("once.service").unwrap();
        let pid = spawn_pid(&mut m, &unit);

        let outcome = m.on_child_exit(pid, ExitOutcome::Success).expect("handled");
        assert!(
            matches!(outcome, Outcome::Exited { .. }),
            "a clean oneshot is not restarted, got {outcome:?}"
        );
        let rt = m.get(&unit).unwrap();
        assert_eq!(rt.state, UnitState::ActiveExited);
        assert!(rt.state.is_satisfied(), "downstream may proceed");
    }

    /// An `oneshot` that fails *is* subject to `Restart=`.
    #[test]
    fn a_failed_oneshot_follows_the_restart_policy() {
        let mut m = oneshot_manager(RestartPolicy::OnFailure, false);
        let unit = UnitName::parse("once.service").unwrap();
        let pid = spawn_pid(&mut m, &unit);

        let outcome = m
            .on_child_exit(pid, ExitOutcome::Failure(1))
            .expect("handled");
        assert!(
            matches!(outcome, Outcome::RestartScheduled { .. }),
            "a failed oneshot with Restart=on-failure must retry, got {outcome:?}"
        );
    }

    /// Spawn `unit` and return the pid the manager tracked for it.
    fn spawn_pid(m: &mut UnitManager, unit: &UnitName) -> libc::pid_t {
        m.spawn_unit(unit).expect("spawn");
        m.get(unit).and_then(|rt| rt.pid).expect("a tracked pid")
    }

    /// Build a manager holding one `oneshot` whose command exits immediately,
    /// successfully or not.
    ///
    /// The child really runs: an exit can only be attributed to a unit if the
    /// manager spawned it, so faking a pid would not exercise the ownership
    /// lookup that production depends on.
    fn oneshot_manager(policy: RestartPolicy, succeed: bool) -> UnitManager {
        let script = if succeed { "exit 0" } else { "exit 1" };
        let mut cfg = UnitConfig::new(UnitName::parse("once.service").unwrap());
        cfg.service_kind = ServiceKind::Oneshot;
        cfg.exec_start = rinit_core::ExecCommand::new(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            script.to_string(),
        ]);
        cfg.restart = policy;
        cfg.restart_sec = 0;
        cfg.start_limit_interval_sec = 600;
        cfg.start_limit_burst = 5;
        let mut m = UnitManager::new();
        m.add(cfg);
        m
    }

    /// An exit for a pid rinit does not know must be ignored, not crash.
    #[test]
    fn an_unknown_pid_is_ignored() {
        let mut m = manager_with("a.service", RestartPolicy::Always);
        assert!(m.on_child_exit(123_456_789, ExitOutcome::Success).is_none());
    }

    #[test]
    fn different_units_get_different_jitter_seeds() {
        let a = fnv1a(b"a.service");
        let b = fnv1a(b"b.service");
        assert_ne!(a, b, "two units must not share a jitter stream");
        assert_eq!(a, fnv1a(b"a.service"), "and it must be stable");
    }

    #[test]
    fn units_are_reported_in_name_order() {
        let mut m = UnitManager::new();
        for n in ["c.service", "a.service", "b.service"] {
            m.add(config(n, RestartPolicy::No));
        }
        let names: Vec<String> = m
            .summary()
            .iter()
            .map(|line| line.split(':').next().unwrap().to_string())
            .collect();
        assert_eq!(names, ["a.service", "b.service", "c.service"]);
        assert!(m.get(&UnitName::parse("a.service").unwrap()).is_some());
        assert!(m.get(&UnitName::parse("z.service").unwrap()).is_none());
    }
}
