//! Configuration reload: `rctl reload` (plan §7.6).
//!
//! The plan is four rules and two boundaries, and they are what this module is:
//!
//! 1. re-read the unit directory;
//! 2. diff it against what the manager holds - added, changed, removed;
//! 3. apply: a *stopped* unit's new definition takes effect immediately, a
//!    *running* one's waits for its next start, an added unit is registered and
//!    started only if the boot target pulls it in, and a removed unit is stopped
//!    and then hidden;
//! 4. a directory that does not parse is not applied at all.
//!
//! The last one is why this is short. `manager::plan_boot` loads, parses, builds
//! the dependency graph, checks the default target and resolves the levels - the
//! same function the boot used - and it either returns a whole schedule or an
//! error. Re-running it *is* the plan's double buffer: there is no second copy of
//! the configuration to half-update, because nothing is touched until the new one
//! exists in full. A cycle, a dangling `Requires=` or a removed default target
//! therefore rejects the reload with the running system untouched, which is what
//! "保持旧配置不变,不回滚到半更新状态" asks for.
//!
//! What a reload deliberately does *not* do is cascade. If a unit's file is
//! deleted while another unit still `Requires=` it, the graph rejects the whole
//! reload (the reference is dangling) rather than silently leaving a running
//! dependent without its dependency - so removing a unit means removing the
//! references to it, in the same reload.

use std::collections::BTreeSet;
use std::path::Path;

use rinit_core::{RequestSource, UnitConfig, UnitName};
use rinit_runtime::RuntimeError;

use crate::activation::Activated;
use crate::args::Config;
use crate::boot::{StartCoordinator, apply_effect};
use crate::log::rlog;
use crate::manager;
use crate::notify::NotifyHub;
use crate::unit_manager::{ConfigChange, UnitManager};

/// What one reload did.
///
/// The fields are the plan's three cases plus the two answers a caller has to be
/// able to see: which changed definitions are already in force and which are
/// waiting for a restart.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// Newly registered units.
    pub added: Vec<UnitName>,
    /// Added units the boot target requires, and which were therefore started.
    pub started: Vec<UnitName>,
    /// Changed definitions now in force (the unit was not running).
    pub applied: Vec<UnitName>,
    /// Changed definitions waiting for the unit's next start.
    pub deferred: Vec<UnitName>,
    /// Units whose file is gone.
    pub removed: Vec<UnitName>,
    /// Removed units that had to be stopped; a subset of `removed`.
    pub stopped: Vec<UnitName>,
    /// Tombstones a previous reload left behind, now dropped.
    pub pruned: usize,
    /// Units whose definition is identical.
    pub unchanged: usize,
}

impl Report {
    /// One line per action, for the console and for `rctl reload`.
    ///
    /// Separate from [`Report::summary`] because the interesting cases are the
    /// rare ones: a reload that changed nothing should say so in a line, and one
    /// that stopped a unit should name it. The summary is not included.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for unit in &self.stopped {
            out.push(format!("stopped {unit}: its unit file is gone"));
        }
        for unit in &self.removed {
            out.push(format!("removed {unit}"));
        }
        for unit in &self.applied {
            out.push(format!("{unit}: new definition in force"));
        }
        for unit in &self.deferred {
            out.push(format!(
                "{unit} is running; its new definition applies at the next start"
            ));
        }
        for unit in &self.added {
            out.push(format!("registered {unit}"));
        }
        for unit in &self.started {
            out.push(format!("started {unit}: the boot target requires it"));
        }
        if self.pruned > 0 {
            out.push(format!(
                "dropped {} unit(s) a previous reload removed",
                self.pruned
            ));
        }
        out
    }

    /// The counts, in one line.
    pub fn summary(&self) -> String {
        format!(
            "reload: {} added ({} started), {} changed ({} deferred), {} removed ({} stopped), {} unchanged",
            self.added.len(),
            self.started.len(),
            self.applied.len() + self.deferred.len(),
            self.deferred.len(),
            self.removed.len(),
            self.stopped.len(),
            self.unchanged,
        )
    }
}

/// Re-read the unit directory and apply the difference (plan §7.6).
///
/// # Errors
/// Whatever `plan_boot` refuses - a parse failure, a cycle, a missing hard
/// dependency, a default target that no longer exists. The running system is
/// untouched in every one of those cases.
pub async fn reload(
    cfg: &Config,
    manager: &mut UnitManager,
    activation: &mut Activated,
    notify: &mut NotifyHub,
    starts: &mut StartCoordinator,
) -> Result<Report, RuntimeError> {
    // The one fallible step, and it is fallible *before* anything changes.
    // Reading and parsing the whole directory happens on the blocking pool:
    // it can touch a slow filesystem, and doing it inline would stop the main
    // loop from reaping children, answering control clients and feeding the
    // watchdog for the duration.
    let unit_dir = cfg.unit_dir.clone();
    let default_target = cfg.default_target.clone();
    let plan = tokio::task::spawn_blocking(move || manager::plan_boot(&unit_dir, &default_target))
        .await
        .map_err(|e| RuntimeError::Join {
            op: "reload plan",
            detail: e.to_string(),
        })??;
    for warning in &plan.warnings {
        rlog!("warning: {warning}");
    }

    let mut report = Report {
        // Tombstones from an earlier reload are collected now, while the
        // directory is known to be the truth: an entry whose stop has landed is
        // dropped, so a unit that is added and removed repeatedly does not
        // accumulate dead entries.
        pruned: manager.prune_retired(),
        ..Report::default()
    };

    // 1. Gone from the directory: stop what is running, then hide it. A
    //    tombstone from an earlier reload is already hidden and is skipped.
    for name in manager.names() {
        if !manager.exists(&name) || manager::config_of(&plan, &name).is_some() {
            continue;
        }
        // An instance is created at runtime from a template, so it is not in
        // `plan.configs`. It is removed only when its template is gone; a
        // reload that still has `worker@.service` must leave `worker@1.service`
        // running.
        if name
            .template()
            .is_some_and(|template| plan.templates.contains_key(template.canonical()))
        {
            continue;
        }
        if manager.is_up(&name) || manager.has_cgroup_members(&name) {
            let effect = manager.request_stop(&name, RequestSource::UserStop);
            apply_effect(manager, &name, effect).await?;
            report.stopped.push(name.clone());
        }
        // A removed unit must not keep a live socket: a connection would still
        // wake the main loop for a name the directory no longer defines.
        activation.remove(&name);
        notify.disarm(&name);
        manager.clear_listener(&name);
        manager.retire(&name);
        report.removed.push(name);
    }

    // 2. Register or update everything the directory now holds.
    let mut added = BTreeSet::new();
    for config in &plan.configs {
        let name = config.name.clone();
        // A tombstone whose file came back is "added" for the schedule's
        // purposes even though the runtime entry still exists: it was hidden
        // and stopped, and the boot target should be able to pull it in again.
        let was_retired = manager.get(&name).is_some_and(|rt| rt.retired);
        let was_notify = manager
            .get(&name)
            .is_some_and(|rt| rt.config.service_kind == rinit_core::ServiceKind::Notify);
        let change = manager.add_with_in_flight(config.clone(), starts.is_starting(&name));
        if was_notify
            && config.service_kind != rinit_core::ServiceKind::Notify
            && matches!(change, ConfigChange::Applied)
        {
            // A running unit keeps its old definition until its next start, so
            // its notify socket must survive a deferred type change. Disarm it
            // only when the new definition is already in force; the start path
            // disarms a stale socket for a non-notify unit.
            notify.disarm(&name);
        }
        if was_retired {
            added.insert(name.clone());
            report.added.push(name);
        } else {
            match change {
                ConfigChange::Added => {
                    added.insert(name.clone());
                    report.added.push(name);
                }
                ConfigChange::Applied => report.applied.push(name),
                ConfigChange::Deferred => report.deferred.push(name),
                ConfigChange::Unchanged => report.unchanged += 1,
            }
        }
        // Socket activation is an rinit-owned resource, not the service's
        // process, so the directory's current definition wins even when the
        // running unit defers the rest of the change: a socket that moved must
        // not keep pointing at the old path, and a removed one must stop
        // waking the loop for a unit that will never start.
        reconcile_socket(activation, manager, config);
    }

    // 3. Start only what the boot target pulls in, and in the schedule's order -
    //    "新增单元 → 注册,不自动启动(除非是 default.target 依赖)". Levels still run
    //    earlier-first, so a unit added together with its dependency starts after
    //    it.
    for name in plan.levels.iter().flatten() {
        if !added.contains(name) {
            continue;
        }
        // Like boot, a socket-activated unit is bound rather than started: the
        // first connection is what brings it up.
        if manager::config_of(&plan, name).is_some_and(|config| config.socket_activate.is_some()) {
            rlog!("{name}: waiting for a connection");
            continue;
        }
        // A `ControlStart` job carries the unit's dependency closure and waits
        // for each prerequisite to become ready. Queueing one per added unit
        // keeps reload non-blocking without letting an `After=`/`Requires=`
        // dependent spawn ahead of what it needs.
        match starts.start_async(manager, name) {
            Ok(()) => report.started.push(name.clone()),
            Err(e) => rlog!("warning: cannot start {name}: {e}"),
        }
    }

    // The directory's templates come with it: a reload can add, change or remove
    // one, and instances made after it must be made from the new text (plan §7.7).
    manager.set_templates(plan.templates.clone());

    // 4. Shutdown walks the current schedule backwards (plan §1.3(8)), so the
    //    order is replaced with the one just resolved. Units started by hand and
    //    not in the schedule are appended by `shutdown_order` itself.
    manager.set_shutdown_order(plan.levels.iter().flatten().cloned().collect());

    for line in report.lines() {
        rlog!("reload: {line}");
    }
    rlog!("{}", report.summary());
    Ok(report)
}

/// Make rinit's held socket match `config`'s `SocketActivate=`.
///
/// Binding is idempotent for an unchanged path; a changed path replaces the old
/// socket and its watcher. A bind failure is logged and leaves the unit without
/// a listener, which is visible both in `rctl status` and in the fact that the
/// service is not started by a connection.
fn reconcile_socket(activation: &mut Activated, manager: &mut UnitManager, config: &UnitConfig) {
    let unit = &config.name;
    let desired = config.socket_activate.as_deref();
    let current = activation.path_of(unit).map(Path::to_path_buf);
    match (desired, current.as_deref()) {
        (None, None) => {}
        (None, Some(_)) => {
            activation.remove(unit);
            manager.clear_listener(unit);
            rlog!("{unit}: socket activation removed");
        }
        (Some(path), Some(current)) if current == Path::new(path) => {
            if let Some(fd) = activation.listener_fd(unit) {
                manager.set_listener(unit, fd);
            }
        }
        (Some(path), _) => {
            // `bind` binds the replacement before it drops the old listener, so
            // a failure leaves the old socket (and its watcher) intact.
            match activation.bind(unit, Path::new(path)) {
                Ok(()) => {
                    if let Some(fd) = activation.listener_fd(unit) {
                        manager.set_listener(unit, fd);
                    }
                }
                Err(e) => {
                    rlog!("error: {unit}: cannot bind {path}: {e}");
                    match activation.listener_fd(unit) {
                        Some(fd) => manager.set_listener(unit, fd),
                        None => manager.clear_listener(unit),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> UnitName {
        UnitName::parse(s).expect("valid unit name")
    }

    /// A reload that changed nothing is the common case and has to say exactly
    /// that, in one line, with no stray action lines.
    #[test]
    fn an_empty_report_has_no_lines_and_counts_zero() {
        let report = Report::default();
        assert!(report.lines().is_empty());
        assert_eq!(
            report.summary(),
            "reload: 0 added (0 started), 0 changed (0 deferred), 0 removed (0 stopped), 0 unchanged"
        );
    }

    /// A changed `SocketActivate=` is rebound immediately, and removing the
    /// directive takes the socket and watcher away. A stale socket would keep
    /// waking the loop for a path the unit no longer owns.
    #[tokio::test(flavor = "current_thread")]
    async fn socket_activation_follows_the_reloaded_definition() {
        let dir = std::env::temp_dir().join(format!("rinit-reload-socket-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = dir.join("first.sock");
        let second = dir.join("second.sock");

        let unit = name("api.service");
        let mut manager = UnitManager::new();
        let mut activation = Activated::disabled();
        let mut config = UnitConfig::new(unit.clone());
        config.socket_activate = Some(first.display().to_string());

        reconcile_socket(&mut activation, &mut manager, &config);
        assert!(first.exists());
        assert!(manager.listener_fd(&unit).is_some());

        config.socket_activate = Some(second.display().to_string());
        reconcile_socket(&mut activation, &mut manager, &config);
        assert!(!first.exists(), "the old path must be released");
        assert!(second.exists());
        assert!(manager.listener_fd(&unit).is_some());

        config.socket_activate = None;
        reconcile_socket(&mut activation, &mut manager, &config);
        assert!(!second.exists());
        assert!(manager.listener_fd(&unit).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The lines name each action once, in the order a reader needs: what went
    /// away, what changed, what arrived, and what is still deferred.
    #[test]
    fn each_action_gets_one_line() {
        let report = Report {
            added: vec![name("new.service")],
            started: vec![name("wanted.service")],
            applied: vec![name("changed.service")],
            deferred: vec![name("running.service")],
            removed: vec![name("gone.service")],
            stopped: vec![name("stopped.service")],
            pruned: 2,
            unchanged: 7,
        };

        assert_eq!(
            report.lines(),
            [
                "stopped stopped.service: its unit file is gone",
                "removed gone.service",
                "changed.service: new definition in force",
                "running.service is running; its new definition applies at the next start",
                "registered new.service",
                "started wanted.service: the boot target requires it",
                "dropped 2 unit(s) a previous reload removed",
            ]
        );
        // "changed" is applied + deferred, and "stopped" is a subset of
        // "removed": the summary counts the union, and names the subset, rather
        // than double-counting either.
        assert_eq!(
            report.summary(),
            "reload: 1 added (1 started), 2 changed (1 deferred), 1 removed (1 stopped), 7 unchanged"
        );
    }
}
