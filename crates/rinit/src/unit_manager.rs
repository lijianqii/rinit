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
use rinit_core::{Effect, Event, ExitOutcome, FailedReason, NotifyMessage, UnitState, transition};
use rinit_core::{Job, JobQueue, JobTarget, RequestSource, UnitConfig, UnitName};

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
    /// When a `notify` start must have signalled readiness by.
    ///
    /// `None` for types that do not wait, and when `TimeoutStartSec=0`
    /// (wait forever, plan §1.1).
    pub start_deadline: Option<Instant>,
    /// When a unit that ignored the start-timeout `SIGTERM` must be `SIGKILL`ed
    /// (plan §1.3(12)).
    pub kill_at: Option<Instant>,
    /// When the next `HealthCheck` probe is due (plan §1.3(5)).
    pub health_at: Option<Instant>,
    /// The running health probe's pid, if one is in flight.
    pub probe_pid: Option<libc::pid_t>,
    /// When an in-flight health probe must be given up on.
    pub probe_deadline: Option<Instant>,
    /// The last probe's verdict: `Some(true)` healthy, `Some(false)` not,
    /// `None` never probed.
    pub healthy: Option<bool>,
    /// Set when a stop was requested *to restart*, not to end the unit: a health
    /// failure that asks for a restart. The next reaped exit honours it.
    pub restart_after_stop: bool,
    /// The last `STATUS=` text, for `rctl status`.
    pub status: Option<String>,
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
            start_deadline: None,
            kill_at: None,
            health_at: None,
            probe_pid: None,
            probe_deadline: None,
            healthy: None,
            restart_after_stop: false,
            status: None,
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

/// An absolute deadline `secs` from now, or `None` for "wait forever".
///
/// `TimeoutStartSec=0` means no timeout (plan §1.1), so it must map to `None`
/// rather than to a deadline of "now".
fn deadline(secs: u64) -> Option<Instant> {
    (secs > 0).then(|| Instant::now() + Duration::from_secs(secs))
}

/// Arm the next health probe for `rt`, if it has a `HealthCheck` at all.
///
/// A zero interval disables probing, mirroring how `TimeoutStartSec=0` disables
/// a timeout.
fn schedule_health(rt: &mut UnitRuntime) {
    rt.health_at = if rt.config.health_check.is_empty() || rt.config.health_interval_sec == 0 {
        None
    } else {
        Some(Instant::now() + Duration::from_secs(rt.config.health_interval_sec))
    };
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

/// Insert or replace `key` in a `KEY=VALUE` list, keeping one entry per key.
fn upsert(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some(entry) = env.iter_mut().find(|(k, _)| k == key) {
        entry.1 = value.to_string();
    } else {
        env.push((key.to_string(), value.to_string()));
    }
}

/// Assemble the environment a service runs with (plan §1.1).
///
/// Precedence, lowest to highest: the fixed [`SERVICE_ENV`](crate::service::SERVICE_ENV)
/// base, the unit's `EnvironmentFile=`, its `Environment=`, then rinit's own
/// additions (`extra_env`, e.g. `NOTIFY_SOCKET`). Entries are merged rather than
/// concatenated, so the child sees exactly one value per key and the later
/// source wins - the plan is explicit that `Environment=` beats the file.
///
/// An unreadable `EnvironmentFile=` is a warning, not a start failure: a
/// service that loses an optional file must still come up.
pub(crate) fn build_service_env(config: &UnitConfig, extra_env: &[String]) -> Vec<String> {
    let mut merged: Vec<(String, String)> = Vec::new();
    for entry in crate::service::SERVICE_ENV {
        if let Some((key, value)) = entry.split_once('=') {
            upsert(&mut merged, key, value);
        }
    }
    if !config.environment_file.is_empty() {
        match std::fs::read_to_string(&config.environment_file) {
            Ok(body) => {
                for (key, value) in rinit_core::parse_environment_file(&body) {
                    upsert(&mut merged, &key, &value);
                }
            }
            Err(e) => rlog!(
                "warning: {}: cannot read EnvironmentFile {}: {e}",
                config.name,
                config.environment_file
            ),
        }
    }
    for (key, value) in &config.environment {
        upsert(&mut merged, key, value);
    }
    for entry in extra_env {
        if let Some((key, value)) = entry.split_once('=') {
            upsert(&mut merged, key, value);
        }
    }
    merged
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect()
}

/// Tracks every unit and turns events into state changes and [`Outcome`]s.
#[derive(Debug)]
pub struct UnitManager {
    units: BTreeMap<UnitName, UnitRuntime>,
    /// What each unit has been asked to become (plan §1.3(11)).
    ///
    /// The queue is the source of truth for *target* state; `units[..].state`
    /// is where the unit actually is. Keeping them apart is what lets two
    /// sources race without producing two decisions (plan §1.3(13)).
    jobs: JobQueue,
    /// Monotonic origin for the Job queue's timestamps.
    origin: Instant,
}

impl UnitManager {
    /// An empty manager.
    pub fn new() -> Self {
        UnitManager {
            units: BTreeMap::new(),
            jobs: JobQueue::new(),
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

    /// Ask for `unit` to be started, merging with any request already held.
    ///
    /// Returns what the merged target implies *right now* (plan §1.3(13)), or
    /// `None` when there is nothing to do: either the target is already
    /// satisfied, or a stop in progress must finish first.
    ///
    /// The effect is derived when the request arrives rather than re-derived on
    /// every loop. That distinction matters: plan §1.3(13) has a *start job*
    /// against `RestartWait` cancel the backoff, and a unit in `RestartWait`
    /// keeps the `Started` target its boot request gave it. Re-deriving the
    /// effect continuously would therefore cancel every automatic restart's
    /// backoff, which is precisely what the backoff exists to prevent.
    pub fn request_start(&mut self, unit: &UnitName, source: RequestSource) -> Option<Effect> {
        self.request(unit, JobTarget::Started, source)
    }

    /// Ask for `unit` to be stopped. Mirrors [`request_start`](Self::request_start).
    pub fn request_stop(&mut self, unit: &UnitName, source: RequestSource) -> Option<Effect> {
        self.request(unit, JobTarget::Stopped, source)
    }

    /// Submit a request and derive the action it implies.
    fn request(
        &mut self,
        unit: &UnitName,
        target: JobTarget,
        source: RequestSource,
    ) -> Option<Effect> {
        if !self.units.contains_key(unit) {
            return None;
        }
        let at = self.now_millis();
        self.jobs.submit(Job::new(unit.clone(), target, source, at));
        self.effect_for(unit)
    }

    /// The action the merged target implies for `unit` right now.
    ///
    /// `None` means either "nothing was asked" or "the request is waiting": a
    /// start against a stop in progress is queued rather than raced
    /// (plan §1.3(13)). M3's `rctl` is the first source that can produce the
    /// second case, so remembering the queued start belongs with it.
    pub fn effect_for(&self, unit: &UnitName) -> Option<Effect> {
        let actual = self.units.get(unit)?.state;
        self.jobs
            .reconcile(unit, actual)
            .map(|(effect, _to)| effect)
    }

    /// Units the queue still wants running, in name order.
    ///
    /// Shutdown stops these (plan §1.3(11)); a unit nobody ever asked for is
    /// not included, because it has nothing to stop.
    pub fn started_units(&self) -> Vec<UnitName> {
        self.jobs
            .targets()
            .into_iter()
            .filter(|(_, target)| target.target == JobTarget::Started)
            .map(|(name, _)| name)
            .collect()
    }

    /// Forget the breaker's failures so the next start is judged fresh.
    ///
    /// This is what makes a manual start the escape hatch from
    /// `Failed(StartLimitHit)` (plan §1.3(13)); `rctl reset-failed` will use
    /// the same method without starting the unit.
    pub fn clear_start_limit(&mut self, unit: &UnitName) {
        if let Some(rt) = self.units.get_mut(unit) {
            rt.backoff.reset();
        }
    }

    /// Units that hard-`Requires=` `unit`, in name order.
    pub fn requires_dependents(&self, unit: &UnitName) -> Vec<UnitName> {
        self.units
            .values()
            .filter(|rt| rt.config.deps.requires.contains(unit))
            .map(|rt| rt.config.name.clone())
            .collect()
    }

    /// Record that `unit` failed to start and fail whatever hard-requires it.
    ///
    /// Plan §1.3(13): a dependent of a failed `Requires=` must not start. The
    /// dependents are marked `Failed` so that a boot that has not reached them
    /// yet skips them, rather than building on a broken foundation. Returns the
    /// units failed as a result, so the caller can say why.
    ///
    /// A dependent that already has a process is left alone: it is running, and
    /// stopping it here would turn one failure into a cascade.
    pub fn fail_start(&mut self, unit: &UnitName) -> Vec<UnitName> {
        if let Some(rt) = self.units.get_mut(unit)
            && !matches!(rt.state, UnitState::Failed(_))
        {
            rt.pid = None;
            rt.state = UnitState::Failed(FailedReason::StartFailure);
        }
        let dependents = self.requires_dependents(unit);
        for dependent in &dependents {
            if let Some(rt) = self.units.get_mut(dependent)
                && rt.pid.is_none()
            {
                rt.state = UnitState::Failed(FailedReason::StartFailure);
            }
        }
        dependents
    }

    /// Whether `unit` is in a failure state.
    pub fn is_failed(&self, unit: &UnitName) -> bool {
        self.units
            .get(unit)
            .is_some_and(|rt| matches!(rt.state, UnitState::Failed(_)))
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

    /// Time until the next restart is due, if any.
    pub fn next_restart_in(&self) -> Option<Duration> {
        let now = Instant::now();
        self.units
            .values()
            .filter_map(|rt| rt.restart_at)
            .map(|at| at.saturating_duration_since(now))
            .min()
    }

    /// Spawn a unit, adding `extra_env` to the child's environment.
    ///
    /// The child's environment is the base set, then its `EnvironmentFile=`,
    /// then its `Environment=` (which wins over the file), then `extra_env`
    /// (rinit's own additions such as `NOTIFY_SOCKET`). Its
    /// `WorkingDirectory=` is applied before `exec`.
    ///
    /// # Errors
    /// Propagates a spawn failure; the caller decides whether that is fatal.
    pub fn spawn_unit_with_env(
        &mut self,
        unit: &UnitName,
        extra_env: &[String],
    ) -> Result<(), RuntimeError> {
        let now = self.now_millis();

        // Everything the spawn needs, read through an immutable borrow so the
        // `EnvironmentFile=` read below cannot race the state update.
        let (argv, env, cwd, kind) = {
            let Some(rt) = self.units.get(unit) else {
                return Ok(());
            };
            if rt.pid.is_some() {
                return Ok(());
            }
            // Targets are process-less; starting one is a bookkeeping no-op.
            if !rt.spawns() {
                if let Some(rt) = self.units.get_mut(unit) {
                    rt.state = UnitState::Active;
                }
                return Ok(());
            }
            (
                rt.config.exec_start.argv().to_vec(),
                build_service_env(&rt.config, extra_env),
                rt.config.working_directory.clone(),
                rt.config.service_kind,
            )
        };

        // A fresh process group, so a stop reaches the whole tree (plan §1.3(6)).
        let pid = spawn_service_in_group(&argv, &env, Some(&cwd))?;

        let Some(rt) = self.units.get_mut(unit) else {
            return Ok(());
        };
        rt.pid = Some(pid);
        rt.state = UnitState::Starting;
        rt.restart_at = None;
        rt.kill_at = None;

        match kind {
            // `simple` is live the moment it is exec'd.
            ServiceKind::Simple => {
                rt.state = UnitState::Active;
                rt.backoff.on_active(now);
                schedule_health(rt);
            }
            // `oneshot` completes on exit; boot waits for it directly.
            ServiceKind::Oneshot => {}
            // `notify` waits for `READY=1`, bounded by `TimeoutStartSec`
            // (plan §1.3(12)). A zero timeout means wait forever.
            ServiceKind::Notify => {
                rt.start_deadline = deadline(rt.config.timeout_start_sec);
            }
        }
        rlog!("started {unit} as pid {pid}");
        Ok(())
    }

    /// Record a notify message for `unit` and act on readiness.
    ///
    /// Returns `true` when the unit became `Active` on this message, so the
    /// caller can log the transition once.
    pub fn on_notify(&mut self, unit: &UnitName, message: &NotifyMessage) -> bool {
        let now = self.now_millis();
        let Some(rt) = self.units.get_mut(unit) else {
            return false;
        };
        if let Some(status) = &message.status {
            rt.status = Some(status.clone());
        }
        if !message.ready || rt.state != UnitState::Starting {
            return false;
        }

        match transition(
            rt.state,
            Event::StartUpComplete,
            rt.policy(),
            rt.config.service_kind,
        ) {
            Ok(t) => {
                rt.state = t.to;
                rt.start_deadline = None;
                rt.backoff.on_active(now);
                schedule_health(rt);
                true
            }
            Err(_) => false,
        }
    }

    /// Units whose `TimeoutStartSec` expired without a readiness signal.
    ///
    /// Plan §1.3(12): the start-timeout sends `SIGTERM` first, and if the
    /// service has still not exited after `TimeoutStopSec`, `SIGKILL`. The
    /// first stage happens here; [`due_kills`](Self::due_kills) does the second.
    /// Returns the units that were timed out, in name order.
    pub fn due_start_timeouts(&mut self) -> Vec<UnitName> {
        let now = Instant::now();
        let mut due = Vec::new();
        for (name, rt) in self.units.iter_mut() {
            // A unit that already left `Starting` cannot time out starting.
            if rt.state != UnitState::Starting {
                rt.start_deadline = None;
                continue;
            }
            if let Some(at) = rt.start_deadline
                && at <= now
            {
                rt.start_deadline = None;
                if let Some(pid) = rt.pid {
                    match process::kill_process_group(pid, libc::SIGTERM) {
                        Ok(()) => {}
                        Err(e) if e.errno() == libc::ESRCH => {}
                        Err(e) => rlog!("warning: SIGTERM to {name} failed: {e}"),
                    }
                }
                rt.kill_at = deadline(rt.config.timeout_stop_sec);
                due.push(name.clone());
            }
        }
        due
    }

    /// Units that ignored the start-timeout `SIGTERM` and must be `SIGKILL`ed.
    ///
    /// Returns the units whose process group was signalled, in name order.
    pub fn due_kills(&mut self) -> Vec<UnitName> {
        let now = Instant::now();
        let mut due = Vec::new();
        for (name, rt) in self.units.iter_mut() {
            if let Some(at) = rt.kill_at
                && at <= now
            {
                rt.kill_at = None;
                if let Some(pid) = rt.pid {
                    match process::kill_process_group(pid, libc::SIGKILL) {
                        Ok(()) => due.push(name.clone()),
                        Err(e) if e.errno() == libc::ESRCH => {}
                        Err(e) => rlog!("warning: SIGKILL to {name} failed: {e}"),
                    }
                }
            }
        }
        due
    }

    /// Time until the next `TimeoutStartSec` deadline, if any.
    pub fn next_start_timeout_in(&self) -> Option<Duration> {
        let now = Instant::now();
        self.units
            .values()
            .filter(|rt| rt.state == UnitState::Starting)
            .filter_map(|rt| rt.start_deadline)
            .map(|at| at.saturating_duration_since(now))
            .min()
    }

    /// Time until the next post-timeout `SIGKILL` is due, if any.
    pub fn next_kill_in(&self) -> Option<Duration> {
        let now = Instant::now();
        self.units
            .values()
            .filter_map(|rt| rt.kill_at)
            .map(|at| at.saturating_duration_since(now))
            .min()
    }

    /// Start the `HealthCheck` of every unit whose probe is due.
    ///
    /// A probe is an ordinary child: it is reaped by the same `waitpid` loop as
    /// everything else, which keeps rinit's single-reaper invariant intact
    /// (plan §1.3(6)). Its exit is recognised because the pid is remembered in
    /// `probe_pid`. Returns the units probed, in name order.
    pub fn due_health_probes(&mut self) -> Vec<UnitName> {
        let now = Instant::now();
        let mut probed = Vec::new();
        for (name, rt) in self.units.iter_mut() {
            if rt.state != UnitState::Active {
                rt.health_at = None;
                continue;
            }
            if rt.probe_pid.is_some() {
                continue;
            }
            if !rt.health_at.is_some_and(|at| at <= now) {
                continue;
            }
            rt.health_at = None;

            let argv = rt.config.health_check.argv().to_vec();
            // The probe runs as the service does: same environment, same
            // working directory (plan §7.4).
            let env = build_service_env(&rt.config, &[]);
            let cwd = rt.config.working_directory.clone();
            match spawn_service_in_group(&argv, &env, Some(&cwd)) {
                Ok(pid) => {
                    rt.probe_pid = Some(pid);
                    rt.probe_deadline = deadline(rt.config.health_timeout_sec);
                    probed.push(name.clone());
                }
                Err(e) => {
                    // A probe that cannot even start must not spin: wait a full
                    // interval before trying again.
                    rlog!("warning: {name}: health probe failed to start: {e}");
                    schedule_health(rt);
                }
            }
        }
        probed
    }

    /// Kill health probes that outlived `HealthTimeoutSec`.
    ///
    /// The `SIGKILL` produces a reaped signal exit, which
    /// [`finish_probe`](Self::finish_probe) turns into an unhealthy verdict, so
    /// this only has to do the signalling.
    pub fn due_probe_timeouts(&mut self) -> Vec<UnitName> {
        let now = Instant::now();
        let mut timed_out = Vec::new();
        for (name, rt) in self.units.iter_mut() {
            if let Some(at) = rt.probe_deadline
                && at <= now
            {
                rt.probe_deadline = None;
                if let Some(pid) = rt.probe_pid {
                    let _ = process::kill_process_group(pid, libc::SIGKILL);
                    timed_out.push(name.clone());
                }
            }
        }
        timed_out
    }

    /// Time until the next health probe is due, if any.
    pub fn next_health_in(&self) -> Option<Duration> {
        let now = Instant::now();
        self.units
            .values()
            .filter_map(|rt| rt.health_at)
            .map(|at| at.saturating_duration_since(now))
            .min()
    }

    /// Time until the next probe timeout, if any.
    pub fn next_probe_timeout_in(&self) -> Option<Duration> {
        let now = Instant::now();
        self.units
            .values()
            .filter_map(|rt| rt.probe_deadline)
            .map(|at| at.saturating_duration_since(now))
            .min()
    }

    /// The unit whose health probe `pid` belongs to, if any.
    fn probe_owner(&self, pid: libc::pid_t) -> Option<UnitName> {
        self.units
            .iter()
            .find(|(_, rt)| rt.probe_pid == Some(pid))
            .map(|(name, _)| name.clone())
    }

    /// Record a finished probe and act on the verdict (plan §1.3(5)).
    ///
    /// A health failure is not a crash: it is marked, and only restarts when
    /// `HealthRestartOnFailure=true`.
    fn finish_probe(&mut self, unit: &UnitName, outcome: ExitOutcome) {
        let Some(rt) = self.units.get_mut(unit) else {
            return;
        };
        rt.probe_pid = None;
        rt.probe_deadline = None;
        let healthy = matches!(outcome, ExitOutcome::Success);
        rt.healthy = Some(healthy);

        if healthy {
            rlog!("{unit} is healthy");
            schedule_health(rt);
            return;
        }

        rlog!("warning: {unit} failed its health check ({outcome:?})");
        if rt.config.health_restart_on_failure && rt.pid.is_some() {
            let pid = rt.pid.expect("checked above");
            match process::kill_process_group(pid, libc::SIGTERM) {
                Ok(()) => {
                    // Mark the stop as a restart so the reaped exit re-enters
                    // the restart path instead of ending the unit. Set only
                    // once the signal was delivered, so a stop that had nothing
                    // to signal cannot leave the flag dangling.
                    rt.state = UnitState::Deactivating;
                    rt.restart_after_stop = true;
                }
                Err(e) if e.errno() == libc::ESRCH => {
                    // The unit died on its own and its exit is on its way; let
                    // the normal crash path handle it rather than restarting
                    // here twice.
                    rt.state = UnitState::Inactive;
                    rt.pid = None;
                }
                Err(e) => {
                    rlog!("warning: SIGTERM to {unit} failed: {e}");
                    schedule_health(rt);
                }
            }
        } else {
            schedule_health(rt);
        }
    }

    /// Signal a unit's process group to stop.
    ///
    /// A pending restart is cancelled: an explicit stop beats an automatic one
    /// (plan §1.3(13)).
    ///
    /// The stop is two-stage (plan §1.1, §1.3(12)): `SIGTERM` now, and a
    /// `SIGKILL` deadline `TimeoutStopSec` later - immediately, when it is `0`.
    /// [`due_kills`](Self::due_kills) fires that deadline. Without it, a service
    /// that ignores `SIGTERM` would never be killed, on shutdown or otherwise.
    pub fn stop_unit(&mut self, unit: &UnitName) {
        let Some(rt) = self.units.get_mut(unit) else {
            return;
        };
        rt.restart_at = None;
        rt.start_deadline = None;
        // A fresh stop re-arms the kill deadline below, so clear any stale one
        // (e.g. left over from a start timeout) first.
        rt.kill_at = None;
        // An explicit stop also abandons any health probing and cancels a
        // health-triggered restart: the operator asked for the unit to stop.
        rt.health_at = None;
        rt.probe_deadline = None;
        rt.restart_after_stop = false;
        if let Some(pid) = rt.probe_pid.take() {
            let _ = process::kill_process_group(pid, libc::SIGKILL);
        }

        match rt.pid {
            Some(pid) => match process::kill_process_group(pid, libc::SIGTERM) {
                Ok(()) => {
                    rt.state = UnitState::Deactivating;
                    if rt.config.timeout_stop_sec == 0 {
                        // Plan §1.1: `TimeoutStopSec=0` means SIGKILL at once.
                        let _ = process::kill_process_group(pid, libc::SIGKILL);
                    } else {
                        rt.kill_at =
                            Some(Instant::now() + Duration::from_secs(rt.config.timeout_stop_sec));
                    }
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
        // A health probe is a child too; its exit is a verdict, not a unit's
        // death (plan §1.3(5)).
        if let Some(unit) = self.probe_owner(pid) {
            self.finish_probe(&unit, outcome);
            return None;
        }

        let unit = self
            .units
            .iter()
            .find(|(_, rt)| rt.pid == Some(pid))
            .map(|(name, _)| name.clone())?;

        let now = self.now_millis();
        let rt = self.units.get_mut(&unit)?;
        rt.pid = None;
        rt.start_deadline = None;
        rt.kill_at = None;
        rt.health_at = None;
        rt.probe_deadline = None;
        rt.last_outcome = Some(outcome);

        // A stop already in flight owns the transition; the exit just confirms
        // it, and must not be mistaken for a crash worth restarting - unless it
        // was a health-triggered restart, which deliberately re-enters the
        // restart path.
        if rt.state == UnitState::Deactivating {
            if rt.restart_after_stop {
                rt.restart_after_stop = false;
                return Some(self.schedule_restart(&unit, now));
            }
            rt.state = UnitState::Inactive;
            return Some(Outcome::Exited {
                unit,
                state: UnitState::Inactive,
            });
        }

        // A unit that was still `Starting` has not "exited" in the sense the
        // transition table models: its exit *is* the end of its start-up. That
        // covers a `oneshot` finishing and a `notify` service that died (or was
        // timed out) before signalling readiness (plan §1.3(3), §1.3(12)).
        let starting = rt.state == UnitState::Starting;
        let oneshot = rt.config.service_kind == ServiceKind::Oneshot;

        let event = if starting {
            if oneshot && matches!(outcome, ExitOutcome::Success) {
                Event::StartUpComplete
            } else {
                Event::StartUpFailed
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
        if starting && oneshot && matches!(outcome, ExitOutcome::Success) {
            rlog!("{unit} completed (Active(Exited))");
            return Some(Outcome::Exited {
                unit,
                state: UnitState::ActiveExited,
            });
        }

        if !rt.policy().wants_restart(outcome) {
            let state = rt.state;
            return Some(Outcome::Exited { unit, state });
        }
        Some(self.schedule_restart(&unit, now))
    }

    /// Arm the backoff for `unit`, unless the breaker forbids it.
    fn schedule_restart(&mut self, unit: &UnitName, now: u64) -> Outcome {
        let Some(rt) = self.units.get_mut(unit) else {
            return Outcome::Exited {
                unit: unit.clone(),
                state: UnitState::Inactive,
            };
        };
        match rt.backoff.on_failure(now) {
            Some(delay) => {
                rt.state = UnitState::RestartWait;
                rt.restart_at = Some(Instant::now() + delay);
                Outcome::RestartScheduled {
                    unit: unit.clone(),
                    delay,
                }
            }
            None => {
                rt.state = UnitState::Failed(FailedReason::StartLimitHit);
                let failures = rt.backoff.failures_in_window();
                Outcome::StartLimitTripped {
                    unit: unit.clone(),
                    failures,
                }
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
        m.spawn_unit_with_env(&unit, &[]).expect("spawn");
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
        m.spawn_unit_with_env(&unit, &[]).expect("start");
        let rt = m.get(&unit).unwrap();
        assert_eq!(rt.state, UnitState::Active);
        assert!(rt.pid.is_none(), "a target must not spawn anything");
    }

    /// The core of M2's acceptance: a killed service is restarted.
    #[test]
    fn a_killed_service_schedules_a_restart() {
        let mut m = manager_with("a.service", RestartPolicy::Always);
        let unit = UnitName::parse("a.service").unwrap();
        m.spawn_unit_with_env(&unit, &[]).expect("spawn");
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
        m.spawn_unit_with_env(&unit, &[]).expect("spawn");
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
        m.spawn_unit_with_env(&unit, &[]).expect("spawn");
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
        m.spawn_unit_with_env(unit, &[]).expect("spawn");
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

    /// Build a manager holding one running `notify` service.
    fn notify_running(name: &str) -> (UnitManager, UnitName) {
        let mut cfg = config(name, RestartPolicy::No);
        cfg.service_kind = ServiceKind::Notify;
        let mut m = UnitManager::new();
        m.add(cfg);
        let unit = UnitName::parse(name).unwrap();
        m.spawn_unit_with_env(&unit, &[]).expect("spawn");
        (m, unit)
    }

    /// Kill the unit's process group and reap it, so a test leaves nothing.
    fn cleanup(m: &mut UnitManager, unit: &UnitName) {
        if let Some(pid) = m.get(unit).and_then(|rt| rt.pid) {
            let _ = process::kill_process_group(pid, libc::SIGKILL);
        }
        let _ = wait::reap_children();
    }

    /// `notify` stays `Starting` until `READY=1` arrives (plan §1.3(12)).
    #[test]
    fn a_notify_unit_becomes_active_on_ready() {
        let (mut m, unit) = notify_running("n.service");
        assert_eq!(m.get(&unit).unwrap().state, UnitState::Starting);

        let became_active = m.on_notify(
            &unit,
            &NotifyMessage {
                ready: true,
                status: None,
                watchdog: false,
            },
        );
        assert!(became_active, "READY=1 must complete start-up");
        assert_eq!(m.get(&unit).unwrap().state, UnitState::Active);

        cleanup(&mut m, &unit);
    }

    /// `STATUS=` updates the description but must not complete start-up on its
    /// own.
    #[test]
    fn status_alone_does_not_make_a_unit_ready() {
        let (mut m, unit) = notify_running("n.service");
        let became_active = m.on_notify(
            &unit,
            &NotifyMessage {
                ready: false,
                status: Some("warming up".to_string()),
                watchdog: false,
            },
        );
        assert!(!became_active);
        let rt = m.get(&unit).unwrap();
        assert_eq!(rt.state, UnitState::Starting);
        assert_eq!(rt.status.as_deref(), Some("warming up"));

        cleanup(&mut m, &unit);
    }

    /// `WATCHDOG=1` is recorded but M2 takes no action on it.
    #[test]
    fn a_watchdog_ping_does_not_complete_start_up() {
        let (mut m, unit) = notify_running("n.service");
        let became_active = m.on_notify(
            &unit,
            &NotifyMessage {
                ready: false,
                status: None,
                watchdog: true,
            },
        );
        assert!(!became_active);
        assert_eq!(m.get(&unit).unwrap().state, UnitState::Starting);

        cleanup(&mut m, &unit);
    }

    /// A `TimeoutStartSec` that expires without `READY=1` sends `SIGTERM` and
    /// arms the `SIGKILL` follow-up (plan §1.3(12)).
    #[test]
    fn a_silent_notify_unit_is_timed_out() {
        let (mut m, unit) = notify_running("n.service");
        // Pretend the deadline already passed instead of waiting for it.
        m.units.get_mut(&unit).unwrap().start_deadline =
            Some(Instant::now() - Duration::from_secs(1));

        let timed_out = m.due_start_timeouts();
        assert_eq!(timed_out.len(), 1, "the unit must be reported once");
        assert_eq!(timed_out[0], unit);
        let rt = m.get(&unit).unwrap();
        assert!(rt.start_deadline.is_none(), "the deadline fires once");
        assert!(rt.kill_at.is_some(), "the SIGKILL follow-up must be armed");

        // And it is not reported again.
        assert!(m.due_start_timeouts().is_empty());

        cleanup(&mut m, &unit);
    }

    /// The follow-up kill fires when its `TimeoutStopSec` elapses.
    #[test]
    fn a_unit_that_ignores_the_stop_signal_is_killed() {
        let (mut m, unit) = notify_running("n.service");
        m.units.get_mut(&unit).unwrap().kill_at = Some(Instant::now() - Duration::from_secs(1));

        let killed = m.due_kills();
        assert_eq!(killed.len(), 1, "the unit must be killed");
        assert_eq!(killed[0], unit);
        assert!(m.get(&unit).unwrap().kill_at.is_none(), "fires once");

        cleanup(&mut m, &unit);
    }

    /// Build a manager holding one running `simple` service with a `HealthCheck`.
    fn health_service(
        name: &str,
        probe: &str,
        restart_on_failure: bool,
    ) -> (UnitManager, UnitName) {
        let mut cfg = config(name, RestartPolicy::No);
        cfg.health_check = rinit_core::ExecCommand::new(vec![probe.to_string()]);
        cfg.health_interval_sec = 3600;
        cfg.health_timeout_sec = 5;
        cfg.health_restart_on_failure = restart_on_failure;
        let mut m = UnitManager::new();
        m.add(cfg);
        let unit = UnitName::parse(name).unwrap();
        m.spawn_unit_with_env(&unit, &[]).expect("spawn");
        (m, unit)
    }

    /// Make the next probe due immediately instead of waiting an interval.
    fn make_probe_due(m: &mut UnitManager, unit: &UnitName) {
        m.units.get_mut(unit).unwrap().health_at = Some(Instant::now() - Duration::from_secs(1));
    }

    /// A probe that exits 0 marks the unit healthy and books the next probe.
    #[test]
    fn a_passing_probe_marks_the_unit_healthy() {
        let (mut m, unit) = health_service("h.service", "/bin/true", false);
        make_probe_due(&mut m, &unit);

        let probed = m.due_health_probes();
        assert_eq!(probed.len(), 1);
        assert_eq!(probed[0], unit);
        let probe = m.get(&unit).unwrap().probe_pid.expect("a probe pid");

        // The probe exiting 0 is fed back as a child exit.
        m.on_child_exit(probe, ExitOutcome::Success);
        let rt = m.get(&unit).unwrap();
        assert_eq!(rt.healthy, Some(true));
        assert!(rt.probe_pid.is_none());
        assert!(rt.health_at.is_some(), "the next probe must be scheduled");
        assert_eq!(rt.state, UnitState::Active, "a healthy unit keeps running");

        cleanup(&mut m, &unit);
    }

    /// A failing probe without `HealthRestartOnFailure` marks the unit unhealthy
    /// but leaves it running.
    #[test]
    fn a_failing_probe_only_marks_without_restart() {
        let (mut m, unit) = health_service("h.service", "/bin/false", false);
        make_probe_due(&mut m, &unit);

        m.due_health_probes();
        let probe = m.get(&unit).unwrap().probe_pid.expect("a probe pid");
        m.on_child_exit(probe, ExitOutcome::Failure(1));

        let rt = m.get(&unit).unwrap();
        assert_eq!(rt.healthy, Some(false));
        assert_eq!(rt.state, UnitState::Active, "no restart was requested");
        assert!(rt.pid.is_some(), "the unit is still running");

        cleanup(&mut m, &unit);
    }

    /// With `HealthRestartOnFailure`, a failing probe stops the unit so the exit
    /// re-enters the restart path (plan §1.3(5)).
    #[test]
    fn a_failing_probe_restarts_when_asked() {
        let (mut m, unit) = health_service("h.service", "/bin/false", true);
        make_probe_due(&mut m, &unit);

        m.due_health_probes();
        let probe = m.get(&unit).unwrap().probe_pid.expect("a probe pid");
        m.on_child_exit(probe, ExitOutcome::Failure(1));
        assert_eq!(m.get(&unit).unwrap().state, UnitState::Deactivating);

        // The unit's own process now exits from the deliberate stop, which must
        // restart rather than end the unit.
        let upid = m.get(&unit).unwrap().pid.expect("the unit pid");
        let outcome = m
            .on_child_exit(upid, ExitOutcome::Signalled(libc::SIGTERM))
            .expect("handled");
        assert!(
            matches!(outcome, Outcome::RestartScheduled { .. }),
            "a health-triggered stop must restart, got {outcome:?}"
        );
        assert_eq!(m.get(&unit).unwrap().state, UnitState::RestartWait);

        cleanup(&mut m, &unit);
    }

    /// A probe that hangs past `HealthTimeoutSec` is killed.
    #[test]
    fn a_hung_probe_is_killed() {
        let (mut m, unit) = health_service("h.service", "/bin/true", false);
        make_probe_due(&mut m, &unit);
        m.due_health_probes();
        // Pretend the timeout already elapsed.
        m.units.get_mut(&unit).unwrap().probe_deadline =
            Some(Instant::now() - Duration::from_secs(1));

        let killed = m.due_probe_timeouts();
        assert_eq!(killed.len(), 1, "the probe must be reported");
        assert_eq!(killed[0], unit);
        assert!(
            m.get(&unit).unwrap().probe_deadline.is_none(),
            "the timeout fires once"
        );

        cleanup(&mut m, &unit);
    }

    /// A unit with no `HealthCheck` is never probed.
    #[test]
    fn a_unit_without_a_health_check_is_not_probed() {
        let mut m = manager_with("a.service", RestartPolicy::No);
        let unit = UnitName::parse("a.service").unwrap();
        m.spawn_unit_with_env(&unit, &[]).expect("spawn");

        assert!(m.due_health_probes().is_empty());
        assert!(m.get(&unit).unwrap().health_at.is_none());

        cleanup(&mut m, &unit);
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

    // --- Job queue interaction (plan §1.3(13)) --------------------------

    /// A start request against a unit that is down asks for a process.
    #[test]
    fn a_start_request_spawns_an_inactive_unit() {
        let mut m = manager_with("a.service", RestartPolicy::No);
        let unit = UnitName::parse("a.service").unwrap();
        assert_eq!(
            m.request_start(&unit, RequestSource::Dependency),
            Some(Effect::SpawnProcess)
        );
    }

    /// The request is remembered even when it produces no action, so a later
    /// reconcile still knows what was asked for.
    #[test]
    fn a_start_request_against_a_running_unit_is_satisfied() {
        let mut m = manager_with("a.service", RestartPolicy::No);
        let unit = UnitName::parse("a.service").unwrap();
        m.request_start(&unit, RequestSource::Dependency);
        spawn_pid(&mut m, &unit);

        assert_eq!(
            m.request_start(&unit, RequestSource::Dependency),
            Some(Effect::None),
            "an already-active unit must not be started twice"
        );
        assert_eq!(
            m.started_units(),
            std::slice::from_ref(&unit),
            "and the target is still the one the first request set"
        );

        cleanup(&mut m, &unit);
    }

    /// Asking a running unit to stop produces the signal, not a state guess.
    #[test]
    fn a_stop_request_signals_a_running_unit() {
        let mut m = manager_with("a.service", RestartPolicy::No);
        let unit = UnitName::parse("a.service").unwrap();
        m.request_start(&unit, RequestSource::Dependency);
        spawn_pid(&mut m, &unit);

        assert_eq!(
            m.request_stop(&unit, RequestSource::UserStop),
            Some(Effect::SendStopSignal)
        );
        assert_eq!(
            m.started_units(),
            Vec::<UnitName>::new(),
            "the queue no longer wants it running"
        );

        cleanup(&mut m, &unit);
    }

    /// Plan §1.3(13): a manual start is the escape hatch from a tripped breaker,
    /// and it cancels a pending backoff rather than waiting it out.
    #[test]
    fn a_start_request_reaches_past_failure_states() {
        for (state, expected) in [
            (
                UnitState::Failed(FailedReason::StartLimitHit),
                Effect::ClearStartLimitAndStart,
            ),
            (
                UnitState::Failed(FailedReason::StartFailure),
                Effect::ClearStartLimitAndStart,
            ),
            (UnitState::RestartWait, Effect::CancelBackoffAndStart),
        ] {
            let mut m = manager_with("a.service", RestartPolicy::Always);
            let unit = UnitName::parse("a.service").unwrap();
            m.units.get_mut(&unit).unwrap().state = state;

            assert_eq!(
                m.request_start(&unit, RequestSource::UserStart),
                Some(expected),
                "{state} must be startable"
            );
        }
    }

    /// Clearing the breaker is what makes the retry a fresh start.
    #[test]
    fn clearing_the_start_limit_forgets_the_failures() {
        let mut m = manager_with("a.service", RestartPolicy::Always);
        let unit = UnitName::parse("a.service").unwrap();
        let rt = m.units.get_mut(&unit).unwrap();
        rt.backoff.on_failure(0);
        rt.backoff.on_failure(0);
        assert_eq!(rt.backoff.failures_in_window(), 2);

        m.clear_start_limit(&unit);
        assert_eq!(
            m.get(&unit).unwrap().backoff.failures_in_window(),
            0,
            "a manual start must not inherit the old failures"
        );
    }

    /// Plan §1.3(13): a start that arrives while a stop is in progress waits
    /// instead of racing it. The stop wins, and the start is not applied to a
    /// unit that is still tearing down.
    #[test]
    fn a_start_against_a_stop_in_progress_is_queued() {
        let mut m = manager_with("a.service", RestartPolicy::No);
        let unit = UnitName::parse("a.service").unwrap();
        m.request_start(&unit, RequestSource::Dependency);
        spawn_pid(&mut m, &unit);

        // A stop that did not come from a request - a health failure does this
        // - so the `Started` target is still standing while the unit tears down.
        m.stop_unit(&unit);
        assert_eq!(m.get(&unit).unwrap().state, UnitState::Deactivating);

        assert_eq!(
            m.request_start(&unit, RequestSource::UserStart),
            None,
            "the start must wait for the stop to finish"
        );

        cleanup(&mut m, &unit);
    }

    /// Plan §1.3(11): a stop outranks a start, so a dependency start that
    /// arrives after shutdown must not bring the unit back - even though it is
    /// newer.
    #[test]
    fn a_shutdown_stop_outranks_a_later_start() {
        let mut m = manager_with("a.service", RestartPolicy::Always);
        let unit = UnitName::parse("a.service").unwrap();
        m.request_start(&unit, RequestSource::Dependency);
        spawn_pid(&mut m, &unit);

        m.request_stop(&unit, RequestSource::ShutdownStop);
        assert_eq!(
            m.request_start(&unit, RequestSource::Dependency),
            Some(Effect::SendStopSignal),
            "the newer start must lose to the shutdown"
        );
        assert!(m.started_units().is_empty());
        assert_eq!(m.get(&unit).unwrap().restart_at, None);

        cleanup(&mut m, &unit);
    }

    /// The boot target keeps saying "running" while a unit waits out a backoff,
    /// so nothing short of a fresh request may cancel that backoff. (Deriving
    /// the effect on every loop iteration would cancel it immediately, which is
    /// the whole failure mode the backoff exists to avoid.)
    #[test]
    fn a_pending_backoff_survives_the_standing_start_target() {
        let mut m = manager_with("a.service", RestartPolicy::Always);
        let unit = UnitName::parse("a.service").unwrap();
        m.request_start(&unit, RequestSource::Dependency);
        let pid = spawn_pid(&mut m, &unit);
        let _ = process::kill(pid, libc::SIGKILL);
        m.on_child_exit(pid, ExitOutcome::Signalled(libc::SIGKILL))
            .expect("handled");

        let rt = m.get(&unit).unwrap();
        assert_eq!(rt.state, UnitState::RestartWait);
        assert!(rt.restart_at.is_some(), "the backoff must still be pending");
        assert_eq!(
            m.started_units(),
            [unit],
            "the target still says the unit should be running"
        );
    }

    /// A request for a unit rinit does not know is a no-op, not a panic.
    #[test]
    fn a_request_for_an_unknown_unit_does_nothing() {
        let mut m = UnitManager::new();
        let ghost = UnitName::parse("ghost.service").unwrap();
        assert_eq!(m.request_start(&ghost, RequestSource::UserStart), None);
        assert_eq!(m.request_stop(&ghost, RequestSource::UserStop), None);
        assert!(m.started_units().is_empty());
    }

    // --- two-stage stop (plan §1.1) -------------------------------------

    /// Plan §1.3(13): a unit that fails to start fails everything that
    /// hard-`Requires=` it, so the dependents are not started on a broken
    /// foundation.
    #[test]
    fn a_failed_dependency_fails_its_requires_dependents() {
        let mut m = UnitManager::new();
        m.add(config("base.service", RestartPolicy::No));
        let mut dep = config("dep.service", RestartPolicy::No);
        dep.deps.requires = vec![UnitName::parse("base.service").unwrap()];
        m.add(dep);

        let base = UnitName::parse("base.service").unwrap();
        let dep = UnitName::parse("dep.service").unwrap();

        assert_eq!(m.requires_dependents(&base), std::slice::from_ref(&dep));
        let failed = m.fail_start(&base);
        assert_eq!(
            failed,
            std::slice::from_ref(&dep),
            "the dependent is reported"
        );
        assert!(m.is_failed(&base));
        assert!(m.is_failed(&dep), "a dependent must not be left startable");
    }
    /// A stop arms the `SIGKILL` follow-up at `TimeoutStopSec`, so a service
    /// that ignores `SIGTERM` is still killed. Without this the shutdown path
    /// could never escalate.
    #[test]
    fn a_stop_arms_the_sigkill_deadline() {
        let mut m = manager_with("a.service", RestartPolicy::No);
        let unit = UnitName::parse("a.service").unwrap();
        spawn_pid(&mut m, &unit);
        m.units.get_mut(&unit).unwrap().config.timeout_stop_sec = 10;

        m.stop_unit(&unit);

        let rt = m.get(&unit).unwrap();
        assert_eq!(rt.state, UnitState::Deactivating);
        assert!(
            rt.kill_at.is_some(),
            "TimeoutStopSec must arm the SIGKILL follow-up"
        );

        cleanup(&mut m, &unit);
    }

    /// `TimeoutStopSec=0` means `SIGKILL` at once (plan §1.1), so there is no
    /// deadline to wait for.
    #[test]
    fn a_zero_stop_timeout_kills_immediately() {
        let mut m = manager_with("a.service", RestartPolicy::No);
        let unit = UnitName::parse("a.service").unwrap();
        spawn_pid(&mut m, &unit);
        m.units.get_mut(&unit).unwrap().config.timeout_stop_sec = 0;

        m.stop_unit(&unit);

        let rt = m.get(&unit).unwrap();
        assert!(
            rt.kill_at.is_none(),
            "TimeoutStopSec=0 signals SIGKILL now, not later"
        );

        cleanup(&mut m, &unit);
    }
}
