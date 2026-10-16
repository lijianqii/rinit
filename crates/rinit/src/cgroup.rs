//! Cgroup v2 policy for the unit manager (plan §7.3).
//!
//! [`rinit_core::unit::cgroup`] says what a unit's directives turn into, and
//! [`rinit_runtime::syscall::cgroup`] does the file operations. This module is
//! the decision layer between them, and it exists because the plan needs three
//! different answers to three different failures:
//!
//! - **No usable hierarchy** - plan §7.3's only stated degrade case ("`cgroup.controllers`
//!   不存在时降级"): log a warning once, and every unit runs unlimited. A system
//!   that cannot limit anyone must still boot.
//! - **The unit's own cgroup cannot be arranged, or this kernel cannot express
//!   a limit** - a read-only or otherwise unwritable hierarchy, a controller the
//!   running kernel does not provide, or an attribute it does not provide
//!   (`cpu.max` needs `CONFIG_CFS_BANDWIDTH`; the `cpu` controller is there
//!   either way). All three are the same class of problem as "not mounted": the
//!   mechanism is simply not available for that unit. Warn, skip what cannot be
//!   applied, and let the unit run. A kernel built without `CONFIG_CGROUP_PIDS`
//!   must not stop a `CgroupTasksMax=` unit from starting, and neither must one
//!   built without CPU bandwidth provisioning.
//! - **A limit that exists was refused** - the hierarchy is there and the
//!   attribute is there, and the kernel said no to the value (an out-of-range
//!   `cpu.max` period, say). That fails the start, naming the attribute.
//!   Starting the service anyway would run it unbounded while its unit file
//!   claims otherwise, which is precisely the outcome §7.3 exists to prevent.
//!   The line between the two is the errno: `ENOENT` on opening an attribute is
//!   the kernel not having it, anything else is the request being refused.
//!
//! Placement is the child's job, not the parent's: [`Cgroup::setup`] creates the
//! cgroup and writes the limits *before* the fork, and returns the
//! `cgroup.procs` path for the child to write its own pid into
//! ([`cgroup::join_from_child`]). Plan §7.3 asks for "fork first, then the parent
//! writes the pid, then the child drops privileges", and the deviation here is
//! only the writer (see the README's deviation list). The reason is timing:
//! `spawn_sandboxed` returns only once the child has `exec`'d, so a parent-side
//! write lands *after* the service is running and could already have forked,
//! while the child writing its own pid is done before anything else the service
//! can do, and while it still owns the file.

use std::path::{Path, PathBuf};

use rinit_core::unit::UnitName;
use rinit_core::unit::cgroup::{CgroupLimits, GROUP, controllers_to_enable, has_controller};
use rinit_runtime::RuntimeError;
use rinit_runtime::syscall::cgroup::{self, CGROUP_ROOT};
use rinit_runtime::syscall::process;

use crate::log::rlog;

/// The cgroup side of the unit manager: a hierarchy root, or nothing at all.
///
/// `Default` is disabled, which is what every unit test that does not care
/// about cgroups gets, and what a boot without a hierarchy ends up with.
#[derive(Debug, Clone, Default)]
pub struct Cgroup {
    /// `None` when limits are not applied at all: dev mode, a boot with no v2
    /// hierarchy, or a caller that never enabled it.
    root: Option<PathBuf>,
}

impl Cgroup {
    /// A cgroup side that does nothing.
    pub const fn disabled() -> Self {
        Cgroup { root: None }
    }

    /// Use `root` as the mounted hierarchy.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Cgroup {
            root: Some(root.into()),
        }
    }

    /// Whether limits are applied at all.
    pub const fn is_enabled(&self) -> bool {
        self.root.is_some()
    }

    /// The unit's own cgroup directory.
    fn dir(&self, unit: &UnitName) -> Option<PathBuf> {
        self.root
            .as_ref()
            .map(|root| root.join(CgroupLimits::path_for(unit)))
    }

    /// Create the unit's cgroup, write its limits, and name the file the child
    /// must join (plan §7.3).
    ///
    /// Returns the `cgroup.procs` the spawned child should write itself into, or
    /// `None` when there is nothing to join - no hierarchy, no directives, or
    /// nothing this kernel can enforce for that unit.
    ///
    /// # Errors
    /// A write to an attribute the kernel provides that the kernel then refused.
    /// See the module docs for why that is the one failure that must stop a
    /// start, and why an attribute the kernel does not provide is not one.
    pub fn setup(
        &self,
        unit: &UnitName,
        limits: &CgroupLimits,
    ) -> Result<Option<PathBuf>, RuntimeError> {
        let Some(dir) = self.dir(unit) else {
            return Ok(None);
        };
        if limits.is_empty() {
            // A unit with no directives is not given a cgroup of its own; see
            // `CgroupLimits::is_empty`.
            return Ok(None);
        }

        if let Err(e) = cgroup::create(&dir) {
            rlog!(
                "warning: {unit}: cannot create cgroup {}: {e}; the unit runs unlimited",
                dir.display()
            );
            return Ok(None);
        }

        // What this cgroup actually has, rather than what the boot-time enable
        // asked for: between the two, the kernel may not have the controller at
        // all, or may have refused it.
        let available = match cgroup::read_controllers(&dir) {
            Ok(list) => list,
            Err(e) => {
                rlog!(
                    "warning: {unit}: {} is not a usable cgroup v2 group: {e}; the unit runs unlimited",
                    dir.display()
                );
                return Ok(None);
            }
        };

        let mut applied = Vec::new();
        for (controller, file, value) in limits.files() {
            if !has_controller(&available, controller) {
                rlog!(
                    "warning: {unit}: this kernel has no '{controller}' controller; {file} not applied"
                );
                continue;
            }
            if let Err(e) = cgroup::write_limit(&dir, file, &value) {
                // `ENOENT` is the kernel saying it does not provide this
                // attribute at all, not a refusal of the value: `cpu.max` is
                // built only with `CONFIG_CFS_BANDWIDTH`, for instance, and the
                // `cpu` controller is there either way. That is the same kind of
                // difference as a missing controller - a capability of the
                // running kernel - so it gets the same answer: skip the limit,
                // say so, and let the unit run. Any other errno is the write
                // itself being refused, which is about what the unit asked for,
                // and that must not start a service the unit file claims is
                // bounded.
                if e.errno() == libc::ENOENT {
                    rlog!(
                        "warning: {unit}: this kernel does not provide {file}; that limit is not applied"
                    );
                    continue;
                }
                rlog!("{unit}: {file} = {value} failed in {}", dir.display());
                return Err(RuntimeError::from(e));
            }
            applied.push(file);
        }

        if applied.is_empty() {
            // Every directive was skipped: the unit asked to be bounded and this
            // kernel cannot bound it, which the warnings above have named. The
            // directory goes back down so a unit running in rinit's own cgroup
            // does not leave an empty one behind - and because placement has to
            // mean something, there is nothing to join.
            match cgroup::remove(&dir) {
                Ok(()) => {}
                Err(e) if e.errno() == libc::ENOENT => {}
                Err(e) => rlog!(
                    "warning: {unit}: cannot remove unused cgroup {}: {e}",
                    dir.display()
                ),
            }
            return Ok(None);
        }
        rlog!(
            "{unit}: cgroup {} with {} ({})",
            dir.display(),
            applied.join(", "),
            limits
                .files()
                .iter()
                .map(|(_, file, value)| format!("{file}={value}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        Ok(Some(dir.join("cgroup.procs")))
    }

    /// How many times the cgroup OOM killer has killed a process in this unit's
    /// cgroup (plan §7.3's "检测到 OOM kill 后进入重启策略").
    ///
    /// This is the reading half of that rule; the manager decides what to do
    /// with it. `None` means "no answer": no hierarchy, no cgroup for this unit,
    /// or a kernel that does not report the counter. It deliberately does not
    /// collapse into `0`, because `0` is a claim - "this unit's cgroup has not
    /// been OOM-killed" - and only the kernel can make it.
    pub fn oom_kills(&self, unit: &UnitName) -> Option<u64> {
        let dir = self.dir(unit)?;
        match cgroup::oom_kills(&dir) {
            Ok(count) => count,
            Err(e) => {
                // Diagnostic, so the unit's death is reported either way and
                // this only says why the cause could not be named.
                rlog!(
                    "warning: {unit}: cannot read {}/memory.events: {e}",
                    dir.display()
                );
                None
            }
        }
    }

    /// Send `signal` to every process in the unit's cgroup (plan §7.3's stop
    /// flow: "向 `cgroup.procs` 中所有 PID 发 `SIGTERM`").
    ///
    /// This is in addition to the process-group signal the manager already
    /// sends, not instead of it. A service that called `setsid` itself is out of
    /// its process group but still in its cgroup, and this is what reaches it.
    /// `cgroup.procs` only ever lists live processes, so there is no stale-pid
    /// hazard in signalling them one by one.
    pub fn signal_members(&self, unit: &UnitName, signal: libc::c_int) {
        let Some(dir) = self.dir(unit) else {
            return;
        };
        // No cgroup for this unit is the normal case for a unit without limits.
        let Ok(pids) = cgroup::processes(&dir) else {
            return;
        };
        for pid in pids {
            match process::kill(pid, signal) {
                Ok(()) => {}
                Err(e) if e.errno() == libc::ESRCH => {}
                Err(e) => {
                    rlog!("warning: {unit}: signal {signal} to cgroup pid {pid} failed: {e}");
                }
            }
        }
    }

    /// Remove the unit's cgroup once its processes are gone (plan §7.3).
    ///
    /// Called from the paths that leave a unit stopped, and idempotent: a unit
    /// that never had a cgroup, or whose cgroup is already gone, is a no-op.
    /// Plan §7.3's rule is that a directory still holding something is *kept*,
    /// with a warning - so this never fails a stop, and never turns a stray
    /// process into a leaked one.
    pub fn cleanup(&self, unit: &UnitName) {
        let Some(dir) = self.dir(unit) else {
            return;
        };
        if !dir.exists() {
            return;
        }
        // Plan §7.3 asks for `cgroup.events`'s `populated=0` as the confirmation,
        // so the directory is not removed while the kernel says it has members
        // (which is also what `rmdir` would answer with `EBUSY`).
        match cgroup::is_populated(&dir) {
            Ok(false) => {}
            Ok(true) => {
                let pids = cgroup::processes(&dir).unwrap_or_default();
                rlog!(
                    "warning: {unit}: cgroup {} still has {} process(es) ({pids:?}); keeping it",
                    dir.display(),
                    pids.len()
                );
                return;
            }
            Err(e) if e.errno() == libc::ENOENT => return,
            Err(e) => {
                rlog!(
                    "warning: {unit}: cannot confirm cgroup {} is empty ({e}); keeping it",
                    dir.display()
                );
                return;
            }
        }

        match cgroup::remove(&dir) {
            Ok(()) => rlog!("{unit}: removed cgroup {}", dir.display()),
            Err(e) if e.errno() == libc::ENOENT => {}
            Err(e) if busy(&e) => rlog!(
                "warning: {unit}: cgroup {} became busy again ({e}); keeping it",
                dir.display()
            ),
            Err(e) => rlog!(
                "warning: {unit}: cannot remove cgroup {}: {e}",
                dir.display()
            ),
        }
    }
}

/// Whether an `rmdir` failure means "something is still in there".
///
/// cgroupfs answers `EBUSY` for a populated cgroup; an ordinary directory - as
/// in this module's tests - answers `ENOTEMPTY` for a non-empty one. Both mean
/// the same thing to the caller, and both are warnings rather than failures.
fn busy(e: &rinit_runtime::SyscallError) -> bool {
    e.errno() == libc::EBUSY || e.errno() == libc::ENOTEMPTY
}

/// Bring the hierarchy up once, at boot (plan §7.3).
///
/// `skip_mounts` is `--dev`: a developer host has a real `/sys/fs/cgroup` and
/// rinit must not create units' cgroups inside it, so dev mode disables the
/// whole mechanism (with a log line) instead of probing the machine's own
/// hierarchy. An explicit `root` - `--cgroup-root` - is the caller saying where
/// the hierarchy is, and it wins over both, which is also what lets the host
/// integration tests drive this path against a directory of ordinary files.
pub fn prepare(skip_mounts: bool, root: Option<PathBuf>) -> Cgroup {
    let root = match root {
        Some(root) => root,
        None if skip_mounts => {
            rlog!("dev mode: cgroup limits are not applied");
            return Cgroup::disabled();
        }
        None => PathBuf::from(CGROUP_ROOT),
    };

    if !cgroup::is_v2(&root) {
        rlog!(
            "warning: no cgroup v2 hierarchy at {}; units run unlimited (plan §7.3)",
            root.display()
        );
        return Cgroup::disabled();
    }

    // The root's children get the controllers...
    enable_controllers(&root);
    // ...and rinit's own group passes them on to the units, which live one level
    // further down. Both writes are needed: a controller only exists in a cgroup
    // if it was enabled in the *parent's* `cgroup.subtree_control`.
    let group = root.join(GROUP);
    if let Err(e) = cgroup::create(&group) {
        rlog!(
            "warning: cannot create cgroup group {}: {e}; units run unlimited",
            group.display()
        );
        return Cgroup::disabled();
    }
    enable_controllers(&group);

    rlog!("cgroup v2 ready at {}", root.display());
    Cgroup::at(root)
}

/// Enable the controllers rinit needs for `dir`'s children, best effort.
///
/// A failure here is not fatal by design: the units' own [`Cgroup::setup`] reads
/// back what each cgroup really has, so a controller that could not be enabled
/// shows up as a skipped limit with a warning rather than as a broken boot.
fn enable_controllers(dir: &Path) {
    let available = match cgroup::read_controllers(dir) {
        Ok(list) => list,
        Err(e) => {
            rlog!(
                "warning: cannot read {}/cgroup.controllers: {e}",
                dir.display()
            );
            return;
        }
    };
    let Some(line) = controllers_to_enable(&available) else {
        rlog!(
            "warning: {} offers none of cpu/memory/pids; cgroup limits will not be enforced",
            dir.display()
        );
        return;
    };
    if let Err(e) = cgroup::enable_controllers(dir, &line) {
        rlog!(
            "warning: cannot enable '{line}' in {}/cgroup.subtree_control: {e}",
            dir.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use rinit_core::unit::config::{CpuQuota, SizeBytes};

    use super::*;

    /// A throwaway directory standing in for a mounted hierarchy, removed on
    /// drop. The tests below run the real code against it: the same
    /// `create`/`write_limit`/`is_populated`/`remove` calls that cgroupfs gets.
    struct Hierarchy {
        root: PathBuf,
    }

    impl Hierarchy {
        /// A hierarchy that a `prepare` probe accepts: the controllers file is
        /// what makes it one, and `subtree_control` files exist where cgroupfs
        /// always has them.
        ///
        /// cgroupfs grows `cgroup.controllers` and `cgroup.subtree_control` in
        /// every cgroup the kernel creates; a plain directory does not, so the
        /// fixture creates them wherever the code under test will look. That is
        /// the one place this stand-in is *less* forgiving than cgroupfs -
        /// elsewhere it is stricter (see [`Hierarchy::with_unit`]).
        fn new(tag: &str, controllers: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("rinit-bin-cgroup-{}-{tag}", std::process::id()));
            std::fs::remove_dir_all(&root).ok();
            std::fs::create_dir_all(&root).expect("scratch root");
            std::fs::write(root.join("cgroup.controllers"), controllers).unwrap();
            std::fs::write(root.join("cgroup.subtree_control"), "").unwrap();

            let group = root.join(GROUP);
            std::fs::create_dir_all(&group).unwrap();
            std::fs::write(group.join("cgroup.controllers"), controllers).unwrap();
            std::fs::write(group.join("cgroup.subtree_control"), "").unwrap();
            Hierarchy { root }
        }

        /// One unit's cgroup, with the attributes its limits will look for.
        ///
        /// Those attributes are ordinary files here, where cgroupfs provides
        /// them without them counting as content - which is why a `rmdir` that
        /// succeeds on a real hierarchy only warns here, and why the removal
        /// side of cleanup is asserted in `scripts/system-test.sh` against a
        /// real cgroupfs instead.
        fn with_unit(&self, unit: &str, controllers: &str, files: &[&str]) -> PathBuf {
            let dir = self.root.join(GROUP).join(unit);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("cgroup.controllers"), controllers).unwrap();
            for file in files {
                std::fs::write(dir.join(file), "").unwrap();
            }
            dir
        }

        fn read(&self, path: impl AsRef<Path>) -> String {
            std::fs::read_to_string(path.as_ref()).unwrap_or_default()
        }
    }

    impl Drop for Hierarchy {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).ok();
        }
    }

    fn unit(name: &str) -> UnitName {
        UnitName::parse(name).expect("valid unit name")
    }

    fn limits() -> CgroupLimits {
        CgroupLimits {
            cpu_max: Some(CpuQuota::Percent(50)),
            memory_max: SizeBytes::parse("32M"),
            memory_high: SizeBytes::parse("24M"),
            tasks_max: Some(64),
        }
    }

    /// A directory that is not a v2 hierarchy disables the whole mechanism,
    /// which is plan §7.3's degrade case - and `prepare` must not have created
    /// anything in it while deciding.
    #[test]
    fn a_root_without_controllers_disables_limits() {
        let dir =
            std::env::temp_dir().join(format!("rinit-bin-cgroup-{}-flat", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        let cgroup = prepare(false, Some(dir.clone()));
        assert!(
            !cgroup.is_enabled(),
            "no cgroup.controllers means units run unlimited"
        );
        assert!(
            std::fs::read_dir(&dir).unwrap().next().is_none(),
            "the probe must leave the directory alone"
        );
        assert_eq!(
            cgroup
                .setup(&unit("a.service"), &limits())
                .expect("disabled is not an error"),
            None
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The two controller writes plan §7.3 does not mention are the difference
    /// between the limit files existing and every write being `ENOENT`.
    #[test]
    fn prepare_enables_controllers_on_the_root_and_the_group() {
        let h = Hierarchy::new("enable", "cpuset cpu io memory pids\n");
        let cgroup = prepare(false, Some(h.root.clone()));
        assert!(cgroup.is_enabled());

        assert_eq!(
            h.read(h.root.join("cgroup.subtree_control")),
            "+cpu +memory +pids"
        );
        assert_eq!(
            h.read(h.root.join(GROUP).join("cgroup.subtree_control")),
            "+cpu +memory +pids",
            "the units live under rinit/, so rinit/ has to pass the controllers on"
        );
    }

    /// A kernel that offers only some controllers still gets those: the write is
    /// intersected with what exists rather than asserted.
    #[test]
    fn prepare_enables_only_what_the_kernel_offers() {
        let h = Hierarchy::new("partial", "memory pids\n");
        assert!(prepare(false, Some(h.root.clone())).is_enabled());
        assert_eq!(
            h.read(h.root.join("cgroup.subtree_control")),
            "+memory +pids",
            "no '+cpu' for a kernel without cpu"
        );
    }

    /// The whole per-unit path: every directive lands in its own file, and the
    /// return value is the `cgroup.procs` the child then joins.
    #[test]
    fn setup_writes_every_limit_and_names_the_procs_file() {
        let h = Hierarchy::new("setup", "cpu memory pids\n");
        let cgroup = prepare(false, Some(h.root.clone()));
        let dir = h.with_unit(
            "worker.service",
            "cpu memory pids\n",
            &["cpu.max", "memory.max", "memory.high", "pids.max"],
        );

        let procs = cgroup
            .setup(&unit("worker"), &limits())
            .expect("limits apply")
            .expect("a unit with limits joins a cgroup");
        assert_eq!(procs, dir.join("cgroup.procs"));

        assert_eq!(h.read(dir.join("cpu.max")), "50000 100000");
        assert_eq!(
            h.read(dir.join("memory.max")),
            (32u64 * 1024 * 1024).to_string()
        );
        assert_eq!(
            h.read(dir.join("memory.high")),
            (24u64 * 1024 * 1024).to_string()
        );
        assert_eq!(h.read(dir.join("pids.max")), "64");
    }

    /// A unit that asked for nothing is not given a cgroup, so there is nothing
    /// to place, nothing to clean up and no directory to litter the hierarchy.
    #[test]
    fn setup_gives_a_unit_without_limits_no_cgroup() {
        let h = Hierarchy::new("empty", "cpu memory pids\n");
        let cgroup = prepare(false, Some(h.root.clone()));

        assert_eq!(
            cgroup
                .setup(&unit("plain.service"), &CgroupLimits::default())
                .expect("nothing to do"),
            None
        );
        assert!(
            !h.root.join(GROUP).join("plain.service").exists(),
            "no directives means no directory"
        );
    }

    /// A controller this kernel does not have is a skipped limit, not a failed
    /// start: `CgroupTasksMax=` must not stop a kernel without `CONFIG_CGROUP_PIDS`
    /// from booting.
    #[test]
    fn a_missing_controller_skips_its_limit_instead_of_failing_the_unit() {
        let h = Hierarchy::new("nocpu", "memory pids\n");
        let cgroup = prepare(false, Some(h.root.clone()));
        let dir = h.with_unit(
            "mixed.service",
            "memory pids\n",
            &["memory.max", "memory.high", "pids.max"],
        );

        let procs = cgroup
            .setup(&unit("mixed"), &limits())
            .expect("the limits it can apply, it applies")
            .expect("the memory and pids limits join");
        assert_eq!(procs, dir.join("cgroup.procs"));

        assert!(
            !dir.join("cpu.max").exists(),
            "no controller, no attribute, and no write attempted"
        );
        assert_eq!(h.read(dir.join("pids.max")), "64");
    }

    /// A write to an attribute that *is* there failing is the one case that must
    /// fail the start: the unit asked to be bounded and cannot be.
    #[test]
    fn a_failed_limit_write_fails_the_start() {
        let h = Hierarchy::new("badwrite", "memory\n");
        let cgroup = prepare(false, Some(h.root.clone()));
        // `memory.max` is a *directory*, which the kernel does not do: the
        // stand-in for "the attribute is there and the write was refused"
        // (`EISDIR`), as against "the kernel does not provide it" (`ENOENT`,
        // covered below).
        let dir = h.with_unit("broken.service", "memory\n", &["memory.high"]);
        std::fs::create_dir(dir.join("memory.max")).unwrap();

        let err = cgroup
            .setup(&unit("broken"), &limits())
            .expect_err("a limit the kernel refuses must stop the start");
        assert!(
            err.to_string().contains("memory.max"),
            "the failing attribute is named: {err}"
        );
        assert!(
            dir.exists(),
            "the cgroup the unit will not use is left alone"
        );
    }

    /// An attribute the kernel does not provide at all is a *capability*, not a
    /// refusal: `cpu.max` needs `CONFIG_CFS_BANDWIDTH`, and a kernel without it
    /// still has the `cpu` controller. The limit is skipped with a warning, and
    /// a unit whose limits are all skipped runs unlimited in rinit's own cgroup
    /// rather than failing to start.
    #[test]
    fn an_attribute_the_kernel_does_not_provide_is_skipped_not_fatal() {
        let h = Hierarchy::new("noattr", "cpu memory pids\n");
        let cgroup = prepare(false, Some(h.root.clone()));
        // The controllers are all there, the attribute files are not - which is
        // what a `cpu.max` on a kernel without `CONFIG_CFS_BANDWIDTH` looks
        // like from here.
        let dir = h.with_unit("unbounded.service", "cpu memory pids\n", &[]);

        assert_eq!(
            cgroup
                .setup(&unit("unbounded"), &limits())
                .expect("a kernel that cannot express a limit is not a failed start"),
            None,
            "nothing could be applied, so there is nothing to join"
        );
        // The unused directory is asked to go back down, but a stand-in whose
        // attributes are ordinary files cannot be `rmdir`'d - the same
        // faithfulness limit the cleanup tests note. What is checked here is
        // that the attempt is not an error and the unit is not placed; the
        // removal itself is asserted on a real hierarchy by the guest's
        // `cgrun-gone`/`cglimit-gone` assertions.
        assert!(
            dir.exists(),
            "the stand-in's files keep the directory alive"
        );
    }

    /// Partial capability: the controllers the kernel does have are applied, and
    /// only the ones it cannot express are skipped.
    #[test]
    fn only_the_limits_this_kernel_cannot_express_are_skipped() {
        let h = Hierarchy::new("partial-attr", "cpu memory pids\n");
        let cgroup = prepare(false, Some(h.root.clone()));
        let dir = h.with_unit(
            "partial.service",
            "cpu memory pids\n",
            &["memory.max", "memory.high", "pids.max"],
        );

        let procs = cgroup
            .setup(&unit("partial"), &limits())
            .expect("the applicable limits apply")
            .expect("the unit is placed for the three it has");
        assert_eq!(procs, dir.join("cgroup.procs"));
        assert_eq!(
            h.read(dir.join("memory.max")),
            (32u64 * 1024 * 1024).to_string()
        );
        assert!(!dir.join("cpu.max").exists(), "no `cpu.max` was created");
    }

    /// Cleanup keeps a directory the kernel still reports as populated, and says
    /// so - plan §7.3's "确认 `populated=0` 再 `rmdir`".
    #[test]
    fn cleanup_keeps_a_cgroup_that_still_has_processes() {
        let h = Hierarchy::new("cleanup-full", "memory\n");
        let cgroup = prepare(false, Some(h.root.clone()));

        let kept = h.with_unit(
            "kept.service",
            "memory\n",
            &["cgroup.events", "cgroup.procs"],
        );
        std::fs::write(kept.join("cgroup.events"), "populated 1\n").unwrap();
        std::fs::write(kept.join("cgroup.procs"), "4242\n").unwrap();
        cgroup.cleanup(&unit("kept"));

        assert!(kept.exists(), "a populated cgroup is kept");
        assert_eq!(h.read(kept.join("cgroup.procs")), "4242\n");
    }

    /// A directory whose emptiness cannot be confirmed is kept as well: guessing
    /// "empty" would mean guessing that a process is dead.
    #[test]
    fn cleanup_does_not_guess_when_it_cannot_confirm_empty() {
        let h = Hierarchy::new("cleanup-blind", "memory\n");
        let cgroup = prepare(false, Some(h.root.clone()));

        // No `cgroup.events` at all: a directory that is not a usable cgroup.
        let dir = h.with_unit("blind.service", "memory\n", &[]);
        cgroup.cleanup(&unit("blind"));
        assert!(dir.exists(), "without `populated` there is no confirmation");
    }

    /// A hierarchy that refuses the `rmdir` is a warning, never a failed stop:
    /// the stand-in refuses it for ordinary-directory reasons, cgroupfs for its
    /// own (`EBUSY`), and both have to leave the unit stopped.
    #[test]
    fn cleanup_tolerates_a_refused_removal() {
        let h = Hierarchy::new("cleanup-busy", "memory\n");
        let cgroup = prepare(false, Some(h.root.clone()));

        let dir = h.with_unit("busy.service", "memory\n", &["cgroup.events"]);
        std::fs::write(dir.join("cgroup.events"), "populated 0\n").unwrap();
        cgroup.cleanup(&unit("busy"));
        assert!(
            dir.exists(),
            "the stand-in's attribute files make the directory non-empty, so the \
             removal is refused; cgroupfs would remove it, and the guest test asserts that"
        );
    }

    /// Cleanup is idempotent and silent about work it never did: a unit with no
    /// cgroup, or one already gone, must not be an error or a panic.
    #[test]
    fn cleanup_is_a_no_op_without_a_cgroup() {
        let h = Hierarchy::new("noop", "memory\n");
        let cgroup = prepare(false, Some(h.root.clone()));

        cgroup.cleanup(&unit("never-started.service"));
        cgroup.cleanup(&unit("never-started.service"));

        // A disabled cgroup does not touch anything at all.
        Cgroup::disabled().cleanup(&unit("anything.service"));
    }

    /// Signalling members is best effort and never fatal: pids that are already
    /// gone are the normal case on a stop path, and there must be no panic and
    /// no error for a unit whose cgroup does not exist.
    #[test]
    fn signalling_members_tolerates_missing_pids_and_missing_cgroups() {
        let h = Hierarchy::new("signal", "memory\n");
        let cgroup = prepare(false, Some(h.root.clone()));
        let dir = h.with_unit("signalled.service", "memory\n", &["cgroup.procs"]);
        // Two pids that cannot be live in this test's process tree; `kill` gets
        // `ESRCH` and the loop is expected to ignore it.
        std::fs::write(dir.join("cgroup.procs"), "2147483646\n2147483645\n").unwrap();

        cgroup.signal_members(&unit("signalled"), libc::SIGTERM);
        cgroup.signal_members(&unit("no-such-unit.service"), libc::SIGKILL);
        Cgroup::disabled().signal_members(&unit("signalled"), libc::SIGTERM);
    }

    /// The OOM counter is read back from the unit's own `memory.events`, and
    /// every way of having no answer stays `None` rather than becoming a `0`
    /// about a hierarchy that was never asked.
    #[test]
    fn the_oom_counter_is_read_from_the_units_memory_events() {
        let h = Hierarchy::new("oom", "memory\n");
        let cgroup = prepare(false, Some(h.root.clone()));

        let dir = h.with_unit("oomed.service", "memory\n", &["memory.events"]);
        std::fs::write(dir.join("memory.events"), "oom 3\noom_kill 1\n").unwrap();
        assert_eq!(cgroup.oom_kills(&unit("oomed")), Some(1));

        // The second kill moves the counter, which is what the manager compares
        // a start-time baseline against.
        std::fs::write(dir.join("memory.events"), "oom 4\noom_kill 2\n").unwrap();
        assert_eq!(cgroup.oom_kills(&unit("oomed")), Some(2));

        // A unit with no cgroup of its own, a kernel without the controller, and
        // a disabled hierarchy all have no answer to give.
        assert_eq!(cgroup.oom_kills(&unit("never-cgrouped.service")), None);
        let bare = h.with_unit("no-count.service", "memory\n", &[]);
        assert_eq!(cgroup.oom_kills(&unit("no-count")), None);
        assert_eq!(Cgroup::disabled().oom_kills(&unit("oomed")), None);
        assert!(bare.exists());
    }

    /// Dev mode must not touch the machine's own hierarchy, and an explicit root
    /// must win - that is what makes the host tests above possible.
    #[test]
    fn dev_mode_disables_limits_unless_a_root_is_named() {
        assert!(!prepare(true, None).is_enabled());

        let h = Hierarchy::new("devroot", "cpu\n");
        let explicit = prepare(true, Some(h.root.clone()));
        assert!(
            explicit.is_enabled(),
            "an explicit root is the caller saying where the hierarchy is"
        );
    }

    /// `--cgroup-root` may name a path that does not exist yet; the probe has to
    /// report that as unavailable rather than as a panic.
    #[test]
    fn a_missing_root_is_unavailable_not_a_panic() {
        let missing =
            std::env::temp_dir().join(format!("rinit-bin-cgroup-{}-absent", std::process::id()));
        std::fs::remove_dir_all(&missing).ok();
        assert!(!prepare(false, Some(missing)).is_enabled());
    }

    /// The busy test is the pair of errnos that mean "still occupied", on
    /// cgroupfs and elsewhere.
    #[test]
    fn busy_covers_cgroupfs_and_ordinary_directories() {
        assert!(busy(&rinit_runtime::SyscallError::new(
            "rmdir",
            libc::EBUSY
        )));
        assert!(busy(&rinit_runtime::SyscallError::new(
            "rmdir",
            libc::ENOTEMPTY
        )));
        assert!(!busy(&rinit_runtime::SyscallError::new(
            "rmdir",
            libc::ENOENT
        )));
    }

    /// The root path is built from the constant, so `--cgroup-root` and the
    /// first-wave mount point cannot drift apart.
    #[test]
    fn the_default_root_is_the_mount_point() {
        assert_eq!(OsStr::new(CGROUP_ROOT), OsStr::new("/sys/fs/cgroup"));
    }
}
