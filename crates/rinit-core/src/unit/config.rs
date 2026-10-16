//! Unit-file data model (plan §1.1).
//!
//! This is the contract every other module codes against: the field types,
//! defaults and zero-value semantics here are the ones the parser must
//! produce and the manager must honour.
//!
//! Three rules from the contract drive most of the design:
//!
//! - **Zero is meaningful.** `RestartSec=0` means "restart immediately",
//!   `TimeoutStartSec=0` means "no timeout", `TimeoutStopSec=0` means "SIGKILL
//!   at once". Those therefore use plain integers, not `Option`.
//! - **Unset is not the same as empty.** `CapabilityBoundingSet` unset means
//!   "inherit"; `CapabilityBoundingSet=` means "clear". That needs
//!   `Option<Vec<_>>`, where `None` and `Some(vec![])` differ.
//! - **A target has no process.** `UnitType::Target` units carry no
//!   `ExecStart` and are not spawned.

use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use super::UnitName;
use super::UnitType;

/// How a service is considered started (plan §1.1 `[Service] Type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ServiceKind {
    /// Started once `ExecStart` is exec'd. Default.
    #[default]
    Simple,
    /// Runs to completion; "active" means it exited successfully.
    Oneshot,
    /// Started once it reports `READY=1` over `NOTIFY_SOCKET`.
    Notify,
    /// A daemon that detaches: started once its main process has exited *and*
    /// the daemon it left behind has been located (plan §7.10).
    Forking,
}

impl ServiceKind {
    /// Parse the directive value.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "simple" => Some(ServiceKind::Simple),
            "oneshot" => Some(ServiceKind::Oneshot),
            "notify" => Some(ServiceKind::Notify),
            "forking" => Some(ServiceKind::Forking),
            _ => None,
        }
    }
}

/// When a service is restarted after it exits (plan §1.1 `Restart=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RestartPolicy {
    /// Never restart. Default.
    #[default]
    No,
    /// Always restart, whatever the exit status.
    Always,
    /// Restart only on a clean exit.
    OnSuccess,
    /// Restart only on a non-zero exit or a signal.
    OnFailure,
    /// Restart only when killed by a signal or on a core dump.
    OnAbnormal,
}

impl RestartPolicy {
    /// Parse the directive value.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "no" => Some(RestartPolicy::No),
            "always" => Some(RestartPolicy::Always),
            "on-success" => Some(RestartPolicy::OnSuccess),
            "on-failure" => Some(RestartPolicy::OnFailure),
            "on-abnormal" => Some(RestartPolicy::OnAbnormal),
            _ => None,
        }
    }
}

/// A command split into `argv` (plan §1.3(1) command tokenisation).
///
/// No shell is involved: `|`, `>`, `$()` and friends are literal arguments,
/// and variables are never expanded (plan §1.3(1)).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecCommand {
    argv: Vec<String>,
}

impl ExecCommand {
    /// An unset command (as for `ExecStop=`, where rinit sends `SIGTERM`).
    pub const fn empty() -> Self {
        ExecCommand { argv: Vec::new() }
    }

    /// Build from an already-split `argv`.
    pub fn new(argv: Vec<String>) -> Self {
        debug_assert!(!argv.is_empty(), "an ExecCommand is never empty");
        ExecCommand { argv }
    }

    /// The `argv`, with `argv[0]` being the program.
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// Whether a command was configured at all.
    pub fn is_empty(&self) -> bool {
        self.argv.is_empty()
    }
}

/// A byte size with an optional `K`/`M`/`G` suffix (plan §1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeBytes(pub u64);

impl SizeBytes {
    /// Parse `32M`, `1G`, `512K` or a bare byte count.
    ///
    /// # Errors
    /// Returns `None` for an empty string, a bad suffix, an overflowing value,
    /// or a suffix with no digits.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        let (digits, scale) = s
            .strip_suffix(['K', 'k'])
            .map(|rest| (rest, 1024u64))
            .or_else(|| s.strip_suffix(['M', 'm']).map(|rest| (rest, 1024 * 1024)))
            .or_else(|| {
                s.strip_suffix(['G', 'g'])
                    .map(|rest| (rest, 1024 * 1024 * 1024))
            })
            .unwrap_or((s, 1));
        let digits = digits.trim();
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        // `SizeBytes` itself places no lower bound; the parser rejects a zero
        // limit for the cgroup directives (plan §1.1: only the timeout/restart
        // keys may be 0).
        digits
            .parse::<u64>()
            .ok()?
            .checked_mul(scale)
            .map(SizeBytes)
    }
}

/// CPU quota (plan §1.1 `CgroupCPUMax=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuQuota {
    /// A percentage of one CPU, e.g. `50%`.
    Percent(u32),
    /// An explicit cgroup quota/period pair, e.g. `200000 100000`.
    QuotaPeriod {
        /// Allowed runtime per `period`.
        quota: u64,
        /// Period length in microseconds.
        period: u64,
    },
}

impl CpuQuota {
    /// Parse `50%` or `quota period`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if let Some(pct) = s.strip_suffix('%') {
            let pct = pct.trim();
            if pct.is_empty() || !pct.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            return pct
                .parse::<u32>()
                .ok()
                .filter(|p| *p <= 100)
                .map(CpuQuota::Percent);
        }
        let mut parts = s.split_whitespace();
        let quota = parts.next()?.parse::<u64>().ok()?;
        let period = parts.next()?.parse::<u64>().ok()?;
        if parts.next().is_some() || period == 0 {
            return None;
        }
        Some(CpuQuota::QuotaPeriod { quota, period })
    }
}

/// A Linux capability name (plan §1.1, used by M5's sandboxing work).
///
/// Kept as a validated name rather than a number: rinit resolves names to
/// capability bit numbers only when it applies them, so the model stays free
/// of an arch-specific table.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cap(String);

impl Cap {
    /// Validate and normalise a capability name.
    ///
    /// Accepts the spelling people actually write - `NET_BIND_SERVICE`,
    /// `CAP_SYS_ADMIN`, or the lower-case form - and stores one canonical
    /// lower-case name with the `CAP_` prefix stripped, so the runtime's name
    /// table has a single key per capability (plan §7.4).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        // `CAP_FOO` and `cap_foo` and `FOO` all mean the same capability.
        let name = s
            .strip_prefix("CAP_")
            .or_else(|| s.strip_prefix("cap_"))
            .unwrap_or(s);
        if name.is_empty() {
            return None;
        }
        let normalised = name.to_ascii_lowercase();
        if !normalised
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return None;
        }
        Some(Cap(normalised))
    }

    /// The capability name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Dependency declarations of one unit (plan §1.1 `[Unit]`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Dependencies {
    /// Hard dependencies: these must activate, else this unit fails.
    pub requires: Vec<UnitName>,
    /// Soft dependencies: started if present, failure is tolerated.
    pub wants: Vec<UnitName>,
    /// Ordering only: start after these.
    pub after: Vec<UnitName>,
    /// Ordering only: start before these.
    pub before: Vec<UnitName>,
    /// Mutual exclusion.
    pub conflicts: Vec<UnitName>,
}

impl Dependencies {
    /// Every unit referenced by any directive, deduplicated.
    pub fn all_referenced(&self) -> Vec<UnitName> {
        let mut out: Vec<UnitName> = Vec::new();
        for group in [
            &self.requires,
            &self.wants,
            &self.after,
            &self.before,
            &self.conflicts,
        ] {
            for unit in group {
                if !out.contains(unit) {
                    out.push(unit.clone());
                }
            }
        }
        out
    }
}

/// A fully parsed unit file.
///
/// Constructed only by [`super::parser`]; every field already has its default
/// applied, so the manager never has to guess what "unset" means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitConfig {
    /// The unit's name, from the file name.
    pub name: UnitName,
    /// Whether this unit spawns a process (`Target` units do not).
    pub kind: UnitType,

    // --- [Unit] ------------------------------------------------------
    /// Human-readable description.
    pub description: String,
    /// Documentation URL.
    pub documentation: String,
    /// Dependency declarations.
    pub deps: Dependencies,

    // --- [Service] ----------------------------------------------------
    /// How startup completion is judged.
    pub service_kind: ServiceKind,
    /// The main command. Empty only for targets.
    pub exec_start: ExecCommand,
    /// Commands run before `ExecStart`.
    pub exec_start_pre: Vec<ExecCommand>,
    /// Commands run after `ExecStart`.
    pub exec_start_post: Vec<ExecCommand>,
    /// Stop command; empty means `SIGTERM`.
    pub exec_stop: ExecCommand,
    /// A controlling terminal for the unit (plan §7.11).
    ///
    /// `None` means the service inherits rinit's own descriptors and no
    /// controlling terminal - what every unit did until this existed, and what
    /// most want. `Some(path)` makes the child open that device after `setsid`,
    /// take it as its controlling terminal and use it as stdin/stdout/stderr,
    /// which is what a `getty`-like service needs before it can run a login
    /// shell.
    pub tty_path: Option<String>,
    /// Whether to reset the terminal when taking it over (plan §7.11).
    ///
    /// A virtual hangup: the queues are flushed and whoever was using the
    /// terminal is told it is gone, which is what makes a respawned console
    /// service start from a clean terminal rather than on top of the last
    /// session's leftovers.
    pub tty_reset: bool,
    /// Where a `Type=forking` daemon writes its pid (plan §7.10).
    ///
    /// `None` means rinit has to find the daemon itself, which it can only do
    /// through the unit's own cgroup - so a `forking` unit without this needs a
    /// `Cgroup*=`, and without either it fails closed rather than guess.
    ///
    /// A `String` rather than a `PathBuf`, like [`working_directory`]: this crate
    /// is `no_std + alloc`, and the path is resolved against the filesystem by
    /// the runtime that has one.
    ///
    /// [`working_directory`]: Self::working_directory
    pub pid_file: Option<String>,
    /// Restart policy.
    pub restart: RestartPolicy,
    /// Backoff base for restarts; `0` means restart immediately.
    pub restart_sec: u64,
    /// Failures allowed within the interval before the circuit breaker opens.
    pub start_limit_burst: u64,
    /// Failure counting window; `0` disables the breaker.
    pub start_limit_interval_sec: u64,
    /// Start timeout; `0` means wait forever.
    pub timeout_start_sec: u64,
    /// Stop timeout; `0` means `SIGKILL` immediately.
    pub timeout_stop_sec: u64,
    /// User to drop to; empty means root.
    pub user: String,
    /// Group to drop to; empty means root's.
    pub group: String,
    /// Whether to deny privilege escalation to children.
    pub no_new_privileges: bool,
    /// `None` = inherit; `Some(vec![])` = cleared; `Some(..)` = explicit set.
    pub capability_bounding_set: Option<Vec<Cap>>,
    /// `None` = grant none; `Some(vec![])` = explicit empty; `Some(..)` = set.
    pub ambient_capabilities: Option<Vec<Cap>>,
    /// Environment overrides as `KEY=VALUE`.
    pub environment: Vec<(String, String)>,
    /// File to read further environment from.
    pub environment_file: String,
    /// Working directory; `/` unless overridden (plan §1.1).
    pub working_directory: String,
    /// Health probe command; empty means no health checking.
    pub health_check: ExecCommand,
    /// Health probe period.
    pub health_interval_sec: u64,
    /// Health probe timeout.
    pub health_timeout_sec: u64,
    /// Whether a failed probe triggers a restart.
    pub health_restart_on_failure: bool,
    /// Memory hard limit; `None` = unlimited.
    pub cgroup_memory_max: Option<SizeBytes>,
    /// Memory soft limit; `None` = unlimited.
    pub cgroup_memory_high: Option<SizeBytes>,
    /// CPU limit; `None` = unlimited.
    pub cgroup_cpu_max: Option<CpuQuota>,
    /// Task limit; `None` = unlimited.
    pub cgroup_tasks_max: Option<u64>,
}

impl UnitConfig {
    /// A configuration with every default applied and no dependencies.
    ///
    /// The parser fills this in; it exists so tests and M1 tooling can build a
    /// minimal unit without going through the text format.
    pub fn new(name: UnitName) -> Self {
        UnitConfig {
            kind: name.unit_type(),
            name,
            description: String::new(),
            documentation: String::new(),
            deps: Dependencies::default(),
            service_kind: ServiceKind::Simple,
            exec_start: ExecCommand::empty(),
            exec_start_pre: Vec::new(),
            exec_start_post: Vec::new(),
            exec_stop: ExecCommand::empty(),
            restart: RestartPolicy::No,
            restart_sec: 2,
            start_limit_burst: 5,
            start_limit_interval_sec: 10,
            timeout_start_sec: 30,
            timeout_stop_sec: 10,
            user: String::new(),
            group: String::new(),
            no_new_privileges: false,
            capability_bounding_set: None,
            ambient_capabilities: None,
            environment: Vec::new(),
            environment_file: String::new(),
            working_directory: String::from("/"),
            tty_path: None,
            tty_reset: true,
            pid_file: None,
            health_check: ExecCommand::empty(),
            health_interval_sec: 30,
            health_timeout_sec: 10,
            health_restart_on_failure: false,
            cgroup_memory_max: None,
            cgroup_memory_high: None,
            cgroup_cpu_max: None,
            cgroup_tasks_max: None,
        }
    }

    /// Whether this unit spawns a process.
    pub fn is_target(&self) -> bool {
        self.kind == UnitType::Target
    }

    /// The `RestartSec=` backoff as a duration.
    pub fn restart_delay(&self) -> Duration {
        Duration::from_secs(self.restart_sec)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn name(s: &str) -> UnitName {
        UnitName::parse(s).unwrap()
    }

    #[test]
    fn defaults_match_the_contract_table() {
        let cfg = UnitConfig::new(name("app.service"));
        assert_eq!(cfg.service_kind, ServiceKind::Simple);
        assert_eq!(cfg.restart, RestartPolicy::No);
        assert_eq!(cfg.restart_sec, 2);
        assert_eq!(cfg.start_limit_burst, 5);
        assert_eq!(cfg.start_limit_interval_sec, 10);
        assert_eq!(cfg.timeout_start_sec, 30);
        assert_eq!(cfg.timeout_stop_sec, 10);
        assert_eq!(cfg.health_interval_sec, 30);
        assert_eq!(cfg.health_timeout_sec, 10);
        assert_eq!(
            cfg.working_directory, "/",
            "plan §1.1: the default working directory is /"
        );
        assert!(!cfg.no_new_privileges);
        assert!(!cfg.health_restart_on_failure);
        assert_eq!(cfg.capability_bounding_set, None);
        assert_eq!(cfg.ambient_capabilities, None);
        assert_eq!(cfg.cgroup_memory_max, None);
        assert_eq!(cfg.cgroup_tasks_max, None);
        assert!(cfg.deps.all_referenced().is_empty());
    }

    /// The unit kind must follow the file's suffix when nothing overrides it.
    #[test]
    fn kind_defaults_to_the_name_suffix() {
        assert!(!UnitConfig::new(name("x.service")).is_target());
        assert!(UnitConfig::new(name("x.target")).is_target());
    }

    #[test]
    fn service_kind_parses_the_four_supported_values() {
        assert_eq!(ServiceKind::parse("simple"), Some(ServiceKind::Simple));
        assert_eq!(ServiceKind::parse("oneshot"), Some(ServiceKind::Oneshot));
        assert_eq!(ServiceKind::parse("notify"), Some(ServiceKind::Notify));
        assert_eq!(ServiceKind::parse("forking"), Some(ServiceKind::Forking));
        assert_eq!(ServiceKind::parse(""), None);
    }

    #[test]
    fn restart_policy_parses_all_five_values() {
        for (text, want) in [
            ("no", RestartPolicy::No),
            ("always", RestartPolicy::Always),
            ("on-success", RestartPolicy::OnSuccess),
            ("on-failure", RestartPolicy::OnFailure),
            ("on-abnormal", RestartPolicy::OnAbnormal),
        ] {
            assert_eq!(RestartPolicy::parse(text), Some(want), "{text}");
        }
        assert_eq!(RestartPolicy::parse("sometimes"), None);
    }

    #[test]
    fn size_parsing_accepts_suffixes_and_bare_bytes() {
        assert_eq!(SizeBytes::parse("512"), Some(SizeBytes(512)));
        assert_eq!(SizeBytes::parse("1K"), Some(SizeBytes(1024)));
        assert_eq!(SizeBytes::parse("32M"), Some(SizeBytes(32 * 1024 * 1024)));
        assert_eq!(SizeBytes::parse("1G"), Some(SizeBytes(1024 * 1024 * 1024)));
        assert_eq!(SizeBytes::parse(" 8m "), Some(SizeBytes(8 * 1024 * 1024)));
    }

    /// A bad suffix must not silently become a byte count.
    #[test]
    fn size_parsing_rejects_nonsense() {
        for bad in ["", "M", "32X", "-1", "3.5M", "abc", "1 2"] {
            assert_eq!(SizeBytes::parse(bad), None, "{bad:?} must be rejected");
        }
    }

    /// Overflow must be rejected rather than wrapping.
    #[test]
    fn size_parsing_rejects_overflow() {
        assert_eq!(SizeBytes::parse("18446744073709551616"), None);
        assert_eq!(SizeBytes::parse("99999999999999999999G"), None);
    }

    #[test]
    fn cpu_quota_parses_percent_and_pair_forms() {
        assert_eq!(CpuQuota::parse("50%"), Some(CpuQuota::Percent(50)));
        assert_eq!(
            CpuQuota::parse("200000 100000"),
            Some(CpuQuota::QuotaPeriod {
                quota: 200_000,
                period: 100_000
            })
        );
    }

    #[test]
    fn cpu_quota_rejects_out_of_range_and_malformed() {
        for bad in [
            "",
            "%",
            "101%",
            "abc%",
            "200000",
            "200000 0",
            "200000 100000 x",
        ] {
            assert_eq!(CpuQuota::parse(bad), None, "{bad:?} must be rejected");
        }
    }

    #[test]
    fn capability_names_are_validated_and_normalised() {
        // Plan §7.4 writes them upper-case (its `AmbientCapabilities=` example
        // is `NET_BIND_SERVICE`), so all spellings must land on one canonical
        // name for the runtime's lookup table.
        for spelling in [
            "net_bind_service",
            "NET_BIND_SERVICE",
            "CAP_NET_BIND_SERVICE",
        ] {
            assert_eq!(
                Cap::parse(spelling).unwrap().as_str(),
                "net_bind_service",
                "{spelling} must normalise"
            );
        }
        assert_eq!(Cap::parse("cap_sys_admin").unwrap().as_str(), "sys_admin");
        assert_eq!(Cap::parse(""), None);
        assert_eq!(Cap::parse("CAP_"), None);
        assert_eq!(Cap::parse("bad-name"), None);
    }

    #[test]
    fn referenced_dependencies_are_deduplicated_across_directives() {
        let mut deps = Dependencies::default();
        let a = name("a.service");
        deps.requires.push(a.clone());
        deps.after.push(a.clone());
        deps.wants.push(name("b.service"));
        assert_eq!(deps.all_referenced().len(), 2);
    }

    #[test]
    fn exec_command_exposes_argv() {
        let cmd = ExecCommand::new(vec![String::from("/bin/echo"), String::from("hi")]);
        assert_eq!(cmd.argv(), ["/bin/echo", "hi"]);
        assert!(!cmd.is_empty());
        assert!(ExecCommand::empty().is_empty());
    }
}
