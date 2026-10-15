//! Unit manager: turns a unit directory into running processes (plan §3 M1).
//!
//! The M1 contract is narrow on purpose. It covers discovery, parsing,
//! dependency resolution and *parallel startup*; lifecycle state, restarts and
//! health checking are M2. What "started" means here is therefore the simplest
//! honest definition the plan allows: a `Type=simple` service is started once
//! it has been exec'd.
//!
//! Startup proceeds in *levels*. Every unit in a level has all of its
//! dependencies in earlier levels, so the whole level can be spawned before
//! moving on. That is where the parallelism comes from - and it is why the
//! acceptance criterion (10+ units, visibly faster than serial) falls out of
//! the schedule rather than needing special-casing.

use std::path::Path;

use rinit_core::unit::dependency::spawns_process;
use rinit_core::{DependencyGraph, ServiceKind, UnitConfig, UnitName};
use rinit_runtime::RuntimeError;
use rinit_runtime::units::load_unit_files;

use crate::log::rlog;

/// A fully resolved plan: the configs and the startup schedule.
///
/// The graph itself is consumed to produce `levels`, so it is not kept: the
/// schedule is what the boot loop needs, and holding the graph would imply an
/// API for dependency queries that M1 has no caller for.
#[derive(Debug)]
pub struct BootPlan {
    /// Parsed units, in deterministic order.
    pub configs: Vec<UnitConfig>,
    /// Startup levels; index 0 runs first, and each level may start in
    /// parallel.
    pub levels: Vec<Vec<UnitName>>,
    /// The boot target everything was resolved from.
    pub default_target: UnitName,
    /// Non-fatal problems worth reporting.
    pub warnings: Vec<String>,
}

/// Load, parse and resolve the unit directory.
///
/// # Errors
/// Returns a [`RuntimeError::Core`] for a parse failure or a dependency
/// problem (a cycle, a missing hard dependency). These are deliberately fatal:
/// plan §1.3(1) requires reference integrity to be checked before anything
/// starts, and a boot that silently skipped half its units would be worse than
/// one that refuses to start.
pub fn plan_boot(unit_dir: &Path, default_target: &str) -> Result<BootPlan, RuntimeError> {
    let (files, warnings) = load_unit_files(unit_dir)?;

    let mut configs: Vec<UnitConfig> = Vec::with_capacity(files.len());
    for file in &files {
        let cfg = rinit_core::unit::parser::parse(&file.name, &file.contents)
            .map_err(|e| annotate(e, &file.path))?;
        configs.push(cfg);
    }

    let target = UnitName::parse(default_target).map_err(RuntimeError::Core)?;

    let by_ref: Vec<&UnitConfig> = configs.iter().collect();
    let graph = DependencyGraph::build(by_ref).map_err(|e| RuntimeError::Core(e.into()))?;
    let mut warnings = warnings;
    warnings.extend(graph.warnings.iter().cloned());

    if !graph.units().contains(&target) {
        return Err(RuntimeError::Core(
            rinit_core::DependencyError::UnknownDefaultTarget { target }.into(),
        ));
    }

    // Only the default target's transitive closure gets started; an unrelated
    // unit in the directory is available but not pulled in.
    let wanted = graph.closure_of(&target);
    let levels = graph
        .levels()
        .map_err(|e| RuntimeError::Core(e.into()))?
        .into_iter()
        .map(|level| {
            level
                .into_iter()
                .filter(|unit| wanted.contains(unit))
                .collect::<Vec<_>>()
        })
        .filter(|level| !level.is_empty())
        .collect();

    Ok(BootPlan {
        configs,
        levels,
        default_target: target,
        warnings,
    })
}

/// Prefix a parse error with the file it came from.
fn annotate(err: rinit_core::ParseError, path: &Path) -> RuntimeError {
    RuntimeError::Io {
        op: "parse unit",
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: {err}", path.display()),
        ),
    }
}

/// Look up a unit's config by name.
pub fn config_of<'a>(plan: &'a BootPlan, name: &UnitName) -> Option<&'a UnitConfig> {
    plan.configs.iter().find(|c| &c.name == name)
}

/// The startup summary, one line per unit, for the boot log.
pub fn describe_plan(plan: &BootPlan) -> Vec<String> {
    let mut lines = Vec::new();
    for (index, level) in plan.levels.iter().enumerate() {
        for name in level {
            let kind = match config_of(plan, name) {
                Some(cfg) if cfg.is_target() => "target",
                Some(_) => "service",
                None => "unknown",
            };
            lines.push(format!("{index}: {name} ({kind})"));
        }
    }
    lines
}

/// Everything the boot loop needs to start one unit of a level.
///
/// Bundling the readiness inputs with the command keeps the caller from having
/// to look the unit up a second time, and keeps "what do we need to know to
/// start this" in one place.
#[derive(Debug, Clone, Copy)]
pub struct LevelEntry<'a> {
    /// The unit being started.
    pub name: &'a UnitName,
    /// Its `argv`, with `argv[0]` the program.
    pub argv: &'a [String],
    /// How startup completion is judged (plan §1.3(3)).
    pub kind: ServiceKind,
    /// `TimeoutStartSec`; `0` means wait forever (plan §1.1).
    pub timeout_start_sec: u64,
}

/// The services of a level, in schedule order.
///
/// Targets are absent: they are process-less and are never exec'd (plan §7.2).
pub fn commands_in_level<'a>(plan: &'a BootPlan, level: &'a [UnitName]) -> Vec<LevelEntry<'a>> {
    level
        .iter()
        .filter_map(|name| {
            let cfg = config_of(plan, name)?;
            spawns_process(cfg).then_some(LevelEntry {
                name,
                argv: cfg.exec_start.argv(),
                kind: cfg.service_kind,
                timeout_start_sec: cfg.timeout_start_sec,
            })
        })
        .collect()
}

/// Load and log a plan, reporting the level structure on the console.
///
/// # Errors
/// As [`plan_boot`].
pub fn announce_plan(unit_dir: &Path, default_target: &str) -> Result<BootPlan, RuntimeError> {
    let plan = plan_boot(unit_dir, default_target)?;
    for warning in &plan.warnings {
        rlog!("warning: {warning}");
    }
    rlog!(
        "boot plan for '{}': {} unit(s) in {} level(s)",
        plan.default_target,
        plan.configs.len(),
        plan.levels.len()
    );
    for line in describe_plan(&plan) {
        rlog!("plan {line}");
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use rinit_core::ServiceKind;

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("rinit-plan-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temp dir");
            TempDir(path)
        }

        fn write(&self, name: &str, contents: &str) {
            fs::write(self.0.join(name), contents).expect("write unit file");
        }

        /// A small but realistic tree: two roots, five leaves, one target.
        fn standard(&self) {
            self.write(
                "rinit-init.target",
                "[Unit]\nDescription=init\nType=target\n",
            );
            self.write(
                "storage.service",
                "[Unit]\nAfter=rinit-init.target\n[Service]\nExecStart=/bin/true\n",
            );
            self.write(
                "network.service",
                "[Unit]\nAfter=rinit-init.target\n[Service]\nExecStart=/bin/true\n",
            );
            for i in 0..5 {
                self.write(
                    &format!("app{i}.service"),
                    "[Unit]\nAfter=storage.service network.service\n[Service]\nExecStart=/bin/true\n",
                );
            }
            self.write(
                "multi-user.target",
                "[Unit]\nType=target\nRequires=rinit-init.target storage.service network.service app0.service app1.service app2.service app3.service app4.service\n",
            );
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The M1 acceptance criterion: a real dependency graph resolves into few
    /// levels, so most units start together.
    #[test]
    fn a_standard_tree_collapses_into_parallel_levels() {
        let dir = TempDir::new("levels");
        dir.standard();
        let plan = plan_boot(&dir.0, "multi-user.target").unwrap();

        // rinit-init -> {storage, network} -> {5 apps} -> multi-user.
        assert_eq!(plan.levels.len(), 4, "levels: {:?}", plan.levels);
        assert_eq!(
            plan.levels[0],
            [UnitName::parse("rinit-init.target").unwrap()]
        );
        assert_eq!(plan.levels[1].len(), 2, "storage and network run together");
        assert_eq!(plan.levels[2].len(), 5, "all five apps run together");
        // The boot target itself is last: it is reached once everything it
        // requires is up, and being a target it is never exec'd.
        assert_eq!(
            plan.levels[3],
            [UnitName::parse("multi-user.target").unwrap()]
        );
    }

    /// Only services are exec'd; targets must never be spawned (plan §7.2).
    #[test]
    fn targets_are_never_execed() {
        let dir = TempDir::new("targets");
        dir.standard();
        let plan = plan_boot(&dir.0, "multi-user.target").unwrap();

        let mut spawned = 0;
        for level in &plan.levels {
            for entry in commands_in_level(&plan, level) {
                assert!(
                    !entry.name.canonical().ends_with(".target"),
                    "{} is a target and must not be exec'd",
                    entry.name
                );
                spawned += 1;
            }
        }
        assert_eq!(spawned, 7, "7 services, 2 targets");
    }

    /// A unit outside the default target's closure must not be started.
    #[test]
    fn units_outside_the_closure_are_not_started() {
        let dir = TempDir::new("closure");
        dir.standard();
        dir.write("unrelated.service", "[Service]\nExecStart=/bin/true\n");
        let plan = plan_boot(&dir.0, "multi-user.target").unwrap();

        let scheduled: Vec<&UnitName> = plan.levels.iter().flatten().collect();
        assert!(
            !scheduled.contains(&&UnitName::parse("unrelated.service").unwrap()),
            "unrelated must not be pulled in: {scheduled:?}"
        );
        // It is still known, just not started.
        assert!(config_of(&plan, &UnitName::parse("unrelated.service").unwrap()).is_some());
    }

    /// Plan §1.3(1): a broken unit file must stop the boot with a message that
    /// names the file and the line.
    #[test]
    fn a_parse_error_names_the_file() {
        let dir = TempDir::new("parse-err");
        dir.write("broken.service", "[Service]\nExecStrt=/bin/true\n");
        let err = plan_boot(&dir.0, "multi-user.target").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("broken.service"), "got: {msg}");
        assert!(msg.contains("line 2"), "got: {msg}");
        assert!(msg.contains("ExecStrt"), "got: {msg}");
    }

    #[test]
    fn a_dependency_cycle_stops_the_boot() {
        let dir = TempDir::new("cycle");
        dir.write(
            "a.service",
            "[Unit]\nAfter=b.service\n[Service]\nExecStart=/bin/true\n",
        );
        dir.write(
            "b.service",
            "[Unit]\nAfter=a.service\n[Service]\nExecStart=/bin/true\n",
        );
        let err = plan_boot(&dir.0, "multi-user.target").unwrap_err();
        assert!(err.to_string().contains("cycle"), "got: {err}");
    }

    #[test]
    fn a_missing_hard_dependency_stops_the_boot() {
        let dir = TempDir::new("missing");
        dir.write(
            "a.service",
            "[Unit]\nRequires=ghost.service\n[Service]\nExecStart=/bin/true\n",
        );
        dir.write("all.target", "[Unit]\nType=target\nRequires=a.service\n");
        let err = plan_boot(&dir.0, "all.target").unwrap_err();
        assert!(err.to_string().contains("ghost.service"), "got: {err}");
    }

    #[test]
    fn a_missing_boot_target_is_reported() {
        let dir = TempDir::new("no-target");
        dir.write("a.service", "[Service]\nExecStart=/bin/true\n");
        let err = plan_boot(&dir.0, "multi-user.target").unwrap_err();
        assert!(err.to_string().contains("multi-user.target"), "got: {err}");
    }

    /// A soft dependency that is missing degrades to a warning and the boot
    /// still proceeds.
    #[test]
    fn a_missing_wants_target_only_warns() {
        let dir = TempDir::new("wants");
        dir.write(
            "a.service",
            "[Unit]\nWants=ghost.service\n[Service]\nExecStart=/bin/true\n",
        );
        dir.write("all.target", "[Unit]\nType=target\nRequires=a.service\n");
        let plan = plan_boot(&dir.0, "all.target").expect("must not fail");
        assert!(
            plan.warnings.iter().any(|w| w.contains("ghost.service")),
            "{:?}",
            plan.warnings
        );
    }

    /// Plan §1.1: `oneshot` must survive parsing into the plan, since M2 will
    /// act on it.
    #[test]
    fn service_kinds_survive_into_the_plan() {
        let dir = TempDir::new("kinds");
        dir.write(
            "once.service",
            "[Service]\nType=oneshot\nExecStart=/bin/true\n",
        );
        dir.write("all.target", "[Unit]\nType=target\nRequires=once.service\n");
        let plan = plan_boot(&dir.0, "all.target").unwrap();
        let cfg = config_of(&plan, &UnitName::parse("once.service").unwrap()).unwrap();
        assert_eq!(cfg.service_kind, ServiceKind::Oneshot);
    }

    #[test]
    fn the_plan_is_reproducible_across_runs() {
        let dir = TempDir::new("repro");
        dir.standard();
        let first = plan_boot(&dir.0, "multi-user.target").unwrap();
        let second = plan_boot(&dir.0, "multi-user.target").unwrap();
        assert_eq!(
            describe_plan(&first),
            describe_plan(&second),
            "the same directory must produce the same schedule"
        );
    }

    #[test]
    fn describe_plan_labels_services_and_targets() {
        let dir = TempDir::new("describe");
        dir.standard();
        let plan = plan_boot(&dir.0, "multi-user.target").unwrap();
        let lines = describe_plan(&plan);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("rinit-init.target (target)")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("storage.service (service)")),
            "{lines:?}"
        );
    }

    #[test]
    fn an_empty_unit_directory_is_an_error_not_an_empty_boot() {
        let dir = TempDir::new("empty");
        let err = plan_boot(&dir.0, "multi-user.target").unwrap_err();
        assert!(err.to_string().contains("multi-user.target"), "got: {err}");
    }
}
