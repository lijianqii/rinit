//! cgroup v2 limits for a unit (plan §7.3).
//!
//! This module owns the *pure* half of §7.3: which files a unit's directives
//! turn into, what goes in them, where the unit's cgroup lives, and which
//! controllers have to be enabled for those files to exist at all. The
//! filesystem side (creating directories, writing, `rmdir`, the degrade rule)
//! is `rinit-runtime` and the `rinit` binary's `cgroup` module; keeping the
//! mapping here is what makes it testable without root and without a cgroup
//! hierarchy mounted.
//!
//! Three details of cgroup v2 that this module encodes, because they are not
//! obvious from the directives alone:
//!
//! - **A controller must be enabled before its interface files exist.** In v2 a
//!   cgroup only has `cpu.max`/`memory.max`/`pids.max` if the controller is
//!   enabled in its *parent's* `cgroup.subtree_control`. Plan §7.3 does not
//!   mention this; without it every limit write is `ENOENT`. So rinit enables
//!   the controllers it needs on the hierarchy root and on the `rinit` group
//!   ([`controllers_to_enable`]).
//! - **Only controllers the running kernel actually provides can be enabled.**
//!   Writing `+pids` where `CONFIG_CGROUP_PIDS` is off fails, and it would take
//!   the whole boot down with it. [`controllers_to_enable`] therefore intersects
//!   the wanted set with `cgroup.controllers` instead of asserting it.
//! - **`memory.high` is a throttle, `memory.max` is a wall** (plan §7.3: the
//!   soft limit reclaims cache first). They are separate files, so a unit may
//!   set either or both, and an unset one is left alone - the kernel's own
//!   default there is `max`, i.e. unlimited.
//!
//! A percentage is turned into a quota/period pair here rather than during
//! parsing (plan §7.3 says "conversion at parse time"): `CpuQuota::Percent`
//! keeps the authored form so status output can say `50%`, while the
//! conversion itself stays pure and unit-tested. The bytes written are the same
//! either way.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::UnitName;
use super::config::{CpuQuota, SizeBytes, UnitConfig};

/// The group every unit's cgroup is created under (plan §7.3's `rinit/<unit>`).
pub const GROUP: &str = "rinit";

/// The period a percentage quota is expressed against, in microseconds.
///
/// 100000 µs is cgroup v2's own default period, which is what makes `50%` mean
/// "half of one CPU" rather than "half of some rinit-chosen window".
pub const DEFAULT_PERIOD_US: u64 = 100_000;

/// The controllers rinit enables on the hierarchy, in the order it writes them.
///
/// `cpu` backs `CgroupCPUMax=`, `memory` backs `CgroupMemoryMax=`/
/// `CgroupMemoryHigh=`, and `pids` backs `CgroupTasksMax=` (plan §7.3).
pub const WANTED_CONTROLLERS: &[&str] = &["cpu", "memory", "pids"];

/// The limits a unit asks for, and the files they become (plan §7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CgroupLimits {
    /// `CgroupCPUMax=`: `cpu.max`.
    pub cpu_max: Option<CpuQuota>,
    /// `CgroupMemoryMax=`: `memory.max`.
    pub memory_max: Option<SizeBytes>,
    /// `CgroupMemoryHigh=`: `memory.high`.
    pub memory_high: Option<SizeBytes>,
    /// `CgroupTasksMax=`: `pids.max`.
    pub tasks_max: Option<u64>,
}

impl CgroupLimits {
    /// The limits a unit's configuration asks for.
    pub fn from_config(config: &UnitConfig) -> Self {
        CgroupLimits {
            cpu_max: config.cgroup_cpu_max,
            memory_max: config.cgroup_memory_max,
            memory_high: config.cgroup_memory_high,
            tasks_max: config.cgroup_tasks_max,
        }
    }

    /// Whether the unit asked for nothing, in which case it is not given a
    /// cgroup of its own at all.
    ///
    /// A unit without limits stays in whatever cgroup rinit itself is in. That
    /// is deliberate: an empty cgroup per service would cost a directory, a
    /// controller-enabled subtree and a `rmdir` per unit to express "no
    /// opinion", and plan §7.3 asks for limits, not for bookkeeping.
    pub fn is_empty(&self) -> bool {
        self.cpu_max.is_none()
            && self.memory_max.is_none()
            && self.memory_high.is_none()
            && self.tasks_max.is_none()
    }

    /// The `(controller, file, contents)` triples to write, in write order.
    ///
    /// The controller is carried alongside each file so the caller can tell "the
    /// kernel does not have this controller" (skip the limit, warn) from "the
    /// write to an existing file failed" (the unit's request is bad, and it must
    /// not start pretending the limit is in force).
    pub fn files(&self) -> Vec<(&'static str, &'static str, String)> {
        let mut files = Vec::new();
        if let Some(quota) = self.cpu_max {
            files.push(("cpu", "cpu.max", quota.cpu_max_value()));
        }
        if let Some(max) = self.memory_max {
            files.push(("memory", "memory.max", max.0.to_string()));
        }
        // After `memory.max`, so a value the kernel refuses cannot leave the
        // hard limit unset while the soft one is in force.
        if let Some(high) = self.memory_high {
            files.push(("memory", "memory.high", high.0.to_string()));
        }
        if let Some(tasks) = self.tasks_max {
            files.push(("pids", "pids.max", tasks.to_string()));
        }
        files
    }

    /// The unit's cgroup, relative to the mounted hierarchy root.
    ///
    /// `UnitName` only admits `[A-Za-z0-9:_.@-]`, so this cannot contain a `/`
    /// and cannot escape the group directory.
    pub fn path_for(unit: &UnitName) -> String {
        format!("{GROUP}/{}", unit.canonical())
    }
}

impl CpuQuota {
    /// The contents of `cpu.max` for this quota.
    ///
    /// `50%` becomes `50000 100000`: quota and period in microseconds, which is
    /// the file's own format (plan §7.3). An explicit pair is written as it was
    /// authored.
    pub fn cpu_max_value(self) -> String {
        match self {
            CpuQuota::Percent(percent) => format!(
                "{} {DEFAULT_PERIOD_US}",
                u64::from(percent) * (DEFAULT_PERIOD_US / 100)
            ),
            CpuQuota::QuotaPeriod { quota, period } => format!("{quota} {period}"),
        }
    }
}

/// Whether `available` (the contents of `cgroup.controllers`) lists `name`.
///
/// The file is a single space-separated line of controller names, e.g.
/// `cpuset cpu io memory pids`.
pub fn has_controller(available: &str, name: &str) -> bool {
    available.split_whitespace().any(|c| c == name)
}

/// The `cgroup.subtree_control` contents that enable what rinit needs.
///
/// Returns `None` when the hierarchy offers none of [`WANTED_CONTROLLERS`] -
/// a kernel built without any of them, or a cgroup the caller is not allowed to
/// configure - in which case enabling is skipped rather than attempted, because
/// writing `+cpu` to a hierarchy that does not have `cpu` fails the write.
pub fn controllers_to_enable(available: &str) -> Option<String> {
    let names: Vec<&str> = WANTED_CONTROLLERS
        .iter()
        .copied()
        .filter(|name| has_controller(available, name))
        .collect();
    if names.is_empty() {
        return None;
    }
    let mut line = String::new();
    for name in names {
        if !line.is_empty() {
            line.push(' ');
        }
        line.push('+');
        line.push_str(name);
    }
    Some(line)
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn unit(name: &str) -> UnitName {
        UnitName::parse(name).expect("valid unit name")
    }

    fn sizes(memory_max: &str, memory_high: &str) -> (SizeBytes, SizeBytes) {
        (
            SizeBytes::parse(memory_max).expect("max"),
            SizeBytes::parse(memory_high).expect("high"),
        )
    }

    /// A unit that asked for nothing must not get a cgroup: no directory, no
    /// controller subtree, nothing to clean up.
    #[test]
    fn no_directives_means_no_limits_and_no_cgroup() {
        let limits = CgroupLimits::default();
        assert!(limits.is_empty());
        assert!(limits.files().is_empty());
    }

    /// Each directive must land in the file plan §7.3 names for it, with the
    /// value expressed the way the kernel reads it.
    #[test]
    fn every_directive_becomes_its_cgroup_file() {
        let (memory_max, memory_high) = sizes("32M", "24M");
        let limits = CgroupLimits {
            cpu_max: Some(CpuQuota::Percent(50)),
            memory_max: Some(memory_max),
            memory_high: Some(memory_high),
            tasks_max: Some(64),
        };
        assert!(!limits.is_empty());

        let files = limits.files();
        assert_eq!(
            files,
            vec![
                ("cpu", "cpu.max", "50000 100000".to_string()),
                ("memory", "memory.max", (32u64 * 1024 * 1024).to_string()),
                ("memory", "memory.high", (24u64 * 1024 * 1024).to_string()),
                ("pids", "pids.max", "64".to_string()),
            ]
        );
    }

    /// The percentage conversion is plan §7.3's example, verbatim.
    #[test]
    fn a_percentage_becomes_a_quota_against_the_default_period() {
        assert_eq!(
            CpuQuota::Percent(50).cpu_max_value(),
            "50000 100000",
            "50% of a 100000us period"
        );
        assert_eq!(CpuQuota::Percent(100).cpu_max_value(), "100000 100000");
        assert_eq!(CpuQuota::Percent(1).cpu_max_value(), "1000 100000");
    }

    /// An explicit pair is not re-scaled: the unit author already said which
    /// period they meant.
    #[test]
    fn an_explicit_quota_period_is_written_as_authored() {
        assert_eq!(
            CpuQuota::QuotaPeriod {
                quota: 200_000,
                period: 100_000
            }
            .cpu_max_value(),
            "200000 100000"
        );
        assert_eq!(
            CpuQuota::QuotaPeriod {
                quota: 5_000,
                period: 50_000
            }
            .cpu_max_value(),
            "5000 50000"
        );
    }

    /// One limit alone is still a cgroup: the other files are simply not
    /// touched, which leaves the kernel's unlimited default in place.
    #[test]
    fn a_single_limit_writes_only_its_own_file() {
        let (memory_max, _) = sizes("8M", "8M");
        let limits = CgroupLimits {
            memory_max: Some(memory_max),
            ..CgroupLimits::default()
        };
        assert!(!limits.is_empty());
        assert_eq!(
            limits.files(),
            vec![("memory", "memory.max", (8u64 * 1024 * 1024).to_string())]
        );
    }

    /// The path is `rinit/<canonical name>` (plan §7.3), and a bare name is
    /// canonicalised first so the directory matches the unit the manager knows.
    #[test]
    fn the_path_is_the_group_and_the_canonical_unit_name() {
        assert_eq!(
            CgroupLimits::path_for(&unit("worker")),
            "rinit/worker.service"
        );
        assert_eq!(
            CgroupLimits::path_for(&unit("multi-user.target")),
            "rinit/multi-user.target"
        );
    }

    /// Controllers are enabled within what the hierarchy has, in a fixed order,
    /// so the same kernel state always produces the same write.
    #[test]
    fn controllers_are_enabled_within_what_the_hierarchy_offers() {
        assert_eq!(
            controllers_to_enable("cpuset cpu io memory pids hugetlb").as_deref(),
            Some("+cpu +memory +pids")
        );
        assert_eq!(
            controllers_to_enable("memory pids").as_deref(),
            Some("+memory +pids"),
            "a kernel without cpu still gets the controllers it has"
        );
        assert_eq!(controllers_to_enable("cpu").as_deref(), Some("+cpu"));
        // Nothing rinit wants: skip the write rather than fail it.
        assert_eq!(controllers_to_enable("hugetlb io"), None);
        assert_eq!(controllers_to_enable(""), None);
    }

    /// The availability test is a plain membership test on the kernel's own
    /// format, and must not match a controller by prefix (`cpu` vs `cpuset`).
    #[test]
    fn controller_lookup_is_exact() {
        assert!(has_controller("cpuset cpu memory", "cpu"));
        assert!(!has_controller("cpuset memory", "cpu"));
        assert!(!has_controller("", "cpu"));
    }

    /// The order of writes is fixed, and `memory.high` follows `memory.max` so a
    /// refused hard limit cannot leave the soft one enabled on its own.
    #[test]
    fn the_write_order_is_fixed() {
        let (memory_max, memory_high) = sizes("32M", "24M");
        let limits = CgroupLimits {
            cpu_max: Some(CpuQuota::Percent(50)),
            memory_max: Some(memory_max),
            memory_high: Some(memory_high),
            tasks_max: Some(8),
        };
        let names: Vec<&str> = limits.files().iter().map(|(_, file, _)| *file).collect();
        assert_eq!(
            names,
            vec!["cpu.max", "memory.max", "memory.high", "pids.max"]
        );
    }
}
