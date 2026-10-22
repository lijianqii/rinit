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

use rinit_core::unit::cgroup::CgroupLimits;
use rinit_core::unit::config::{RestartPolicy, ServiceKind};
use rinit_core::unit::dependency::spawns_process;
use rinit_core::unit::restart::BackoffState;
use rinit_core::{Effect, Event, ExitOutcome, FailedReason, NotifyMessage, UnitState, transition};
use rinit_core::{Job, JobQueue, JobTarget, RequestSource, UnitConfig, UnitName};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use rinit_runtime::RuntimeError;
use rinit_runtime::SyscallError;
use rinit_runtime::syscall::{process, wait};

use crate::cgroup::Cgroup;
use crate::log::rlog;
use crate::service::spawn_service_in_group;

/// What registering a unit definition did (plan §7.6).
///
/// The four answers are the plan's classification of a reload's diff, and the
/// manager is the only place that can give them: "did this change take effect"
/// depends on whether the unit has a process, which is state the manager owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigChange {
    /// The unit was not known: registered, `Inactive`.
    Added,
    /// Known and not running: the new definition is in force now.
    Applied,
    /// Known and running: the new definition waits for the next start.
    Deferred,
    /// Known, and the definition is identical.
    Unchanged,
}

/// What happened to a unit when a child was reaped.
///
/// The manager applies the change itself - it owns the state, the backoff and
/// the restart timer - so this is a *report* for logging and tests, not an
/// instruction. An earlier version returned an `Action` for the caller to
/// perform, which meant the manager had to describe work it had already done,
/// and the variants drifted out of use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A `forking` unit's main process exited and the daemon it left behind was
    /// found and taken over (plan §7.10).
    Forked {
        /// The unit.
        unit: UnitName,
        /// The daemon that is now the unit's process.
        daemon: libc::pid_t,
    },
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

/// How a restart request was carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPlan {
    /// The unit was stopped and a start follows once the exit is reaped.
    Stopping,
    /// Nothing was running, so it can simply be started.
    Start,
    /// The unit has no process to restart and restarting it is meaningless
    /// (plan §3 M3's exit code 4, e.g. `Active(Exited)`).
    NotApplicable,
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
    /// The running `ExecStop=` command's pid, if one is in flight (plan §1.1).
    pub exec_stop_pid: Option<libc::pid_t>,
    /// The last `STATUS=` text, for `rctl status`.
    pub status: Option<String>,
    /// The last outcome, kept for logging and `rctl status`.
    pub last_outcome: Option<ExitOutcome>,
    /// When an idle socket-activated unit should be stopped (plan §7.9).
    ///
    /// Set when the unit starts, and re-armed by the supervisor when a connection
    /// arrives; `0` in `IdleTimeoutSec=` means never, which is no deadline at all.
    pub idle_at: Option<Instant>,
    /// The unit cgroup's OOM-kill count when this start began, or `None` when
    /// there is no cgroup to ask (plan §7.3).
    ///
    /// A baseline rather than a bare "was it killed": the counter is the
    /// cgroup's and survives a restart when the directory had to be kept, so
    /// only a *change* across this run is this run's OOM kill.
    pub oom_kills_at_start: Option<u64>,
    /// A reload's replacement definition, waiting for the next start
    /// (plan §7.6).
    ///
    /// A running unit keeps the definition it was started with - including the
    /// `ExecStop=` that will stop it - and picks this one up at its next start.
    /// That is the plan's "运行中单元延迟到下次重启时生效", and keeping the *old*
    /// config in place until then is what makes "delayed" mean delayed.
    pub pending_config: Option<UnitConfig>,
    /// Whether a reload found this unit's file gone (plan §7.6).
    ///
    /// The runtime itself has to stay while a stop is in flight - the `SIGKILL`
    /// deadline and the reaping both need it - so removal is a tombstone: every
    /// protocol answer treats the unit as gone (`exists`), and the entry is
    /// dropped by the next reload that finds it still retired and now down.
    pub retired: bool,
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
            exec_stop_pid: None,
            status: None,
            last_outcome: None,
            idle_at: None,
            oom_kills_at_start: None,
            pending_config: None,
            retired: false,
        }
    }

    /// Whether this unit should get a process at all.
    fn spawns(&self) -> bool {
        spawns_process(&self.config)
    }

    /// Whether the unit has a process, or is on its way to not having one.
    ///
    /// Deliberately *not* "the state is active": a `oneshot` that finished is
    /// `Active(Exited)` with no process, and a reload must be free to replace its
    /// definition - there is nothing running to protect.
    ///
    /// The second half is a guard rather than a second case: every path that
    /// clears `pid` also leaves `Deactivating` (`stop_unit` goes straight to
    /// `Inactive` when there is nothing to signal, and `on_child_exit` settles
    /// the unit it just cleared), so `Deactivating` currently implies a pid. It
    /// is checked anyway because plan §7.6's rule is about a unit *tearing
    /// down*, and a future stop path that clears the pid first would otherwise
    /// let a reload swap the definition of a unit that is still stopping.
    fn is_up(&self) -> bool {
        self.pid.is_some() || self.state == UnitState::Deactivating
    }

    /// Take a reload's definition, if the unit has settled enough to adopt it
    /// (plan §7.6).
    ///
    /// Called at the start of a start, which is the only moment the two
    /// definitions could disagree in a way that matters.
    pub fn adopt_pending_config(&mut self) {
        if self.is_up() {
            return;
        }
        if let Some(config) = self.pending_config.take() {
            self.config = config;
        }
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
    (secs > 0).then(|| crate::deadline::after_secs(secs))
}

/// Arm the next health probe for `rt`, if it has a `HealthCheck` at all.
///
/// A zero interval disables probing, mirroring how `TimeoutStartSec=0` disables
/// a timeout.
fn schedule_health(rt: &mut UnitRuntime) {
    rt.health_at = if rt.config.health_check.is_empty() || rt.config.health_interval_sec == 0 {
        None
    } else {
        Some(crate::deadline::after_secs(rt.config.health_interval_sec))
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
    /// Starts queued behind a stop in progress, and who asked for each
    /// (plan §1.3(13)).
    ///
    /// The source is kept because the request is *re-made* when the stop
    /// finishes (see `due_queued_starts`), and re-making it with the wrong
    /// priority would let a queued dependency start outrank a user stop.
    queued_starts: BTreeMap<UnitName, RequestSource>,
    /// Startup order (levels flattened), reversed for shutdown (plan §1.3(8)).
    shutdown_order: Vec<UnitName>,
    /// Monotonic origin for the Job queue's timestamps.
    origin: Instant,
    /// Where units' cgroup limits are applied (plan §7.3); disabled unless the
    /// boot sequence brought a hierarchy up.
    cgroup: Cgroup,
    /// The listening socket each activated unit's service should be handed
    /// (plan §7.9), as a raw descriptor.
    ///
    /// A *mirror* of `activation::Activated`, which owns the sockets: the manager
    /// needs the number where a service is spawned, and passing it down the whole
    /// start path would mean threading the socket set through every caller of
    /// `apply_effect`. The owner keeps this in step when a socket is bound or
    /// released, and a descriptor is only ever a `dup2` source.
    listen_fds: BTreeMap<UnitName, libc::c_int>,
    /// Template units by canonical name, with their file text (plan §7.7).
    ///
    /// Held in memory rather than re-read from disk so an instance can be made
    /// after a reload replaced the directory, and so the expansion uses the text
    /// the plan was built from.
    templates: BTreeMap<String, String>,
}

impl UnitManager {
    /// An empty manager.
    pub fn new() -> Self {
        UnitManager {
            units: BTreeMap::new(),
            jobs: JobQueue::new(),
            queued_starts: BTreeMap::new(),
            shutdown_order: Vec::new(),
            origin: Instant::now(),
            cgroup: Cgroup::disabled(),
            listen_fds: BTreeMap::new(),
            templates: BTreeMap::new(),
        }
    }

    /// Record the listening socket a unit's service should be handed
    /// (plan §7.9).
    ///
    /// Called by whoever bound it; the descriptor stays `activation`'s to close.
    pub fn set_listener(&mut self, unit: &UnitName, fd: libc::c_int) {
        self.listen_fds.insert(unit.clone(), fd);
    }

    /// The descriptor to hand `unit`'s service, if it has one.
    pub fn listener_fd(&self, unit: &UnitName) -> Option<libc::c_int> {
        self.listen_fds.get(unit).copied()
    }

    /// Give the manager the directory's template units (plan §7.7).
    pub fn set_templates(&mut self, templates: BTreeMap<String, String>) {
        self.templates = templates;
    }

    /// Whether `unit` is a template - a name that exists to be instantiated and
    /// cannot be started itself (plan §7.7).
    pub fn is_template(&self, unit: &UnitName) -> bool {
        unit.is_template() && self.templates.contains_key(unit.canonical())
    }

    /// Bring a template instance into being, if it is one (plan §7.7).
    ///
    /// The expansion happens here, once, and the result is parsed as an ordinary
    /// unit: from this point on an instance is a unit like any other and nothing
    /// re-expands its text. That is the plan's "替换仅在解析期完成一次" - and it is
    /// also what makes an instance name containing a `%` harmless.
    ///
    /// # Errors
    /// The reason the instance could not be made, for the caller to report: not
    /// an instance name at all, no such template, a `%` specifier this build does
    /// not know, or a template whose expansion does not parse.
    pub fn instantiate(&mut self, unit: &UnitName) -> Result<(), String> {
        let Some(template) = unit.template() else {
            return Err(format!("{unit} is not a template instance"));
        };
        let Some(text) = self.templates.get(template.canonical()) else {
            return Err(format!("no template {}", template.canonical()));
        };
        let expanded = rinit_core::substitute(text, unit).ok_or_else(|| {
            format!(
                "{} has a %-specifier that is not %i, %n, %p or %%",
                template.canonical()
            )
        })?;
        let config = rinit_core::unit::parser::parse(unit, &expanded)
            .map_err(|e| format!("{}: {e}", template.canonical()))?;
        self.units.insert(unit.clone(), UnitRuntime::new(config));
        Ok(())
    }

    /// Give the manager the cgroup side of the boot (plan §7.3).
    ///
    /// Separate from [`new`](Self::new) so a manager without one - every unit
    /// test that does not care about limits - behaves exactly as it did before
    /// cgroups existed, rather than reaching for the host's own hierarchy.
    pub fn set_cgroup(&mut self, cgroup: Cgroup) {
        self.cgroup = cgroup;
    }

    /// Milliseconds since the manager was created.
    pub fn now_millis(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }

    /// Register a unit, or update a known one (plan §7.6).
    ///
    /// A unit that is already known keeps its runtime state - a reload must not
    /// reset a running service's pid, state or backoff - and the new definition
    /// either takes over now (nothing is running) or waits for the next start.
    /// See [`UnitRuntime::pending_config`].
    pub fn add(&mut self, config: UnitConfig) -> ConfigChange {
        let name = config.name.clone();
        let Some(rt) = self.units.get_mut(&name) else {
            self.units.insert(name, UnitRuntime::new(config));
            return ConfigChange::Added;
        };
        // The file is back (or never left): the unit is a unit again.
        rt.retired = false;
        if rt.config == config {
            // The directory now says what the unit is already running, so a
            // definition an earlier reload deferred is *withdrawn* - leaving it
            // pending would let a withdrawn change take over at the next start.
            rt.pending_config = None;
            return ConfigChange::Unchanged;
        }
        if rt.is_up() {
            rt.pending_config = Some(config);
            ConfigChange::Deferred
        } else {
            rt.config = config;
            rt.pending_config = None;
            ConfigChange::Applied
        }
    }

    /// Whether a unit is known *and* still has a unit file (plan §7.6).
    ///
    /// This, not [`get`](Self::get), is what the control protocol asks: a unit a
    /// reload removed is gone as far as `rctl` is concerned, even while its stop
    /// is still in flight and the runtime therefore still exists.
    pub fn exists(&self, unit: &UnitName) -> bool {
        self.units.get(unit).is_some_and(|rt| !rt.retired)
    }

    /// Mark `unit` as removed by a reload (plan §7.6).
    ///
    /// Hidden from every protocol answer from now on; the entry stays until its
    /// stop has landed, and the next reload drops it.
    pub fn retire(&mut self, unit: &UnitName) {
        if let Some(rt) = self.units.get_mut(unit) {
            rt.retired = true;
        }
    }

    /// Drop retired entries that have nothing left in flight (plan §7.6).
    ///
    /// Returns how many went. Called at the start of a reload, so the tombstones
    /// a previous reload left behind are collected exactly when the directory is
    /// known to be the truth again.
    pub fn prune_retired(&mut self) -> usize {
        let before = self.units.len();
        self.units.retain(|_, rt| !rt.retired || rt.is_up());
        before - self.units.len()
    }

    /// Every known unit, retired ones included, in name order.
    pub fn names(&self) -> Vec<UnitName> {
        self.units.keys().cloned().collect()
    }

    /// Whether `unit` has a process, or is on its way to not having one.
    pub fn is_up(&self, unit: &UnitName) -> bool {
        self.units.get(unit).is_some_and(UnitRuntime::is_up)
    }

    /// Read a unit.
    pub fn get(&self, unit: &UnitName) -> Option<&UnitRuntime> {
        self.units.get(unit)
    }

    /// Take a pending reload definition at the start of a start (plan §7.6).
    ///
    /// Called by the start path rather than by a state change, because "the next
    /// start" is the only moment the plan gives a new definition permission to
    /// take over from a running one.
    pub fn adopt_pending_config(&mut self, unit: &UnitName) {
        if let Some(rt) = self.units.get_mut(unit) {
            rt.adopt_pending_config();
        }
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
            // Plan §7.7: asking for an instance *is* what creates it. A failure
            // here is reported by the caller, which has somewhere to put a
            // message; `request` only has an effect to return.
            if self.instantiate(unit).is_err() {
                return None;
            }
        }
        let at = self.now_millis();
        self.forget_spent_target(unit);
        self.jobs.submit(Job::new(unit.clone(), target, source, at));
        let effect = self.effect_for(unit);

        // A satisfied request can still have a state to reach with no side
        // effect at all - `Stopped` against `Active(Exited)` or `Failed` lands
        // in `Inactive` (plan §1.3(13)). The manager owns state, so it applies
        // that part here rather than leaving the unit parked in a state the
        // caller has no way to fix.
        if effect == Some(Effect::None) {
            self.apply_none_transition(unit);
        }

        match target {
            // Plan §1.3(13): a start against a stop in progress waits for it.
            // That is only knowable here, so remember it; `due_queued_starts`
            // picks it up once the stop finishes.
            JobTarget::Started => {
                if effect.is_none() && self.is_deactivating(unit) {
                    self.queued_starts.insert(unit.clone(), source);
                }
            }
            // An explicit stop beats a start that was still queued.
            JobTarget::Stopped => {
                self.queued_starts.remove(unit);
            }
        }
        effect
    }

    /// Apply the state part of a request that needs no side effect.
    fn apply_none_transition(&mut self, unit: &UnitName) {
        let Some(actual) = self.units.get(unit).map(|rt| rt.state) else {
            return;
        };
        if let Some((Effect::None, to)) = self.jobs.reconcile(unit, actual)
            && let Some(rt) = self.units.get_mut(unit)
        {
            rt.state = to;
        }
    }

    fn is_deactivating(&self, unit: &UnitName) -> bool {
        self.units
            .get(unit)
            .is_some_and(|rt| rt.state == UnitState::Deactivating)
    }

    /// Starts that were queued behind a stop which has since finished.
    ///
    /// Plan §1.3(13): the start is not applied to a unit that is still tearing
    /// down, so it is applied here, on the first pass after the stop completes.
    pub fn due_queued_starts(&mut self) -> Vec<UnitName> {
        let queued: Vec<(UnitName, RequestSource)> = self
            .queued_starts
            .iter()
            .map(|(unit, source)| (unit.clone(), *source))
            .collect();
        let mut ready = Vec::new();
        for (unit, source) in queued {
            // Still stopping: leave it queued.
            if self.is_deactivating(&unit) {
                continue;
            }
            self.queued_starts.remove(&unit);
            // The start is *re-made* rather than read back from the target. A
            // stop that came from a request leaves the merged target at
            // `Stopped`, and a start does not outrank a user stop
            // (plan §1.3(11)) - so asking again, now that the stop is done and
            // its target is spent, is what makes the queued start happen.
            if self.request(&unit, JobTarget::Started, source) == Some(Effect::SpawnProcess) {
                ready.push(unit);
            }
        }
        ready
    }

    /// Forget a target the unit already satisfies (plan §1.3(13)).
    ///
    /// `JobQueue`'s precedence is about requests that are still *outstanding*,
    /// and a target the unit has reached is not outstanding. Without this it
    /// would veto every later request from a lower-priority source, forever:
    /// `rctl stop app.service` is a `UserStop`, `rctl start` is a `UserStart`,
    /// and a user start does not outrank a user stop - so the unit could never
    /// be started again. That is a bug, not a feature of the priority order.
    fn forget_spent_target(&mut self, unit: &UnitName) {
        let Some(state) = self.units.get(unit).map(|rt| rt.state) else {
            return;
        };
        if let Some((Effect::None, _)) = self.jobs.reconcile(unit, state) {
            self.jobs.forget(unit);
        }
    }

    /// Restart `unit`: stop it if it is running, then start it again.
    ///
    /// A running unit is stopped with `restart_after_stop` set, which is the
    /// same mechanism a health-triggered restart uses: the reaped exit
    /// re-enters the backoff path instead of ending the unit (plan §1.3(3)).
    pub fn restart_unit(&mut self, unit: &UnitName) -> RestartPlan {
        let Some(rt) = self.units.get(unit) else {
            return RestartPlan::NotApplicable;
        };
        match rt.state {
            // There is no process and no work to redo: plan §3 M3 calls this a
            // conflict rather than silently doing nothing.
            UnitState::ActiveExited => return RestartPlan::NotApplicable,
            UnitState::Active
            | UnitState::Starting
            | UnitState::RestartWait
            | UnitState::Deactivating => {}
            // Already down: a restart is just a start.
            UnitState::Inactive | UnitState::Failed(_) => return RestartPlan::Start,
        }

        self.stop_unit(unit);
        let Some(rt) = self.units.get_mut(unit) else {
            return RestartPlan::NotApplicable;
        };
        if rt.state == UnitState::Deactivating {
            rt.restart_after_stop = true;
            RestartPlan::Stopping
        } else {
            // `stop_unit` found nothing to signal (the process was already
            // gone), so there is nothing to wait for.
            RestartPlan::Start
        }
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

    /// The merged target for `unit`, if a request set one.
    pub fn target_of(&self, unit: &UnitName) -> Option<JobTarget> {
        self.jobs.target_of(unit).map(|merged| merged.target)
    }

    /// Clear a unit's failure state and breaker without starting it.
    ///
    /// Plan §1.3(13)'s `reset-failed` escape hatch: unlike a manual start, this
    /// does not immediately bring the unit back up.
    pub fn reset_failed(&mut self, unit: &UnitName) {
        if let Some(rt) = self.units.get_mut(unit) {
            rt.backoff.reset();
            if matches!(rt.state, UnitState::Failed(_)) {
                rt.state = UnitState::Inactive;
            }
        }
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

    /// Record the *startup* order, for shutdown to walk backwards.
    ///
    /// Boot passes the schedule as it started it (levels flattened); the
    /// manager owns the reversal so the "reverse topological" rule of
    /// plan §1.3(8) lives in one place.
    pub fn set_shutdown_order(&mut self, order: Vec<UnitName>) {
        self.shutdown_order = order;
    }

    /// Units to stop at shutdown: dependents before the units they depend on.
    ///
    /// Plan §1.3(8) stops in reverse topological order, so a service is taken
    /// down before whatever it was started for. The startup order recorded at
    /// boot is walked backwards and filtered to what the queue still wants
    /// running; anything started but missing from it is appended, so no unit
    /// can be left behind.
    pub fn shutdown_order(&self) -> Vec<UnitName> {
        let started = self.started_units();
        let mut out: Vec<UnitName> = Vec::new();
        for unit in self.shutdown_order.iter().rev() {
            if started.contains(unit) && !out.contains(unit) {
                out.push(unit.clone());
            }
        }
        for unit in started {
            if !out.contains(&unit) {
                out.push(unit);
            }
        }
        self.apply_conflict_order(&mut out);
        out
    }

    /// Adjust a shutdown sequence so `Conflicts=` pairs tear down in order.
    ///
    /// Plan §1.3(8): shutdown stops the *superseded* unit before the one that
    /// supersedes it, mirroring how starting the pair resolved it. A
    /// contradictory pair (each conflicting with the other) cannot be ordered,
    /// so the pass is bounded and simply leaves the last arrangement.
    fn apply_conflict_order(&self, order: &mut Vec<UnitName>) {
        for _ in 0..order.len() {
            let mut changed = false;
            for index in 0..order.len() {
                let conflicts = match order.get(index).and_then(|name| self.units.get(name)) {
                    Some(rt) => rt.config.deps.conflicts.clone(),
                    // A name in the order with no runtime is dropped when the
                    // order is rebuilt, so skipping it is the same as ignoring
                    // a unit that is no longer loaded.
                    None => continue,
                };
                for other in conflicts {
                    if let Some(other_index) = order.iter().position(|u| u == &other)
                        && other_index > index
                    {
                        let superseded = order.remove(other_index);
                        order.insert(index, superseded);
                        changed = true;
                        break;
                    }
                }
            }
            if !changed {
                break;
            }
        }
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

    /// Stop every unit `unit` mutually excludes, so that it can start.
    ///
    /// Plan §1.1: `Conflicts=` means "stop the other one first". This only
    /// touches units with a live process, and it returns them so the caller can
    /// wait for them to actually go away before spawning `unit`.
    pub fn stop_conflicts(&mut self, unit: &UnitName) -> Vec<UnitName> {
        let conflicts = match self.units.get(unit) {
            Some(rt) => rt.config.deps.conflicts.clone(),
            None => return Vec::new(),
        };
        let mut stopped = Vec::new();
        for other in conflicts {
            // A conflicting unit that is not running is already resolved.
            if self.units.get(&other).is_some_and(|rt| rt.pid.is_some()) {
                self.stop_unit(&other);
                stopped.push(other);
            }
        }
        stopped
    }

    /// Cap every unit's `TimeoutStopSec` at `secs`.
    ///
    /// Power failure gets almost no time to stop (plan §7.1), so the shutdown
    /// path compresses each unit's budget before issuing its stop.
    pub fn compress_stop_timeouts(&mut self, secs: u64) {
        for rt in self.units.values_mut() {
            rt.config.timeout_stop_sec = rt.config.timeout_stop_sec.min(secs);
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

        // `Requires=` is transitive: a target that hard-requires another target
        // must fail when the service underneath it fails, not only when the
        // immediate dependent does. Walk the reverse edges breadth-first. A
        // dependent with a process is left alone (the documented "running
        // dependents are not cascaded" decision), and a visited set keeps a
        // malformed cycle from looping forever.
        let mut failed = Vec::new();
        let mut visited = std::collections::BTreeSet::new();
        let mut queue = vec![unit.clone()];
        visited.insert(unit.clone());
        while let Some(current) = queue.pop() {
            for dependent in self.requires_dependents(&current) {
                let Some(rt) = self.units.get_mut(&dependent) else {
                    continue;
                };
                if rt.pid.is_some() {
                    continue;
                }
                if !matches!(rt.state, UnitState::Failed(_)) {
                    rt.state = UnitState::Failed(FailedReason::StartFailure);
                    failed.push(dependent.clone());
                }
                if visited.insert(dependent.clone()) {
                    queue.push(dependent);
                }
            }
        }
        failed
    }

    /// Mark a process-less target as reached after its level has settled.
    ///
    /// Targets are filtered out of [`commands_in_level`](crate::manager::commands_in_level)
    /// because they spawn nothing, so the boot loop has to complete them
    /// explicitly (plan §7.2). A target whose hard dependency already failed is
    /// left `Failed` rather than being overwritten with `Active`; failing it
    /// again propagates to anything that requires the target.
    pub fn reach_target(&mut self, unit: &UnitName) -> UnitState {
        let failed = match self.units.get(unit) {
            Some(rt) => {
                debug_assert!(rt.config.is_target(), "{unit} is not a target");
                matches!(rt.state, UnitState::Failed(_))
                    || rt
                        .config
                        .deps
                        .requires
                        .iter()
                        .any(|dependency| self.is_failed(dependency))
            }
            None => return UnitState::Inactive,
        };

        if failed {
            self.fail_start(unit);
            return UnitState::Failed(FailedReason::StartFailure);
        }

        if let Some(rt) = self.units.get_mut(unit) {
            rt.state = UnitState::Active;
        }
        UnitState::Active
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
    pub fn spawn_unit_with_output(
        &mut self,
        unit: &UnitName,
        extra_env: &[String],
        output: Option<libc::c_int>,
        listen_fd: Option<libc::c_int>,
    ) -> Result<(), RuntimeError> {
        let now = self.now_millis();

        // Everything the spawn needs, read through an immutable borrow so the
        // `EnvironmentFile=` read below cannot race the state update.
        let (argv, env, cwd, kind, sandbox, limits, tty_path, tty_reset, idle_secs) = {
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
                // `User=`/`Group=`/`NoNewPrivileges=` (plan §7.4). A name that
                // does not resolve fails the start here, before anything is
                // forked: running the service as root instead would be the one
                // outcome worse than not starting it.
                crate::sandbox::resolve(&rt.config)?,
                // The cgroup limits the unit asked for (plan §7.3).
                CgroupLimits::from_config(&rt.config),
                // The controlling terminal, if the unit asked for one
                // (plan §7.11): the path and whether to reset it.
                rt.config.tty_path.clone(),
                rt.config.tty_reset,
                // Plan §7.9: the idle clock starts when the service does, and
                // only a unit that asked to be stopped for being idle has one.
                rt.config
                    .socket_activate
                    .as_ref()
                    .filter(|_| rt.config.idle_timeout_sec > 0)
                    .map(|_| rt.config.idle_timeout_sec),
            )
        };

        // Plan §7.3: the cgroup and its limits are in place *before* the fork,
        // so the service is bounded from its first instruction rather than some
        // time after it started working. Only a failed limit write fails the
        // start; an unusable hierarchy degrades to a warning inside `setup`.
        let cgroup_procs = if self.cgroup.is_enabled() && !limits.is_empty() {
            match self.cgroup.setup(unit, &limits) {
                Ok(procs) => procs,
                Err(e) => {
                    // Nothing was spawned, so nothing can be in the cgroup: take
                    // the half-configured directory back down rather than leave
                    // it for a unit that never started.
                    self.cgroup.cleanup(unit);
                    return Err(e);
                }
            }
        } else {
            None
        };

        // Plan §7.3's OOM detection needs a baseline from *before* the service
        // could allocate anything, so it is read here - after the setup, with
        // nothing spawned yet. A unit in its own cgroup is the only one whose
        // counter means anything: without one it is running in rinit's.
        let oom_kills_at_start = cgroup_procs
            .is_some()
            .then(|| self.cgroup.oom_kills(unit))
            .flatten();

        // Plan §7.11: the terminal path goes through the same C-string
        // conversion as every other path rinit opens, so an interior NUL is a
        // failed start rather than a silently different device. The `CString`
        // outlives the spawn because the child borrows it.
        let tty_cpath = tty_path
            .as_deref()
            .map(|path| rinit_runtime::syscall::cstring("TTYPath", path))
            .transpose()?;
        let tty = tty_cpath.as_ref().map(|path| process::ChildTty {
            path: path.as_c_str(),
            reset: tty_reset,
        });

        // A fresh process group, so a stop reaches the whole tree (plan §1.3(6)).
        let pid = spawn_service_in_group(
            &argv,
            &env,
            Some(&cwd),
            output,
            &sandbox,
            crate::service::ChildPlacement {
                cgroup_procs: cgroup_procs.as_deref(),
                tty,
                listen_fd,
            },
        )?;

        let Some(rt) = self.units.get_mut(unit) else {
            return Ok(());
        };
        rt.pid = Some(pid);
        rt.state = UnitState::Starting;
        rt.restart_at = None;
        rt.kill_at = None;
        rt.exec_stop_pid = None;
        rt.oom_kills_at_start = oom_kills_at_start;
        rt.idle_at = idle_secs.and_then(deadline);

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
            // `forking` waits for the main process to exit *and* for the daemon
            // it leaves behind to be located (plan §7.10), so it is `Starting`
            // with the same deadline a `notify` unit gets: a daemon that never
            // detaches is a hung start, not a running service.
            ServiceKind::Forking => {
                rt.start_deadline = deadline(rt.config.timeout_start_sec);
            }
        }
        rlog!("started {unit} as pid {pid}");
        Ok(())
    }

    /// Re-arm an idle unit's deadline, or clear it (plan §7.9).
    ///
    /// Called when a connection arrives: a unit that is being used is not idle,
    /// and the clock starts again from the last thing that happened.
    pub fn note_connection(&mut self, unit: &UnitName) {
        if let Some(rt) = self.units.get_mut(unit) {
            rt.idle_at =
                deadline(rt.config.idle_timeout_sec).filter(|_| rt.config.idle_timeout_sec > 0);
        }
    }

    /// Units whose idle deadline has passed (plan §7.9).
    ///
    /// The deadline is cleared here and the *decision* is left to the caller: one
    /// that fires while something is queued on the socket means the unit is
    /// working, not idle, and the caller re-arms it.
    pub fn due_idle_units(&mut self) -> Vec<UnitName> {
        let now = Instant::now();
        let mut due = Vec::new();
        for (name, rt) in self.units.iter_mut() {
            if let Some(at) = rt.idle_at
                && at <= now
            {
                rt.idle_at = None;
                due.push(name.clone());
            }
        }
        due
    }

    /// Whether the unit is started: a socket-activated one is not, until a
    /// connection arrives (plan §7.9).
    pub fn is_running(&self, unit: &UnitName) -> bool {
        self.units.get(unit).is_some_and(|rt| rt.pid.is_some())
    }

    /// Become `Active` with `daemon` as the unit's process (plan §7.10).
    ///
    /// The boot loop waits for a `forking` unit's main process itself rather than
    /// through the reaper, so the step that takes over from it has to be callable
    /// from both paths.
    pub fn mark_daemon_started(&mut self, unit: &UnitName, daemon: libc::pid_t) {
        let now = self.now_millis();
        if let Some(rt) = self.units.get_mut(unit) {
            rt.pid = Some(daemon);
            rt.state = UnitState::Active;
            rt.start_deadline = None;
            rt.idle_at = None;
            rt.backoff.on_active(now);
            schedule_health(rt);
        }
    }

    /// Send `signal` to everything still in `unit`'s cgroup.
    ///
    /// §7.3's second stage, exposed for the one path that abandons a start
    /// without going through `stop_unit`: boot's wait for a `forking` unit's main
    /// process, which times out into `fail_unit` and would otherwise leave the
    /// daemon that unit had already started behind. A unit with no cgroup of its
    /// own has nothing to sweep.
    pub fn signal_cgroup_members(&self, unit: &UnitName, signal: libc::c_int) {
        self.cgroup.signal_members(unit, signal);
    }

    /// Take over from a `forking` unit's main process (plan §7.10).
    ///
    /// The boot loop waits for that process itself - boot ordering cannot depend
    /// on the reaper's loop - so this is the same step `on_child_exit` takes,
    /// callable from there. On failure the unit has not been changed: the caller
    /// fails the start, because "locate the daemon" has no partial success.
    ///
    /// # Errors
    /// The reason the daemon could not be located, for the unit's failure line.
    pub fn finish_forking_start(&mut self, unit: &UnitName) -> Result<libc::pid_t, String> {
        let Some(rt) = self.units.get(unit) else {
            return Err(format!("{unit} is not registered"));
        };
        let daemon = find_daemon(&rt.config, &self.cgroup, unit)?;
        self.mark_daemon_started(unit, daemon);
        Ok(daemon)
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
                // Plan §7.3's first stage reaches whatever the process group
                // cannot, which for a `forking` unit that never detached is the
                // daemon it started before hanging (plan §7.10).
                self.cgroup.signal_members(name, libc::SIGTERM);
                if let Some(pid) = rt.pid {
                    match signal_process(pid, libc::SIGTERM) {
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
    /// Returns the units whose process group was signalled, in name order. An
    /// in-flight `ExecStop=` command is killed too: a stuck stop command must
    /// not keep the unit alive past its timeout.
    pub fn due_kills(&mut self) -> Vec<UnitName> {
        let now = Instant::now();
        let mut due = Vec::new();
        for (name, rt) in self.units.iter_mut() {
            if let Some(at) = rt.kill_at
                && at <= now
            {
                rt.kill_at = None;
                if let Some(stop_pid) = rt.exec_stop_pid.take() {
                    let _ = process::kill_process_group(stop_pid, libc::SIGKILL);
                }
                // Plan §7.3's second stage: whatever the group signal missed,
                // the cgroup still names, and this is the point where it is no
                // longer asked politely. Sent before the group kill so a
                // daemonized straggler is reached in the same pass.
                self.cgroup.signal_members(name, libc::SIGKILL);
                if let Some(pid) = rt.pid {
                    match signal_process(pid, libc::SIGKILL) {
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
    /// Time until the next idle deadline, if any (plan §7.9).
    pub fn next_idle_in(&self) -> Option<Duration> {
        let now = Instant::now();
        self.units
            .values()
            .filter_map(|rt| rt.idle_at)
            .map(|at| at.saturating_duration_since(now))
            .min()
    }

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
            // working directory, same user (plan §7.4).
            let env = build_service_env(&rt.config, &[]);
            let cwd = rt.config.working_directory.clone();
            let sandbox = match crate::sandbox::resolve(&rt.config) {
                Ok(sandbox) => sandbox,
                Err(e) => {
                    // A probe whose user cannot be resolved cannot run either;
                    // like a probe that failed to start, it waits a whole
                    // interval rather than spinning.
                    rlog!("warning: {name}: health probe: {e}");
                    schedule_health(rt);
                    continue;
                }
            };
            // A probe runs *outside* the unit's cgroup, so `cgroup_procs` is
            // `None`. It is a separate process whose pid rinit tracks itself
            // (`probe_pid`), and having it join would keep the unit's cgroup
            // populated across the probe - which is exactly the state plan §7.3
            // reads as "still running, do not remove this directory".
            match spawn_service_in_group(
                &argv,
                &env,
                Some(&cwd),
                None,
                &sandbox,
                crate::service::ChildPlacement::default(),
            ) {
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

    /// The unit whose `ExecStop=` command `pid` belongs to, if any.
    fn exec_stop_owner(&self, pid: libc::pid_t) -> Option<UnitName> {
        self.units
            .iter()
            .find(|(_, rt)| rt.exec_stop_pid == Some(pid))
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
        if rt.config.health_restart_on_failure
            && let Some(pid) = rt.pid
        {
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
                    // Its exit will not come back through `on_child_exit` (the
                    // pid is cleared here), so this is the last chance to take
                    // its cgroup down.
                    self.cgroup.cleanup(unit);
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

    /// Stop a running unit.
    ///
    /// A pending restart is cancelled: an explicit stop beats an automatic one
    /// (plan §1.3(13)).
    ///
    /// `ExecStop=`, when set, *is* the stop action; otherwise `SIGTERM` goes to
    /// the process group (plan §1.1). Either way the stop is two-stage: the
    /// `SIGKILL` deadline is armed at `TimeoutStopSec` (immediately, when it is
    /// `0`) and [`due_kills`](Self::due_kills) fires it. Without that, a service
    /// that ignores the stop would never be killed.
    pub fn stop_unit(&mut self, unit: &UnitName) {
        // Plan §7.3's stop flow starts here: `SIGTERM` to every pid in the
        // unit's cgroup, not only to its process group. A service that called
        // `setsid` itself is out of the group but still in the cgroup, and the
        // process-group signal below would miss it. Safe to do before the state
        // update: signalling cannot reorder the manager's own bookkeeping.
        self.cgroup.signal_members(unit, libc::SIGTERM);

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

        let Some(pid) = rt.pid else {
            // No process: an `Active(Exited)` oneshot and anything already down
            // both land straight in Inactive (plan §1.3(3) skips Deactivating
            // when there is nothing to signal).
            rt.state = UnitState::Inactive;
            // Plan §7.3's cleanup. There is no process left to have put anything
            // in the cgroup, but a stray process from an earlier stop can be
            // there, and `cleanup` keeps the directory if so.
            self.cgroup.cleanup(unit);
            return;
        };

        let mut stopping = false;
        let stop_cmd = rt.config.exec_stop.argv().to_vec();
        if stop_cmd.is_empty() {
            match signal_process(pid, libc::SIGTERM) {
                Ok(()) => {
                    rt.state = UnitState::Deactivating;
                    stopping = true;
                    rlog!("stopping {unit} (pid {pid})");
                }
                Err(e) if e.errno() == libc::ESRCH => {
                    // Already gone: nothing to signal, so the stop completes
                    // now rather than waiting for a SIGCHLD that has already
                    // been consumed.
                    rt.pid = None;
                    rt.state = UnitState::Inactive;
                    self.cgroup.cleanup(unit);
                }
                Err(e) => rlog!("warning: SIGTERM to {unit} failed: {e}"),
            }
        } else {
            let env = build_service_env(&rt.config, &[]);
            let cwd = rt.config.working_directory.clone();
            // `ExecStop=` runs as the service user too (plan §7.4). If its user
            // cannot be resolved, this falls back to the signal below like any
            // other ExecStop that cannot start.
            let started = match crate::sandbox::resolve(&rt.config) {
                // `cgroup_procs` is `None` for the same reason as a health
                // probe: a stop command joining the unit's cgroup would make the
                // directory look populated for as long as it runs, and plan
                // §7.3 reads a populated directory as a reason to keep it.
                Ok(sandbox) => spawn_service_in_group(
                    &stop_cmd,
                    &env,
                    Some(&cwd),
                    None,
                    &sandbox,
                    crate::service::ChildPlacement::default(),
                ),
                Err(e) => Err(e),
            };
            match started {
                Ok(stop_pid) => {
                    rt.exec_stop_pid = Some(stop_pid);
                    rt.state = UnitState::Deactivating;
                    stopping = true;
                    rlog!("stopping {unit} (ExecStop pid {stop_pid})");
                }
                Err(e) => {
                    // A stop command that cannot run must not strand the unit:
                    // fall back to the signal.
                    rlog!("warning: {unit}: cannot start ExecStop: {e}; sending SIGTERM");
                    match process::kill_process_group(pid, libc::SIGTERM) {
                        Ok(()) => {
                            rt.state = UnitState::Deactivating;
                            stopping = true;
                        }
                        Err(e) if e.errno() == libc::ESRCH => {
                            rt.pid = None;
                            rt.state = UnitState::Inactive;
                            self.cgroup.cleanup(unit);
                        }
                        Err(e) => rlog!("warning: SIGTERM to {unit} failed: {e}"),
                    }
                }
            }
        }

        if stopping {
            if rt.config.timeout_stop_sec == 0 {
                // Plan §1.1: `TimeoutStopSec=0` means SIGKILL at once.
                let _ = process::kill_process_group(pid, libc::SIGKILL);
            } else {
                rt.kill_at = Some(crate::deadline::after_secs(rt.config.timeout_stop_sec));
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
        // So is an `ExecStop=` command: its exit is not the service exiting.
        if let Some(unit) = self.exec_stop_owner(pid) {
            if let Some(rt) = self.units.get_mut(&unit) {
                rt.exec_stop_pid = None;
            }
            rlog!("{unit}: ExecStop finished");
            return None;
        }

        let unit = self
            .units
            .iter()
            .find(|(_, rt)| rt.pid == Some(pid))
            .map(|(name, _)| name.clone())?;

        // Plan §7.10: a `forking` unit's main process exiting 0 is the *middle*
        // of its start, not the end of it - so the daemon it left behind is
        // located before the ordinary exit handling can settle the unit. It is
        // done here, before the runtime is borrowed mutably, because locating it
        // reads the unit's config and its cgroup.
        let forked = self
            .units
            .get(&unit)
            .filter(|rt| {
                rt.state == UnitState::Starting
                    && rt.config.service_kind == ServiceKind::Forking
                    && matches!(outcome, ExitOutcome::Success)
            })
            .map(|rt| find_daemon(&rt.config, &self.cgroup, &unit));
        if let Some(result) = forked {
            match result {
                Ok(daemon) => {
                    self.mark_daemon_started(&unit, daemon);
                    return Some(Outcome::Forked { unit, daemon });
                }
                Err(why) => {
                    rlog!("error: {unit}: cannot locate the daemon: {why}");
                    // And on to the ordinary failed-start handling below, so
                    // `Restart=` still decides whether to try again.
                }
            }
        }

        let now = self.now_millis();
        let rt = self.units.get_mut(&unit)?;
        let oom_baseline = rt.oom_kills_at_start.take();
        rt.pid = None;
        rt.start_deadline = None;
        rt.kill_at = None;
        rt.health_at = None;
        rt.probe_deadline = None;
        rt.last_outcome = Some(outcome);

        // Plan §7.3's OOM link, and it belongs on *every* exit path rather than
        // only the ones that restart. `SIGKILL` is also how a stop timeout and
        // an outside `kill -9` arrive, so the kernel's own counter is what says
        // which one this was - and when it was the cgroup's killer, the memory
        // limit that caused it is the thing an operator needs to see, whether
        // the unit comes back or not.
        if matches!(outcome, ExitOutcome::Signalled(libc::SIGKILL))
            && let Some(before) = oom_baseline
            && let Some(after) = self.cgroup.oom_kills(&unit)
            && after > before
        {
            let limit = CgroupLimits::from_config(&rt.config)
                .files()
                .into_iter()
                .find(|(_, file, _)| *file == "memory.max")
                .map(|(_, _, value)| value);
            match limit {
                Some(value) => {
                    rlog!("{unit} was killed by the cgroup OOM killer (memory.max={value})");
                }
                None => rlog!("{unit} was killed by the cgroup OOM killer"),
            }
        }

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
            // The stop is complete, so plan §7.3's cleanup runs now: an empty
            // directory goes, one that still holds a stray process is kept.
            self.cgroup.cleanup(&unit);
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
                self.cgroup.cleanup(&unit);
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
            // A completed oneshot is done, so its cgroup has no reason to stay.
            self.cgroup.cleanup(&unit);
            return Some(Outcome::Exited {
                unit,
                state: UnitState::ActiveExited,
            });
        }

        if !rt.policy().wants_restart(outcome) {
            let state = rt.state;
            // No restart is coming, so this is where the unit came to rest.
            self.cgroup.cleanup(&unit);
            // A service that failed at run time has to fail whatever hard
            // `Requires=` it just the same as a failed start does (plan §1.3(13),
            // §7.2). Targets have no process, so `fail_start` covers them.
            if matches!(state, UnitState::Failed(_)) {
                for dependent in self.fail_start(&unit) {
                    rlog!("error: {dependent} requires {unit}, which failed");
                }
            }
            return Some(Outcome::Exited { unit, state });
        }
        let outcome = self.schedule_restart(&unit, now);
        if matches!(outcome, Outcome::StartLimitTripped { .. }) {
            for dependent in self.fail_start(&unit) {
                rlog!("error: {dependent} requires {unit}, which failed");
            }
        }
        Some(outcome)
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
                rt.restart_at = Some(crate::deadline::after(delay));
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
    ///
    /// Retired units are left out: a reload removed them, and `rctl list` must
    /// agree with `rctl start` about which units exist (plan §7.6).
    pub fn summary(&self) -> Vec<String> {
        let templates = self
            .templates
            .keys()
            .map(|name| format!("{name}: template"));
        let units = self.units.values().filter(|rt| !rt.retired).map(|rt| {
            let pid = match rt.pid {
                Some(p) => format!(" pid {p}"),
                None => String::new(),
            };
            format!("{}: {}{pid}", rt.config.name, rt.state)
        });
        // A template is not a unit, so it has no state - but an operator looking
        // for the unit they want to start needs to see that it exists.
        units.chain(templates).collect()
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
/// Send `signal` to a unit's process: its process group, or the pid itself.
///
/// Both are needed, and `Type=forking` (plan §7.10) is why: the daemon a forking
/// unit leaves behind *is* the unit's process, but it inherited its process group
/// from whatever forked it - usually a shell that has since exited - so the group
/// signal addresses a group that no longer exists while the daemon is very much
/// alive. Falling back to the pid costs one syscall on a path that has already
/// failed, and it is what keeps a unit's own process inside the unit.
fn signal_process(pid: libc::pid_t, signal: libc::c_int) -> Result<(), SyscallError> {
    match process::kill_process_group(pid, signal) {
        Ok(()) => Ok(()),
        Err(e) if e.errno() == libc::ESRCH => process::kill(pid, signal),
        Err(e) => Err(e),
    }
}

/// Locate the daemon a `Type=forking` unit left behind (plan §7.10).
///
/// Two ways, in the plan's order. The `PIDFile=` the daemon wrote is the
/// primary one; without it, the unit's *own cgroup* is the only thing that
/// knows, and only if it holds exactly one process - "无法通过 cgroup 唯一确定守护进程
/// 时,启动判 Failed".
///
/// Either way the answer is checked before it is believed: a pid that is not
/// alive is a stale file rather than a daemon, and one that is not in the unit's
/// cgroup is *moved* there, because that is what makes the unit's limits, its
/// stop signalling and its cleanup apply to it.
///
/// Every failure is a reason string and the caller fails the start. Guessing -
/// picking the youngest process, say - would give a unit that claims to be
/// running with nothing behind it, which is the one outcome the plan's
/// fail-closed rule exists to prevent.
fn find_daemon(
    config: &UnitConfig,
    cgroup: &Cgroup,
    unit: &UnitName,
) -> Result<libc::pid_t, String> {
    let pid = match &config.pid_file {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read PIDFile {path}: {e}"))?;
            let text = text.trim();
            text.parse::<libc::pid_t>()
                .map_err(|_| format!("PIDFile {path} does not hold a pid ('{text}')"))?
        }
        None => {
            // No `PIDFile=`: the cgroup is all there is, and a unit with no
            // cgroup of its own has nothing to look at.
            let members = cgroup.members(unit).ok_or_else(|| {
                format!(
                    "{unit} has no PIDFile= and no cgroup of its own; a Type=forking unit \
                     needs one or the other"
                )
            })?;
            match members.as_slice() {
                [only] => *only,
                other => {
                    return Err(format!(
                        "its cgroup holds {} process(es), so the daemon is not uniquely \
                         determined",
                        other.len()
                    ));
                }
            }
        }
    };

    if pid <= 0 {
        return Err(format!("pid {pid} is not a process"));
    }
    // SAFETY: signal 0 only probes whether the pid exists.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return Err(format!("pid {pid} is not running"));
    }
    if let Some(members) = cgroup.members(unit)
        && !members.contains(&pid)
    {
        cgroup.place(unit, pid).map_err(|e| {
            format!("pid {pid} is not in the unit's cgroup and cannot be moved: {e}")
        })?;
    }
    Ok(pid)
}

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

    /// A scratch directory standing in for the mounted cgroup v2 hierarchy, with
    /// one unit's cgroup inside it.
    ///
    /// `cgroup.procs` is an ordinary file here, and `members`/`place` only ever
    /// read or write that one, so the real code runs against it unchanged - the
    /// same stand-in `cgroup.rs`'s tests use.
    struct FakeCgroup {
        root: std::path::PathBuf,
    }

    impl FakeCgroup {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "rinit-unit-manager-cgroup-{}-{tag}",
                std::process::id()
            ));
            std::fs::remove_dir_all(&root).ok();
            std::fs::create_dir_all(&root).expect("scratch cgroup root");
            FakeCgroup { root }
        }

        /// The unit's cgroup directory, created if it is not there.
        fn unit_dir(&self, unit: &str) -> std::path::PathBuf {
            let dir = self
                .root
                .join(CgroupLimits::path_for(&UnitName::parse(unit).unwrap()));
            std::fs::create_dir_all(&dir).expect("unit cgroup directory");
            dir
        }

        fn cgroup(&self) -> Cgroup {
            Cgroup::at(&self.root)
        }

        /// What `cgroup.procs` says, as a trimmed string.
        fn procs(&self, unit: &str) -> String {
            std::fs::read_to_string(self.unit_dir(unit).join("cgroup.procs"))
                .unwrap_or_default()
                .trim()
                .to_string()
        }

        /// Write `pids` into the unit's `cgroup.procs`.
        fn set_procs(&self, unit: &str, pids: &[libc::pid_t]) {
            let text: String = pids.iter().map(|pid| format!("{pid}\n")).collect();
            std::fs::write(self.unit_dir(unit).join("cgroup.procs"), text)
                .expect("write cgroup.procs");
        }
    }

    impl Drop for FakeCgroup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Plan §7.3's stop flow signals `cgroup.procs` as well as the process group:
    /// a service that called `setsid` is out of its process group but still in
    /// the cgroup, and this is the signal that reaches it.
    #[test]
    fn a_cgroup_signal_reaches_a_process_that_left_its_process_group() {
        use std::os::unix::process::ExitStatusExt;

        let _guard = crate::test_support::children_lock();
        let fake = FakeCgroup::new("signal");
        let unit = UnitName::parse("app.service").unwrap();

        // A real process, in the unit's cgroup only - which is exactly what
        // `setsid` leaves behind.
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn a stand-in for the stray service");
        fake.set_procs("app.service", &[child.id() as libc::pid_t]);

        let mut m = UnitManager::new();
        m.set_cgroup(fake.cgroup());
        m.signal_cgroup_members(&unit, libc::SIGKILL);

        let status = child.wait().expect("the signalled process must exit");
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "the pid named in cgroup.procs is the one that must be signalled"
        );
    }

    /// A unit with no cgroup of its own is the normal case for a unit without
    /// limits, and signalling it is a no-op rather than an error.
    #[test]
    fn a_cgroup_signal_without_a_cgroup_is_a_no_op() {
        let m = UnitManager::new();
        m.signal_cgroup_members(&UnitName::parse("app.service").unwrap(), libc::SIGKILL);
    }

    /// Plan §7.10's PIDFile rules, one at a time: the daemon must exist, and one
    /// that is not already in the unit's cgroup is *moved* into it - being in the
    /// cgroup is what makes the unit's limits, its stop signalling and its
    /// cleanup apply to it.
    #[test]
    fn a_located_daemon_outside_its_cgroup_is_moved_into_it() {
        let fake = FakeCgroup::new("place");
        let unit = UnitName::parse("app.service").unwrap();
        let mut cfg = config("app.service", RestartPolicy::No);
        let pid_file = fake.root.join("app.pid");
        // rinit's own pid: it exists, and `find_daemon` only probes it with
        // signal 0, so nothing is signalled.
        let pid = std::process::id() as libc::pid_t;
        std::fs::write(&pid_file, format!("{pid}\n")).expect("pid file");
        cfg.pid_file = Some(pid_file.display().to_string());
        fake.set_procs("app.service", &[]);

        let located = find_daemon(&cfg, &fake.cgroup(), &unit).expect("the daemon is found");

        assert_eq!(located, pid);
        assert_eq!(
            fake.procs("app.service"),
            pid.to_string(),
            "a daemon outside its cgroup must be moved into it"
        );
    }

    /// A daemon that is already in the cgroup is left alone: writing the same pid
    /// back is a no-op the kernel does not need.
    #[test]
    fn a_daemon_already_in_its_cgroup_is_not_moved() {
        let fake = FakeCgroup::new("already");
        let unit = UnitName::parse("app.service").unwrap();
        let mut cfg = config("app.service", RestartPolicy::No);
        let pid_file = fake.root.join("app.pid");
        let pid = std::process::id() as libc::pid_t;
        std::fs::write(&pid_file, format!("{pid}\n")).expect("pid file");
        cfg.pid_file = Some(pid_file.display().to_string());
        fake.set_procs("app.service", &[pid]);

        assert_eq!(find_daemon(&cfg, &fake.cgroup(), &unit).unwrap(), pid);
        assert_eq!(fake.procs("app.service"), pid.to_string());
    }

    /// A `PIDFile=` naming a pid that is not running is an error, not a start:
    /// the unit would otherwise be marked active around a process nobody has.
    #[test]
    fn a_pid_file_naming_a_dead_pid_is_refused() {
        let fake = FakeCgroup::new("dead");
        let unit = UnitName::parse("app.service").unwrap();
        let mut cfg = config("app.service", RestartPolicy::No);
        let pid_file = fake.root.join("app.pid");
        // A pid that can never exist, rather than one that has just exited: the
        // kernel's pid space is bounded by `/proc/sys/kernel/pid_max`, which is
        // itself capped at 4194304 (`PID_MAX_LIMIT`), so this is ESRCH for the
        // `kill(pid, 0)` probe every time. Reaping a child and reusing its pid
        // looked deterministic and was not - another test in the same binary
        // spawns processes, and one of them took the freed number.
        let dead: libc::pid_t = i32::MAX - 1;
        std::fs::write(&pid_file, format!("{dead}\n")).expect("pid file");
        cfg.pid_file = Some(pid_file.display().to_string());

        let why = find_daemon(&cfg, &fake.cgroup(), &unit).expect_err("a dead pid is not a daemon");

        assert!(why.contains("not running"), "unexpected reason: {why}");
    }

    /// Without a `PIDFile=`, the cgroup is all there is - and it has to name
    /// exactly one process, or the daemon is not determined.
    #[test]
    fn without_a_pid_file_the_cgroup_must_name_exactly_one_process() {
        let fake = FakeCgroup::new("members");
        let unit = UnitName::parse("app.service").unwrap();
        let cfg = config("app.service", RestartPolicy::No);
        assert!(cfg.pid_file.is_none(), "this test is about the fallback");

        let me = std::process::id() as libc::pid_t;
        fake.set_procs("app.service", &[me]);
        assert_eq!(
            find_daemon(&cfg, &fake.cgroup(), &unit).expect("one member is the daemon"),
            me
        );

        fake.set_procs("app.service", &[me, 1]);
        let why = find_daemon(&cfg, &fake.cgroup(), &unit)
            .expect_err("two members are not a unique daemon");
        assert!(
            why.contains("not uniquely determined"),
            "unexpected reason: {why}"
        );
    }

    /// And a unit with neither a `PIDFile=` nor a cgroup of its own has nothing
    /// to look at, which is said rather than guessed at.
    #[test]
    fn without_a_pid_file_or_a_cgroup_the_daemon_is_undetermined() {
        let fake = FakeCgroup::new("no-cgroup");
        let unit = UnitName::parse("app.service").unwrap();
        let cfg = config("app.service", RestartPolicy::No);

        let why =
            find_daemon(&cfg, &fake.cgroup(), &unit).expect_err("there is nothing to look at");

        assert!(
            why.contains("no PIDFile= and no cgroup"),
            "unexpected reason: {why}"
        );
    }

    #[test]
    fn a_new_unit_is_inactive_and_untracked() {
        let _guard = crate::test_support::children_lock();
        let m = manager_with("a.service", RestartPolicy::No);
        let rt = m.get(&UnitName::parse("a.service").unwrap()).unwrap();
        assert_eq!(rt.state, UnitState::Inactive);
        assert!(rt.pid.is_none());
        assert!(m.summary()[0].contains("inactive"));
    }

    /// Spawning must actually produce a process the manager knows about.
    #[test]
    fn spawning_a_unit_tracks_its_pid_and_becomes_active() {
        let _guard = crate::test_support::children_lock();
        let mut m = manager_with("a.service", RestartPolicy::No);
        let unit = UnitName::parse("a.service").unwrap();
        m.spawn_unit_with_output(&unit, &[], None, None)
            .expect("spawn");
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
        let _guard = crate::test_support::children_lock();
        let mut cfg = config("t.target", RestartPolicy::No);
        cfg.kind = rinit_core::UnitType::Target;
        cfg.exec_start = rinit_core::ExecCommand::empty();
        let mut m = UnitManager::new();
        m.add(cfg);
        let unit = UnitName::parse("t.target").unwrap();
        m.spawn_unit_with_output(&unit, &[], None, None)
            .expect("start");
        let rt = m.get(&unit).unwrap();
        assert_eq!(rt.state, UnitState::Active);
        assert!(rt.pid.is_none(), "a target must not spawn anything");
    }

    /// The core of M2's acceptance: a killed service is restarted.
    #[test]
    fn a_killed_service_schedules_a_restart() {
        let _guard = crate::test_support::children_lock();
        let mut m = manager_with("a.service", RestartPolicy::Always);
        let unit = UnitName::parse("a.service").unwrap();
        m.spawn_unit_with_output(&unit, &[], None, None)
            .expect("spawn");
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
        let _guard = crate::test_support::children_lock();
        let mut m = manager_with("a.service", RestartPolicy::No);
        let unit = UnitName::parse("a.service").unwrap();
        m.spawn_unit_with_output(&unit, &[], None, None)
            .expect("spawn");
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
        let _guard = crate::test_support::children_lock();
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
        let _guard = crate::test_support::children_lock();
        let mut m = manager_with("a.service", RestartPolicy::Always);
        let unit = UnitName::parse("a.service").unwrap();
        m.spawn_unit_with_output(&unit, &[], None, None)
            .expect("spawn");
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
        let _guard = crate::test_support::children_lock();
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
        let _guard = crate::test_support::children_lock();
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
        m.spawn_unit_with_output(unit, &[], None, None)
            .expect("spawn");
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
        let _guard = crate::test_support::children_lock();
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
        m.spawn_unit_with_output(&unit, &[], None, None)
            .expect("spawn");
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
        let _guard = crate::test_support::children_lock();
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
        let _guard = crate::test_support::children_lock();
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
        let _guard = crate::test_support::children_lock();
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
        let _guard = crate::test_support::children_lock();
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
        let _guard = crate::test_support::children_lock();
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
        m.spawn_unit_with_output(&unit, &[], None, None)
            .expect("spawn");
        (m, unit)
    }

    /// Make the next probe due immediately instead of waiting an interval.
    fn make_probe_due(m: &mut UnitManager, unit: &UnitName) {
        m.units.get_mut(unit).unwrap().health_at = Some(Instant::now() - Duration::from_secs(1));
    }

    /// A probe that exits 0 marks the unit healthy and books the next probe.
    #[test]
    fn a_passing_probe_marks_the_unit_healthy() {
        let _guard = crate::test_support::children_lock();
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
        let _guard = crate::test_support::children_lock();
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
        let _guard = crate::test_support::children_lock();
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
        let _guard = crate::test_support::children_lock();
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
        let _guard = crate::test_support::children_lock();
        let mut m = manager_with("a.service", RestartPolicy::No);
        let unit = UnitName::parse("a.service").unwrap();
        m.spawn_unit_with_output(&unit, &[], None, None)
            .expect("spawn");

        assert!(m.due_health_probes().is_empty());
        assert!(m.get(&unit).unwrap().health_at.is_none());

        cleanup(&mut m, &unit);
    }

    #[test]
    fn different_units_get_different_jitter_seeds() {
        let _guard = crate::test_support::children_lock();
        let a = fnv1a(b"a.service");
        let b = fnv1a(b"b.service");
        assert_ne!(a, b, "two units must not share a jitter stream");
        assert_eq!(a, fnv1a(b"a.service"), "and it must be stable");
    }

    #[test]
    fn units_are_reported_in_name_order() {
        let _guard = crate::test_support::children_lock();
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
        let _guard = crate::test_support::children_lock();
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
        let _guard = crate::test_support::children_lock();
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
        let _guard = crate::test_support::children_lock();
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
        let _guard = crate::test_support::children_lock();
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
        let _guard = crate::test_support::children_lock();
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
        let _guard = crate::test_support::children_lock();
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
        let _guard = crate::test_support::children_lock();
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
        let _guard = crate::test_support::children_lock();
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
        let _guard = crate::test_support::children_lock();
        let mut m = UnitManager::new();
        let ghost = UnitName::parse("ghost.service").unwrap();
        assert_eq!(m.request_start(&ghost, RequestSource::UserStart), None);
        assert_eq!(m.request_stop(&ghost, RequestSource::UserStop), None);
        assert!(m.started_units().is_empty());
    }

    /// Plan §1.1: power failure compresses every stop timeout to a hard cap.
    #[test]
    fn compress_stop_timeouts_caps_every_unit() {
        let _guard = crate::test_support::children_lock();
        let mut m = UnitManager::new();
        m.add(config("slow.service", RestartPolicy::No));
        m.add(config("quick.service", RestartPolicy::No));
        m.units
            .get_mut(&UnitName::parse("slow.service").unwrap())
            .unwrap()
            .config
            .timeout_stop_sec = 30;
        m.units
            .get_mut(&UnitName::parse("quick.service").unwrap())
            .unwrap()
            .config
            .timeout_stop_sec = 1;

        m.compress_stop_timeouts(2);

        assert_eq!(
            m.get(&UnitName::parse("slow.service").unwrap())
                .unwrap()
                .config
                .timeout_stop_sec,
            2
        );
        assert_eq!(
            m.get(&UnitName::parse("quick.service").unwrap())
                .unwrap()
                .config
                .timeout_stop_sec,
            1,
            "a shorter timeout is not lengthened"
        );
    }

    // --- two-stage stop (plan §1.1) -------------------------------------

    /// Plan §1.3(8): shutdown is the startup order reversed, so a dependent is
    /// stopped before what it depends on.
    #[test]
    fn shutdown_order_reverses_the_startup_order() {
        let mut m = UnitManager::new();
        m.add(config("base.service", RestartPolicy::No));
        m.add(config("app.service", RestartPolicy::No));
        let base = UnitName::parse("base.service").unwrap();
        let app = UnitName::parse("app.service").unwrap();
        m.set_shutdown_order(vec![base.clone(), app.clone()]);
        m.request_start(&app, RequestSource::Dependency);
        m.request_start(&base, RequestSource::Dependency);

        assert_eq!(m.shutdown_order(), vec![app, base]);
    }

    /// A `Conflicts=` pair is torn down with the superseded unit first.
    #[test]
    fn shutdown_order_stops_the_superseded_side_of_a_conflict_first() {
        let mut m = UnitManager::new();
        m.add(config("alpha.service", RestartPolicy::No));
        let mut beta = config("beta.service", RestartPolicy::No);
        beta.deps.conflicts = vec![UnitName::parse("alpha.service").unwrap()];
        m.add(beta);

        let alpha = UnitName::parse("alpha.service").unwrap();
        let beta = UnitName::parse("beta.service").unwrap();
        // Startup order has alpha first, so the reverse would stop beta first.
        m.set_shutdown_order(vec![alpha.clone(), beta.clone()]);
        m.request_start(&alpha, RequestSource::Dependency);
        m.request_start(&beta, RequestSource::Dependency);

        assert_eq!(
            m.shutdown_order(),
            vec![alpha, beta],
            "beta supersedes alpha, so alpha stops first"
        );
    }

    /// Plan §1.1: starting a unit stops whatever it conflicts with.
    #[test]
    fn starting_a_unit_stops_its_conflicts() {
        let _guard = crate::test_support::children_lock();
        let mut m = UnitManager::new();
        m.add(config("alpha.service", RestartPolicy::No));
        let mut beta = config("beta.service", RestartPolicy::No);
        beta.deps.conflicts = vec![UnitName::parse("alpha.service").unwrap()];
        m.add(beta);

        let alpha = UnitName::parse("alpha.service").unwrap();
        let beta = UnitName::parse("beta.service").unwrap();
        spawn_pid(&mut m, &alpha);

        let stopped = m.stop_conflicts(&beta);
        assert_eq!(stopped, std::slice::from_ref(&alpha));
        assert_eq!(
            m.get(&alpha).unwrap().state,
            UnitState::Deactivating,
            "the conflicting unit must be stopping"
        );

        cleanup(&mut m, &alpha);
    }

    /// `ExecStop=`, when set, is the stop action: the service is asked to stop
    /// by the command rather than by a signal.
    #[test]
    fn exec_stop_is_used_as_the_stop_action() {
        let _guard = crate::test_support::children_lock();
        let mut m = manager_with("a.service", RestartPolicy::No);
        let unit = UnitName::parse("a.service").unwrap();
        m.units.get_mut(&unit).unwrap().config.exec_stop =
            rinit_core::ExecCommand::new(vec!["/bin/true".to_string()]);
        spawn_pid(&mut m, &unit);

        m.stop_unit(&unit);

        let rt = m.get(&unit).unwrap();
        assert_eq!(rt.state, UnitState::Deactivating);
        assert!(rt.exec_stop_pid.is_some(), "ExecStop must be running");
        assert!(rt.kill_at.is_some(), "the SIGKILL escalation still applies");

        cleanup(&mut m, &unit);
    }

    /// Plan §1.3(13): a unit that fails to start fails everything that
    /// hard-`Requires=` it, so the dependents are not started on a broken
    /// foundation.
    #[test]
    fn a_failed_dependency_fails_its_requires_dependents() {
        let _guard = crate::test_support::children_lock();
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
        let _guard = crate::test_support::children_lock();
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
        let _guard = crate::test_support::children_lock();
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

    // --- control-plane pieces (plan §3 M3) ------------------------------

    /// Plan §1.3(13): a start that arrives while the unit is stopping is
    /// queued, then applied once the stop completes.
    #[test]
    fn a_start_during_deactivation_is_queued_then_applied() {
        let _guard = crate::test_support::children_lock();
        let mut m = manager_with("a.service", RestartPolicy::No);
        let unit = UnitName::parse("a.service").unwrap();
        m.request_start(&unit, RequestSource::Dependency);
        let pid = spawn_pid(&mut m, &unit);

        // A stop that did not come from a request, so the `Started` target
        // stands while the unit tears down.
        m.stop_unit(&unit);
        assert_eq!(m.get(&unit).unwrap().state, UnitState::Deactivating);

        assert_eq!(
            m.request_start(&unit, RequestSource::UserStart),
            None,
            "the start waits for the stop"
        );
        assert!(m.due_queued_starts().is_empty(), "still stopping");

        // The stop completes.
        let _ = process::kill_process_group(pid, libc::SIGKILL);
        let outcome = m
            .on_child_exit(pid, ExitOutcome::Signalled(libc::SIGKILL))
            .expect("handled");
        assert!(matches!(outcome, Outcome::Exited { .. }));
        assert_eq!(m.get(&unit).unwrap().state, UnitState::Inactive);

        assert_eq!(
            m.due_queued_starts(),
            std::slice::from_ref(&unit),
            "the queued start is applied once the stop is done"
        );
        assert!(m.due_queued_starts().is_empty(), "and only once");

        // `on_child_exit` is the manager's bookkeeping, not a `waitpid`: the
        // real child is still a zombie until this reaps it. A test that left
        // one behind would be reaped by whatever test ran next.
        cleanup(&mut m, &unit);
    }

    /// A user stop must not make a later user start impossible.
    ///
    /// `UserStop` outranks `UserStart` (plan §1.3(11)), and the merged target
    /// used to survive the stop it asked for - so `rctl stop app.service`
    /// followed by `rctl start app.service` answered "already active" and left
    /// the unit inactive. Precedence is about requests that are still
    /// outstanding; one that has been carried out is spent, not outstanding.
    #[test]
    fn a_user_start_after_a_user_stop_starts_the_unit_again() {
        let _guard = crate::test_support::children_lock();
        let mut m = manager_with("a.service", RestartPolicy::No);
        let unit = UnitName::parse("a.service").unwrap();
        m.request_start(&unit, RequestSource::Dependency);
        let pid = spawn_pid(&mut m, &unit);

        m.request_stop(&unit, RequestSource::UserStop);
        m.stop_unit(&unit);
        let _ = process::kill_process_group(pid, libc::SIGKILL);
        let _ = m.on_child_exit(pid, ExitOutcome::Signalled(libc::SIGKILL));
        assert_eq!(m.get(&unit).unwrap().state, UnitState::Inactive);

        assert_eq!(
            m.request_start(&unit, RequestSource::UserStart),
            Some(Effect::SpawnProcess),
            "the stop that has been carried out must not veto the start"
        );
        assert_eq!(m.target_of(&unit), Some(JobTarget::Started));
        cleanup(&mut m, &unit);
    }

    /// The same, for a start that arrived while the requested stop was still in
    /// flight: it is queued, and it has to be *applied* once the stop finishes.
    #[test]
    fn a_start_queued_behind_a_requested_stop_is_applied() {
        let _guard = crate::test_support::children_lock();
        let mut m = manager_with("a.service", RestartPolicy::No);
        let unit = UnitName::parse("a.service").unwrap();
        m.request_start(&unit, RequestSource::Dependency);
        let pid = spawn_pid(&mut m, &unit);

        m.request_stop(&unit, RequestSource::UserStop);
        m.stop_unit(&unit);
        assert_eq!(
            m.request_start(&unit, RequestSource::UserStart),
            None,
            "the start waits for the stop"
        );
        assert!(m.due_queued_starts().is_empty(), "still stopping");

        let _ = process::kill_process_group(pid, libc::SIGKILL);
        let _ = m.on_child_exit(pid, ExitOutcome::Signalled(libc::SIGKILL));
        assert_eq!(m.get(&unit).unwrap().state, UnitState::Inactive);

        assert_eq!(
            m.due_queued_starts(),
            std::slice::from_ref(&unit),
            "the queued start is applied once the stop is done"
        );
        assert_eq!(
            m.target_of(&unit),
            Some(JobTarget::Started),
            "and it took the target back from the spent stop"
        );
        cleanup(&mut m, &unit);
    }

    /// An explicit stop cancels a queued start.
    #[test]
    fn a_stop_cancels_a_queued_start() {
        let _guard = crate::test_support::children_lock();
        let mut m = manager_with("a.service", RestartPolicy::No);
        let unit = UnitName::parse("a.service").unwrap();
        m.request_start(&unit, RequestSource::Dependency);
        spawn_pid(&mut m, &unit);
        m.stop_unit(&unit);
        m.request_start(&unit, RequestSource::UserStart);

        m.request_stop(&unit, RequestSource::UserStop);
        assert!(m.due_queued_starts().is_empty(), "the stop wins");

        cleanup(&mut m, &unit);
    }

    /// `restart` of a running unit stops it and marks the stop as a restart.
    #[test]
    fn restarting_a_running_unit_stops_it_for_a_restart() {
        let _guard = crate::test_support::children_lock();
        let mut m = manager_with("a.service", RestartPolicy::Always);
        let unit = UnitName::parse("a.service").unwrap();
        m.request_start(&unit, RequestSource::Dependency);
        spawn_pid(&mut m, &unit);

        assert_eq!(m.restart_unit(&unit), RestartPlan::Stopping);
        let rt = m.get(&unit).unwrap();
        assert_eq!(rt.state, UnitState::Deactivating);
        assert!(
            rt.restart_after_stop,
            "the exit must re-enter the restart path"
        );

        cleanup(&mut m, &unit);
    }

    /// `restart` of a unit that is already down is just a start.
    #[test]
    fn restarting_a_stopped_unit_is_a_start() {
        let mut m = manager_with("a.service", RestartPolicy::No);
        let unit = UnitName::parse("a.service").unwrap();
        assert_eq!(m.restart_unit(&unit), RestartPlan::Start);
    }

    /// `reset-failed` clears the failure without starting the unit.
    #[test]
    fn reset_failed_clears_the_state_but_does_not_start() {
        let mut m = manager_with("a.service", RestartPolicy::Always);
        let unit = UnitName::parse("a.service").unwrap();
        m.fail_start(&unit);
        m.units.get_mut(&unit).unwrap().backoff.on_failure(0);
        assert!(m.is_failed(&unit));

        m.reset_failed(&unit);

        let rt = m.get(&unit).unwrap();
        assert_eq!(rt.state, UnitState::Inactive);
        assert!(rt.pid.is_none(), "it must not be started");
        assert_eq!(rt.backoff.failures_in_window(), 0);
    }

    /// `target_of` reports what the queue is asking for, which is what
    /// `rctl status` shows.
    #[test]
    fn target_of_reports_the_merged_target() {
        let mut m = manager_with("a.service", RestartPolicy::No);
        let unit = UnitName::parse("a.service").unwrap();
        assert_eq!(m.target_of(&unit), None, "nothing asked yet");
        m.request_start(&unit, RequestSource::Dependency);
        assert_eq!(m.target_of(&unit), Some(JobTarget::Started));
        m.request_stop(&unit, RequestSource::UserStop);
        assert_eq!(m.target_of(&unit), Some(JobTarget::Stopped));
    }

    // --- §7.7: template instances ----------------------------------

    /// Asking for an instance is what creates it, and each one is a unit of its
    /// own: two instances of one template share nothing but the text.
    #[test]
    fn an_instance_is_created_from_its_template_and_is_its_own_unit() {
        let mut m = UnitManager::new();
        let mut templates = BTreeMap::new();
        templates.insert(
            "worker@.service".to_string(),
            "[Service]\nExecStart=/bin/echo worker-%i (%n, %p)%%\n".to_string(),
        );
        m.set_templates(templates);

        let first = UnitName::parse("worker@blue.service").unwrap();
        assert_eq!(m.instantiate(&first), Ok(()));
        let second = UnitName::parse("worker@green.service").unwrap();
        assert_eq!(m.instantiate(&second), Ok(()));

        let argv = |unit: &UnitName| {
            m.get(unit)
                .expect("registered")
                .config
                .exec_start
                .argv()
                .join(" ")
        };
        assert_eq!(
            argv(&first),
            "/bin/echo worker-blue (worker@blue.service, worker)%"
        );
        assert_eq!(
            argv(&second),
            "/bin/echo worker-green (worker@green.service, worker)%"
        );
        assert!(
            m.get(&first).unwrap().config.name == first
                && m.get(&second).unwrap().config.name == second,
            "each instance is its own unit, named after itself"
        );
    }

    /// The template itself is not a unit, and the three ways an instance can fail
    /// to be made each say which one it was.
    #[test]
    fn instantiation_reports_why_it_could_not_be_done() {
        let mut m = UnitManager::new();
        let mut templates = BTreeMap::new();
        templates.insert(
            "worker@.service".to_string(),
            "[Service]\nExecStart=/bin/echo %j\n".to_string(),
        );
        m.set_templates(templates);

        // A `%` specifier this build does not know.
        let bad = UnitName::parse("worker@1.service").unwrap();
        let why = m.instantiate(&bad).expect_err("must be refused");
        assert!(
            why.contains("%j") || why.contains("%-specifier"),
            "got: {why}"
        );

        // A name that is not an instance at all.
        let plain = UnitName::parse("plain.service").unwrap();
        let why = m.instantiate(&plain).expect_err("must be refused");
        assert!(why.contains("not a template instance"), "got: {why}");

        // An instance of a template that does not exist.
        let orphan = UnitName::parse("other@1.service").unwrap();
        let why = m.instantiate(&orphan).expect_err("must be refused");
        assert!(why.contains("no template other@.service"), "got: {why}");

        // And a template is startable only through an instance, which is what
        // `is_template` lets a caller say.
        assert!(m.is_template(&UnitName::parse("worker@.service").unwrap()));
        assert!(!m.is_template(&bad));
        assert!(
            m.summary()
                .iter()
                .any(|line| line == "worker@.service: template"),
            "`rctl list` has to show that it exists: {:?}",
            m.summary()
        );
    }

    /// A template that was never given to the manager is not a template, so a
    /// typo cannot pass for one.
    #[test]
    fn a_template_name_with_no_template_behind_it_is_not_one() {
        let m = UnitManager::new();
        assert!(!m.is_template(&UnitName::parse("worker@.service").unwrap()));
    }

    // --- §7.6: what a reload does to the manager -------------------

    /// Registering a definition answers one of four things, and the two that
    /// matter are the ones plan §7.6 separates: a *stopped* unit takes the new
    /// definition now, a *running* one keeps the old one until its next start.
    #[test]
    fn a_reload_applies_a_new_definition_now_or_defers_it_to_the_next_start() {
        let _guard = crate::test_support::children_lock();
        let mut m = UnitManager::new();
        let unit = UnitName::parse("app.service").unwrap();
        let original = config("app.service", RestartPolicy::No);

        assert_eq!(m.add(original.clone()), ConfigChange::Added);
        assert_eq!(m.get(&unit).unwrap().state, UnitState::Inactive);
        assert_eq!(m.add(original.clone()), ConfigChange::Unchanged);

        // Not running: in force immediately, and nothing is left pending.
        let mut first = original.clone();
        first.description = "first".to_string();
        assert_eq!(m.add(first), ConfigChange::Applied);
        assert_eq!(m.get(&unit).unwrap().config.description, "first");
        assert!(m.get(&unit).unwrap().pending_config.is_none());

        // Running: the process keeps the definition it was started with - which
        // is what makes the deferral real, `ExecStop=` included - and the new one
        // waits.
        m.spawn_unit_with_output(&unit, &[], None, None)
            .expect("spawn");
        let mut second = original.clone();
        second.description = "second".to_string();
        assert_eq!(m.add(second), ConfigChange::Deferred);
        assert_eq!(
            m.get(&unit).unwrap().config.description,
            "first",
            "a running unit must keep the definition it is running"
        );
        assert_eq!(
            m.get(&unit)
                .unwrap()
                .pending_config
                .as_ref()
                .map(|c| c.description.clone()),
            Some("second".to_string())
        );

        // A start that finds the unit still up does not take it either: only a
        // real start does.
        m.adopt_pending_config(&unit);
        assert_eq!(m.get(&unit).unwrap().config.description, "first");

        // Let it settle, then the next start adopts the pending definition.
        let pid = m.get(&unit).unwrap().pid.expect("spawned");
        let _ = process::kill_process_group(pid, libc::SIGKILL);
        let _ = m.on_child_exit(pid, ExitOutcome::Signalled(libc::SIGKILL));
        m.adopt_pending_config(&unit);
        assert_eq!(m.get(&unit).unwrap().config.description, "second");
        assert!(m.get(&unit).unwrap().pending_config.is_none());
    }

    /// Putting the file back the way the running unit has it withdraws a
    /// deferred definition: the next start must not adopt a change that no
    /// longer exists on disk.
    #[test]
    fn reverting_a_deferred_definition_withdraws_it() {
        let _guard = crate::test_support::children_lock();
        let mut m = UnitManager::new();
        let unit = UnitName::parse("app.service").unwrap();
        let running = config("app.service", RestartPolicy::No);
        m.add(running.clone());
        m.spawn_unit_with_output(&unit, &[], None, None)
            .expect("spawn");

        let mut changed = running.clone();
        changed.description = "withdrawn".to_string();
        assert_eq!(m.add(changed), ConfigChange::Deferred);
        assert!(m.get(&unit).unwrap().pending_config.is_some());

        // The operator puts the file back.
        assert_eq!(m.add(running), ConfigChange::Unchanged);
        assert!(
            m.get(&unit).unwrap().pending_config.is_none(),
            "a change that is no longer on disk must not be waiting to take over"
        );

        let pid = m.get(&unit).unwrap().pid.expect("spawned");
        let _ = process::kill_process_group(pid, libc::SIGKILL);
        let _ = m.on_child_exit(pid, ExitOutcome::Signalled(libc::SIGKILL));
        m.adopt_pending_config(&unit);
        assert!(m.get(&unit).unwrap().config.description.is_empty());
    }

    /// A removed unit is hidden from the protocol immediately, but the runtime
    /// stays while its stop is in flight - the `SIGKILL` deadline and the reaping
    /// both need it - and is dropped by the next reload once it is down.
    #[test]
    fn a_retired_unit_is_hidden_at_once_and_pruned_once_it_is_down() {
        let _guard = crate::test_support::children_lock();
        // Not running: hidden and pruned in one go.
        let mut m = manager_with("app.service", RestartPolicy::No);
        let unit = UnitName::parse("app.service").unwrap();
        assert!(m.exists(&unit));

        m.retire(&unit);
        assert!(
            !m.exists(&unit),
            "a removed unit is gone as far as rctl is concerned"
        );
        assert!(
            !m.summary().iter().any(|line| line.contains("app.service")),
            "and `rctl list` must agree: {:?}",
            m.summary()
        );
        assert!(m.get(&unit).is_some(), "the entry itself is still there");
        assert_eq!(m.prune_retired(), 1);
        assert!(
            m.get(&unit).is_none(),
            "and is dropped once nothing is in flight"
        );

        // Running: the entry has to survive the prune, because the stop needs it.
        let mut m = manager_with("app.service", RestartPolicy::No);
        m.spawn_unit_with_output(&unit, &[], None, None)
            .expect("spawn");
        m.retire(&unit);
        assert_eq!(
            m.prune_retired(),
            0,
            "an in-flight stop still needs the entry"
        );
        assert!(m.get(&unit).is_some());

        // A file that comes back makes it a unit again, state and all.
        assert_eq!(
            m.add(config("app.service", RestartPolicy::No)),
            ConfigChange::Unchanged
        );
        assert!(m.exists(&unit), "re-registering clears the tombstone");
        assert_eq!(m.get(&unit).unwrap().state, UnitState::Active);

        m.stop_unit(&unit);
        if let Some(pid) = m.get(&unit).unwrap().pid {
            let _ = process::kill_process_group(pid, libc::SIGKILL);
        }
    }
}
