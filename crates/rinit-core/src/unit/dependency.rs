//! Dependency graph: construction, topological order, cycle detection
//! (plan §1.1, §1.3(1), §7.2).
//!
//! The graph is what makes M1's parallel startup correct, so its edges encode
//! the plan's distinctions precisely:
//!
//! - `Requires=` and `Wants=` are *activation* dependencies that also imply
//!   ordering (plan §1.1), so they produce edges like `After=` does.
//! - `After=` / `Before=` are ordering only and imply no liveness.
//! - `Conflicts=` is mutual exclusion and deliberately produces **no** start
//!   edge; it does not propagate transitively either (plan §1.1).
//!
//! Scheduling is expressed as *levels*: every unit whose dependencies are all
//! in earlier levels can be started at the same time, which is what turns a
//! serial boot into a parallel one.
//!
//! Ordering ties are broken by [`UnitName`]'s `Ord`, so two boots of the same
//! unit directory produce the same schedule.

// This module indexes `Vec`s by design, and every index is derived in the same
// function from the same `0..units.len()` range: `remaining`/`placed`/`marks`/
// `stack` are all sized from `units.len()`, `cursor` walks `dependencies[node]`
// while `cursor < len`, and `next` always comes from a `dependencies[i]` entry
// holding an index that was itself produced by `index_of`. Rewriting each of
// them as `.get(..)` would add a fallback branch per line that can never be
// taken, in the one place where the *algorithm* is what needs reading.
//
// Duplicate names - the only way `index_of` could miss - are rejected before
// any of this runs (see `build`), so the graph is always well formed.
#![allow(clippy::indexing_slicing)]

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use super::{UnitName, UnitType};
use crate::unit::config::UnitConfig;

/// Something wrong with the dependency structure of a unit directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyError {
    /// The dependency cycle, listed as a closed path `a -> b -> a`.
    Cycle {
        /// The cycle, starting and ending at the same unit.
        path: Vec<UnitName>,
    },
    /// A `Requires=`/`After=`/`Before=` names a unit with no file.
    MissingDependency {
        /// The unit that declared the reference.
        required_by: UnitName,
        /// The referenced unit that does not exist.
        missing: UnitName,
        /// The directive that referenced it.
        directive: &'static str,
    },
    /// The requested boot target is not in the unit directory.
    UnknownDefaultTarget {
        /// The requested target.
        target: UnitName,
    },
    /// Two units declare the same `Sockets=` path (plan §7.8).
    ///
    /// The path is what a `NeedsSocket=` resolves *to*, so two candidates for it
    /// means the declaration is ambiguous - and ambiguity there is a boot-time
    /// error rather than a guess about which unit to wait for.
    DuplicateSocket {
        /// The path that two units claimed.
        path: String,
        /// The first unit to claim it.
        first: UnitName,
        /// The second.
        second: UnitName,
    },
    /// Two inputs defined the same unit.
    ///
    /// The loader keys a unit by its file name, so a directory cannot produce
    /// this; the check lives here because the graph is where identity matters,
    /// and it keeps any other caller (a future reload, `rctl`, a test fixture)
    /// from silently merging two definitions the way `build` used to.
    DuplicateUnit {
        /// The unit that was defined twice.
        unit: UnitName,
    },
}

impl core::fmt::Display for DependencyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DependencyError::Cycle { path } => {
                write!(f, "dependency cycle: ")?;
                for (i, unit) in path.iter().enumerate() {
                    if i > 0 {
                        write!(f, " -> ")?;
                    }
                    write!(f, "{unit}")?;
                }
                Ok(())
            }
            DependencyError::MissingDependency {
                required_by,
                missing,
                directive,
            } => write!(
                f,
                "{required_by}: {directive}={missing} but no such unit file exists"
            ),
            DependencyError::UnknownDefaultTarget { target } => {
                write!(f, "boot target {target} not found in the unit directory")
            }
            DependencyError::DuplicateSocket {
                path,
                first,
                second,
            } => write!(
                f,
                "{first} and {second} both declare Sockets={path}; a socket path has to have one provider"
            ),
            DependencyError::DuplicateUnit { unit } => {
                write!(f, "{unit} is defined more than once")
            }
        }
    }
}

/// How strongly a reference binds the unit that declared it.
///
/// The variants exist so an unresolvable reference can be reported against the
/// directive that actually wrote it: plan §1.3(1) makes `Requires=` and `After=`
/// hard failures while `Wants=` is only a warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edge {
    /// `Requires=`: hard activation, and the referenced unit must exist.
    Requires,
    /// `Wants=`: soft activation; a missing unit degrades to a warning.
    Wants,
    /// `After=`: ordering only, but the reference must still resolve.
    After,
}

impl Edge {
    const fn directive(self) -> &'static str {
        match self {
            Edge::Requires => "Requires",
            Edge::Wants => "Wants",
            Edge::After => "After",
        }
    }
}

/// A resolved dependency graph over one unit directory.
#[derive(Debug, Clone, Default)]
pub struct DependencyGraph {
    /// Every known unit, sorted by name for deterministic iteration.
    units: Vec<UnitName>,
    /// For each unit index, the indices it must wait for before starting.
    ///
    /// This is the *ordering* relation: `Requires=`, `Wants=`, `After=` and the
    /// mirror of `Before=`. Used by [`levels`](Self::levels) and
    /// [`topological_order`](Self::topological_order).
    dependencies: Vec<Vec<usize>>,
    /// For each unit index, the indices it *activates*: `Requires=`/`Wants=`
    /// only. Used by [`closure_of`](Self::closure_of).
    ///
    /// Kept separate from `dependencies` because plan §1.1 makes `After=`
    /// ordering-only: an `After=`-referenced unit must be ordered first *if it
    /// is started anyway*, but must never be started *because of* that
    /// reference. Folding both into one adjacency (which this used to do) made
    /// `app.service` with `After=db.service` pull `db.service` into the boot.
    activations: Vec<Vec<usize>>,
    /// Soft references that could not be resolved, reported as warnings.
    pub warnings: Vec<String>,
}

/// Turn `Sockets=`/`NeedsSocket=` declarations into dependency edges
/// (plan §7.8).
///
/// Each path may be declared by exactly one unit; a `NeedsSocket=` with a
/// provider becomes a hard `Requires=` *and* an `After=` on it, because the
/// consumer cannot work before the provider is up. A `NeedsSocket=` with no
/// provider is not an error: the plan degrades it ("按 `Wants` 弱依赖降级处理"), and
/// the property that matters is kept anyway, since the consumer still waits for
/// the path itself before it starts. Those are returned as warnings, for the boot
/// log to say out loud.
///
/// # Errors
/// [`DependencyError::DuplicateSocket`] when two units claim one path.
pub fn resolve_sockets(configs: &mut [UnitConfig]) -> Result<Vec<String>, DependencyError> {
    // Owned copies rather than borrows: the loop below mutates the configs, and a
    // map of references into them would make that impossible.
    let mut providers: BTreeMap<String, UnitName> = BTreeMap::new();
    for config in configs.iter() {
        for path in &config.sockets {
            if let Some(first) = providers.insert(path.clone(), config.name.clone()) {
                return Err(DependencyError::DuplicateSocket {
                    path: path.clone(),
                    first,
                    second: config.name.clone(),
                });
            }
        }
    }

    let mut warnings = Vec::new();
    for config in configs.iter_mut() {
        let mut requires: Vec<UnitName> = Vec::new();
        for path in &config.needs_socket {
            match providers.get(path) {
                // A unit cannot declare its own socket as a prerequisite of
                // itself: that would be a self-loop, and the graph would reject
                // the boot for a unit file that reads harmlessly.
                Some(provider) if provider != &config.name => requires.push(provider.clone()),
                Some(_) => {}
                None => warnings.push(format!(
                    "{}: NeedsSocket={path} has no unit declaring it; waiting for the path anyway",
                    config.name
                )),
            }
        }
        for provider in requires {
            if !config.deps.requires.contains(&provider) {
                config.deps.requires.push(provider.clone());
            }
            if !config.deps.after.contains(&provider) {
                config.deps.after.push(provider);
            }
        }
    }
    Ok(warnings)
}

impl DependencyGraph {
    /// Build the graph from a set of parsed units.
    ///
    /// # Errors
    /// Returns [`DependencyError::MissingDependency`] for an unresolvable
    /// hard reference, and [`DependencyError::Cycle`] if the ordering edges
    /// form a loop.
    pub fn build<'a>(
        configs: impl IntoIterator<Item = &'a UnitConfig>,
    ) -> Result<Self, DependencyError> {
        // Collect once, then sort: a deterministic unit order is what makes
        // every index below (and the whole schedule) reproducible.
        let mut configs: Vec<&UnitConfig> = configs.into_iter().collect();
        configs.sort_by(|a, b| a.name.cmp(&b.name));

        let mut units: Vec<UnitName> = Vec::with_capacity(configs.len());
        for cfg in &configs {
            // Plan §1.1: two definitions of the same unit is an error, never a
            // silent merge.
            if units.contains(&cfg.name) {
                return Err(DependencyError::DuplicateUnit {
                    unit: cfg.name.clone(),
                });
            }
            units.push(cfg.name.clone());
        }
        units.sort();

        let index_of = |name: &UnitName| units.iter().position(|u| u == name);
        let mut dependencies: Vec<Vec<usize>> = vec![Vec::new(); units.len()];
        let mut activations: Vec<Vec<usize>> = vec![Vec::new(); units.len()];
        let mut warnings = Vec::new();

        for cfg in &configs {
            // Unreachable: `units` is exactly the configured names and
            // duplicates were rejected above. Skipping rather than panicking
            // keeps a future graph bug from taking PID 1 down with it.
            let Some(me) = index_of(&cfg.name) else {
                continue;
            };

            // (referenced unit, strength) pairs. `Requires`/`Wants` imply
            // ordering (plan §1.1), so they produce edges here too.
            let mut edges: Vec<(&UnitName, Edge)> = Vec::new();
            for unit in &cfg.deps.requires {
                edges.push((unit, Edge::Requires));
            }
            for unit in &cfg.deps.wants {
                edges.push((unit, Edge::Wants));
            }
            for unit in &cfg.deps.after {
                edges.push((unit, Edge::After));
            }
            // `Before=` is the mirror image: the referenced unit waits for us.
            for unit in &cfg.deps.before {
                match index_of(unit) {
                    Some(other) => push_unique(&mut dependencies[other], me),
                    None => {
                        return Err(DependencyError::MissingDependency {
                            required_by: cfg.name.clone(),
                            missing: unit.clone(),
                            directive: "Before",
                        });
                    }
                }
            }
            // `Conflicts=` intentionally contributes no start edge.

            for (unit, edge) in edges {
                let Some(other) = index_of(unit) else {
                    match edge {
                        // Plan §1.3(1): a missing hard reference is reported
                        // here, at build time, instead of at run time.
                        Edge::Requires | Edge::After => {
                            return Err(DependencyError::MissingDependency {
                                required_by: cfg.name.clone(),
                                missing: unit.clone(),
                                directive: edge.directive(),
                            });
                        }
                        Edge::Wants => {
                            warnings.push(format!(
                                "{}: Wants={} but no such unit file exists",
                                cfg.name, unit
                            ));
                            continue;
                        }
                    }
                };
                // A self-reference is deliberately *not* filtered out: it forms
                // a cycle, which the operator needs to fix rather than have
                // silently ignored (plan §4.1 lists self-dependency as a DAG
                // case).
                push_unique(&mut dependencies[me], other);
                // Only `Requires=`/`Wants=` activate (plan §1.1).
                if matches!(edge, Edge::Requires | Edge::Wants) {
                    push_unique(&mut activations[me], other);
                }
            }
        }

        let graph = DependencyGraph {
            units,
            dependencies,
            activations,
            warnings,
        };
        graph.check_acyclic()?;
        Ok(graph)
    }

    /// Every known unit, in deterministic order.
    pub fn units(&self) -> &[UnitName] {
        &self.units
    }

    /// The units `unit` must wait for before it may start.
    pub fn dependencies_of(&self, unit: &UnitName) -> Vec<UnitName> {
        match self.units.iter().position(|u| u == unit) {
            Some(i) => self.dependencies[i]
                .iter()
                .filter_map(|&d| self.units.get(d))
                .cloned()
                .collect(),
            None => Vec::new(),
        }
    }

    /// The units that are waiting on `unit`.
    pub fn dependents_of(&self, unit: &UnitName) -> Vec<UnitName> {
        let Some(me) = self.units.iter().position(|u| u == unit) else {
            return Vec::new();
        };
        self.dependencies
            .iter()
            .enumerate()
            .filter(|(_, deps)| deps.contains(&me))
            .filter_map(|(i, _)| self.units.get(i))
            .cloned()
            .collect()
    }

    /// Assign every unit a startup level.
    ///
    /// Level 0 has no dependencies; every other unit sits one level below its
    /// deepest dependency. All units in a level may start concurrently.
    ///
    /// # Errors
    /// [`DependencyError::Cycle`] if the graph is cyclic. Calling this after
    /// [`DependencyGraph::build`] cannot fail, since `build` already rejects
    /// cycles; the signature stays fallible so this can be used standalone.
    pub fn levels(&self) -> Result<Vec<Vec<UnitName>>, DependencyError> {
        let order = self.topological_order()?;
        let mut level_of: Vec<(UnitName, usize)> = Vec::with_capacity(order.len());
        let mut levels: Vec<Vec<UnitName>> = Vec::new();

        for unit in order {
            let deepest = self
                .dependencies_of(&unit)
                .iter()
                .map(|dep| {
                    level_of
                        .iter()
                        .find(|(n, _)| n == dep)
                        .map(|(_, l)| *l)
                        .unwrap_or(0)
                })
                .max()
                .unwrap_or(0);
            let level = if deepest == 0 && self.dependencies_of(&unit).is_empty() {
                0
            } else {
                deepest + 1
            };
            level_of.push((unit.clone(), level));
            if levels.len() <= level {
                levels.resize(level + 1, Vec::new());
            }
            levels[level].push(unit);
        }
        Ok(levels)
    }

    /// A deterministic topological order (Kahn's algorithm).
    ///
    /// # Errors
    /// [`DependencyError::Cycle`] with the offending path spelled out.
    pub fn topological_order(&self) -> Result<Vec<UnitName>, DependencyError> {
        let n = self.units.len();
        let mut remaining: Vec<usize> = (0..n).map(|i| self.dependencies[i].len()).collect();
        let mut ordered: Vec<UnitName> = Vec::with_capacity(n);
        let mut placed = vec![false; n];

        // Pick the ready unit with the smallest name each round: stable output
        // without a second sort.
        while ordered.len() < n {
            let next = (0..n)
                .filter(|&i| !placed[i] && remaining[i] == 0)
                .min_by(|&a, &b| self.units[a].cmp(&self.units[b]));
            let Some(next) = next else {
                return Err(self.find_cycle());
            };
            placed[next] = true;
            ordered.push(self.units[next].clone());
            // Anything that was waiting on `next` is one step closer.
            for (i, deps) in self.dependencies.iter().enumerate() {
                if deps.contains(&next) {
                    remaining[i] -= 1;
                }
            }
        }
        Ok(ordered)
    }

    /// Depth-first search for a concrete cycle, to report it.
    fn find_cycle(&self) -> DependencyError {
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            Unseen,
            OnStack,
            Done,
        }
        let n = self.units.len();
        let mut marks = vec![Mark::Unseen; n];
        let mut stack: Vec<usize> = Vec::new();

        // Iterative DFS so a deep graph cannot overflow the stack.
        for start in 0..n {
            if marks[start] != Mark::Unseen {
                continue;
            }
            let mut work: Vec<(usize, usize)> = vec![(start, 0)];
            marks[start] = Mark::OnStack;
            stack.push(start);

            while let Some((node, cursor)) = work.pop() {
                let deps = &self.dependencies[node];
                if cursor < deps.len() {
                    work.push((node, cursor + 1));
                    let next = deps[cursor];
                    match marks[next] {
                        Mark::Unseen => {
                            marks[next] = Mark::OnStack;
                            stack.push(next);
                            work.push((next, 0));
                        }
                        Mark::OnStack => {
                            // Found it: slice the cycle out of the stack.
                            let start_at = stack.iter().position(|&i| i == next).unwrap_or(0);
                            let path: Vec<UnitName> = stack[start_at..]
                                .iter()
                                .map(|&i| self.units[i].clone())
                                .collect();
                            let mut path = path;
                            path.push(self.units[next].clone());
                            return DependencyError::Cycle { path };
                        }
                        Mark::Done => {}
                    }
                } else {
                    marks[node] = Mark::Done;
                    stack.pop();
                }
            }
        }
        // Unreachable for a graph Kahn could not drain, but keep it total.
        DependencyError::Cycle {
            path: self.units.clone(),
        }
    }

    /// Verify the graph has no cycle (used by `build`).
    fn check_acyclic(&self) -> Result<(), DependencyError> {
        self.topological_order().map(|_| ())
    }

    /// Everything `target` transitively *activates*, including itself.
    ///
    /// Activation is `Requires=`/`Wants=` only (plan §1.1): `After=`/`Before=`
    /// are ordering-only, and a `Conflicts=` is an exclusion, so neither pulls
    /// a unit in. The result is sorted for determinism.
    pub fn closure_of(&self, target: &UnitName) -> Vec<UnitName> {
        let mut out: Vec<UnitName> = Vec::new();
        let mut stack = vec![target.clone()];
        while let Some(unit) = stack.pop() {
            if out.contains(&unit) {
                continue;
            }
            let activated: Vec<UnitName> = match self.units.iter().position(|u| u == &unit) {
                Some(i) => self.activations[i]
                    .iter()
                    .filter_map(|&d| self.units.get(d))
                    .cloned()
                    .collect(),
                None => Vec::new(),
            };
            out.push(unit.clone());
            for dep in activated {
                stack.push(dep);
            }
        }
        out.sort();
        out
    }
}

fn push_unique(list: &mut Vec<usize>, value: usize) {
    if !list.contains(&value) {
        list.push(value);
    }
}

/// Whether a unit spawns a process, i.e. whether it needs launching at all.
///
/// Targets are aggregation points (plan §7.2): they reach a terminal state
/// without ever being exec'd.
pub fn spawns_process(config: &UnitConfig) -> bool {
    config.kind == UnitType::Service
}

/// A convenience wrapper: build, then schedule, in one call.
///
/// # Errors
/// Propagates any [`DependencyError`] from [`DependencyGraph::build`].
pub fn schedule<'a>(
    configs: impl IntoIterator<Item = &'a UnitConfig>,
    default_target: &UnitName,
) -> Result<Vec<Vec<UnitName>>, DependencyError> {
    let graph = DependencyGraph::build(configs)?;
    if !graph.units().contains(default_target) {
        return Err(DependencyError::UnknownDefaultTarget {
            target: default_target.clone(),
        });
    }
    graph.levels()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> UnitName {
        UnitName::parse(s).expect("valid name")
    }

    fn unit(n: &str, requires: &[&str], after: &[&str], kind: UnitType) -> UnitConfig {
        let mut cfg = UnitConfig::new(name(n));
        cfg.kind = kind;
        cfg.deps.requires = requires.iter().map(|s| name(s)).collect();
        cfg.deps.after = after.iter().map(|s| name(s)).collect();
        cfg
    }

    /// A minimal three-unit graph: `a` and `b` are independent, `c` needs both.
    fn diamond() -> Vec<UnitConfig> {
        vec![
            unit("a.service", &[], &[], UnitType::Service),
            unit("b.service", &[], &[], UnitType::Service),
            unit(
                "c.service",
                &["a.service", "b.service"],
                &[],
                UnitType::Service,
            ),
        ]
    }

    /// A config with the two socket directives, and nothing else.
    fn socket_unit(n: &str, sockets: &[&str], needs: &[&str]) -> UnitConfig {
        let mut cfg = UnitConfig::new(name(n));
        cfg.sockets = sockets.iter().map(|s| String::from(*s)).collect();
        cfg.needs_socket = needs.iter().map(|s| String::from(*s)).collect();
        cfg
    }

    /// Plan §7.8: a `NeedsSocket=` becomes a hard dependency on the unit that
    /// declared the path - so the consumer waits for its provider - and one with
    /// no provider is a warning rather than a failure, because the consumer waits
    /// for the *path* either way.
    #[test]
    fn needs_socket_resolves_to_its_provider() {
        let mut configs = vec![
            socket_unit("api.service", &["/run/api.sock"], &[]),
            socket_unit("client.service", &[], &["/run/api.sock"]),
            socket_unit("orphan.service", &[], &["/run/nowhere.sock"]),
        ];

        let warnings = resolve_sockets(&mut configs).expect("one provider per path");
        assert_eq!(warnings.len(), 1, "only the orphan warns: {warnings:?}");
        assert!(warnings[0].contains("/run/nowhere.sock"), "{warnings:?}");

        let by_name = |n: &str| {
            configs
                .iter()
                .find(|c| c.name.canonical() == n)
                .unwrap()
                .clone()
        };
        let client = by_name("client.service");
        assert_eq!(client.deps.requires, [name("api.service")]);
        assert_eq!(
            client.deps.after,
            [name("api.service")],
            "and the ordering edge comes with it"
        );
        assert!(
            by_name("api.service").deps.requires.is_empty(),
            "the provider does not depend on its consumer"
        );

        // And the graph puts the provider first, which is the point of the edge.
        let graph = DependencyGraph::build(&configs).unwrap();
        let levels = graph.levels().unwrap();
        let level_of = |n: &str| {
            levels
                .iter()
                .position(|level| level.iter().any(|u| u == &name(n)))
                .expect("scheduled")
        };
        assert!(level_of("api.service") < level_of("client.service"));
    }

    /// Two units claiming one path is a boot-time error: a `NeedsSocket=`
    /// resolves *to* a provider, so two candidates for one path is an ambiguity
    /// nobody can resolve later.
    #[test]
    fn two_units_declaring_one_socket_is_an_error() {
        let mut configs = vec![
            socket_unit("api.service", &["/run/api.sock"], &[]),
            socket_unit("other.service", &["/run/api.sock"], &[]),
        ];
        match resolve_sockets(&mut configs) {
            Err(DependencyError::DuplicateSocket {
                path,
                first,
                second,
            }) => {
                assert_eq!(path, "/run/api.sock");
                assert_eq!(
                    (first.canonical(), second.canonical()),
                    ("api.service", "other.service")
                );
            }
            other => panic!("expected a duplicate-socket error, got {other:?}"),
        }
    }

    /// A unit that declares its own socket and needs it is not turned into a
    /// self-loop: the declaration reads harmlessly, and the graph would refuse
    /// the boot for it.
    #[test]
    fn a_unit_that_needs_its_own_socket_is_left_alone() {
        let mut configs = vec![socket_unit(
            "api.service",
            &["/run/api.sock"],
            &["/run/api.sock"],
        )];
        let warnings = resolve_sockets(&mut configs).expect("no duplicate");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(configs[0].deps.requires.is_empty());
        assert!(DependencyGraph::build(&configs).is_ok(), "no cycle to find");
    }

    #[test]
    fn independent_units_share_level_zero() {
        let configs = diamond();
        let graph = DependencyGraph::build(&configs).unwrap();
        let levels = graph.levels().unwrap();
        assert_eq!(
            levels[0],
            [name("a.service"), name("b.service")],
            "a and b have no dependencies and may start together"
        );
        assert_eq!(levels[1], [name("c.service")]);
        assert_eq!(levels.len(), 2);
    }

    /// The M1 acceptance criterion: a wide graph must collapse into few
    /// levels, i.e. most units start in parallel.
    #[test]
    fn a_wide_graph_starts_mostly_in_parallel() {
        let base = unit("base.service", &[], &[], UnitType::Service);
        let configs: Vec<UnitConfig> = (0..10)
            .map(|i| {
                unit(
                    &format!("svc{i}.service"),
                    &[],
                    &["base.service"],
                    UnitType::Service,
                )
            })
            .collect();
        let by_ref: Vec<&UnitConfig> = configs.iter().chain([&base]).collect();

        let levels = schedule(by_ref, &name("svc0.service")).unwrap();
        assert_eq!(
            levels[0],
            [name("base.service")],
            "the single root starts alone"
        );
        assert_eq!(
            levels[1].len(),
            10,
            "all ten dependents may start concurrently: {levels:?}"
        );
    }

    #[test]
    fn topological_order_respects_edges() {
        let configs = diamond();
        let graph = DependencyGraph::build(&configs).unwrap();
        let order = graph.topological_order().unwrap();
        let pos = |n: &str| order.iter().position(|u| u == &name(n)).unwrap();
        assert!(pos("a.service") < pos("c.service"));
        assert!(pos("b.service") < pos("c.service"));
    }

    #[test]
    fn topological_order_is_deterministic() {
        let configs: Vec<UnitConfig> = ["e", "d", "c", "b", "a"]
            .iter()
            .map(|n| unit(&format!("{n}.service"), &[], &[], UnitType::Service))
            .collect();
        let first = DependencyGraph::build(&configs)
            .unwrap()
            .topological_order();
        // Same input, reversed construction order, same schedule.
        let reversed: Vec<UnitConfig> = configs.iter().rev().cloned().collect();
        let second = DependencyGraph::build(&reversed)
            .unwrap()
            .topological_order();
        assert_eq!(first, second);
    }

    /// A cycle must be reported *with its path*, otherwise the operator has no
    /// idea which units to edit.
    #[test]
    fn direct_cycle_names_the_path() {
        let configs = vec![
            unit("a.service", &["b.service"], &[], UnitType::Service),
            unit("b.service", &["a.service"], &[], UnitType::Service),
        ];
        let err = DependencyGraph::build(&configs).unwrap_err();
        let DependencyError::Cycle { path } = err else {
            panic!("expected a cycle, got {err:?}");
        };
        assert!(path.len() >= 3, "a closed path needs a repeat: {path:?}");
        assert_eq!(path.first(), path.last(), "the path must close: {path:?}");
        assert!(path.contains(&name("a.service")) && path.contains(&name("b.service")));
    }

    #[test]
    fn longer_cycle_names_the_path() {
        let configs = vec![
            unit("a.service", &["c.service"], &[], UnitType::Service),
            unit("b.service", &["a.service"], &[], UnitType::Service),
            unit("c.service", &["b.service"], &[], UnitType::Service),
        ];
        let DependencyError::Cycle { path } = DependencyGraph::build(&configs).unwrap_err() else {
            panic!("expected a cycle");
        };
        assert_eq!(path.len(), 4, "{path:?}");
        assert_eq!(path[0], path[3]);
    }

    #[test]
    fn self_dependency_is_a_cycle() {
        let configs = vec![unit("a.service", &["a.service"], &[], UnitType::Service)];
        assert!(matches!(
            DependencyGraph::build(&configs),
            Err(DependencyError::Cycle { .. })
        ));
    }

    /// A cycle that runs through unrelated units must not be missed.
    #[test]
    fn cycle_is_found_even_when_independent_units_interleave() {
        let configs = vec![
            unit("a.service", &["b.service"], &[], UnitType::Service),
            unit("b.service", &["a.service"], &[], UnitType::Service),
            unit("free.service", &[], &[], UnitType::Service),
        ];
        assert!(matches!(
            DependencyGraph::build(&configs),
            Err(DependencyError::Cycle { .. })
        ));
    }

    /// Plan §1.3(1): hard references must be resolved at build time, not left
    /// to blow up at runtime.
    #[test]
    fn missing_requires_is_an_error() {
        let configs = vec![unit(
            "a.service",
            &["ghost.service"],
            &[],
            UnitType::Service,
        )];
        let err = DependencyGraph::build(&configs).unwrap_err();
        assert_eq!(
            err,
            DependencyError::MissingDependency {
                required_by: name("a.service"),
                missing: name("ghost.service"),
                directive: "Requires",
            }
        );
    }

    #[test]
    fn missing_after_is_an_error() {
        let configs = vec![unit(
            "a.service",
            &[],
            &["ghost.service"],
            UnitType::Service,
        )];
        assert!(matches!(
            DependencyGraph::build(&configs),
            Err(DependencyError::MissingDependency {
                directive: "After",
                ..
            })
        ));
    }

    /// Plan §1.3(1): a missing `Wants` target degrades to a warning.
    #[test]
    fn missing_wants_is_only_a_warning() {
        let configs = vec![UnitConfig {
            deps: super::super::config::Dependencies {
                wants: vec![name("ghost.service")],
                ..Default::default()
            },
            ..unit("a.service", &[], &[], UnitType::Service)
        }];
        let graph = DependencyGraph::build(&configs).expect("must not fail");
        assert_eq!(graph.warnings.len(), 1, "{:?}", graph.warnings);
        assert!(
            graph.warnings[0].contains("ghost.service"),
            "{:?}",
            graph.warnings
        );
    }

    /// `Conflicts=` must not become a start edge, or mutually exclusive units
    /// would deadlock each other.
    #[test]
    fn conflicts_contributes_no_start_edge() {
        let cfg = UnitConfig {
            deps: super::super::config::Dependencies {
                conflicts: vec![name("other.service")],
                ..Default::default()
            },
            ..unit("a.service", &[], &[], UnitType::Service)
        };
        let graph =
            DependencyGraph::build([&cfg, &unit("other.service", &[], &[], UnitType::Service)])
                .unwrap();
        assert!(graph.dependencies_of(&name("a.service")).is_empty());
        assert!(graph.dependencies_of(&name("other.service")).is_empty());
    }

    /// Plan §1.1: `Before=` is the mirror of `After=`. `late Before=early`
    /// means late starts first, so *early* is the one that waits.
    #[test]
    fn before_makes_the_other_unit_wait() {
        let configs = vec![
            unit("early.service", &[], &[], UnitType::Service),
            UnitConfig {
                deps: super::super::config::Dependencies {
                    before: vec![name("early.service")],
                    ..Default::default()
                },
                ..unit("late.service", &[], &[], UnitType::Service)
            },
        ];
        let graph = DependencyGraph::build(&configs).unwrap();
        assert!(
            graph.dependencies_of(&name("late.service")).is_empty(),
            "late waits for nothing"
        );
        assert_eq!(
            graph.dependencies_of(&name("early.service")),
            [name("late.service")],
            "early must wait for late"
        );
    }

    #[test]
    fn duplicate_references_produce_one_edge() {
        let cfg = UnitConfig {
            deps: super::super::config::Dependencies {
                requires: vec![name("a.service")],
                after: vec![name("a.service")],
                ..Default::default()
            },
            ..unit("b.service", &[], &[], UnitType::Service)
        };
        let graph = DependencyGraph::build([&cfg, &unit("a.service", &[], &[], UnitType::Service)])
            .unwrap();
        assert_eq!(graph.dependencies_of(&name("b.service")).len(), 1);
    }

    /// Targets take part in the graph exactly like services (plan §7.2).
    #[test]
    fn targets_participate_in_ordering() {
        let configs = vec![
            unit("net.service", &[], &[], UnitType::Service),
            unit("net.target", &["net.service"], &[], UnitType::Target),
            unit("app.service", &["net.target"], &[], UnitType::Service),
        ];
        let graph = DependencyGraph::build(&configs).unwrap();
        let levels = graph.levels().unwrap();
        assert_eq!(levels[0], [name("net.service")]);
        assert_eq!(levels[1], [name("net.target")]);
        assert_eq!(levels[2], [name("app.service")]);
    }

    #[test]
    fn closure_includes_transitive_dependencies() {
        let configs = vec![
            unit("a.service", &[], &[], UnitType::Service),
            unit("b.service", &["a.service"], &[], UnitType::Service),
            unit("all.target", &["b.service"], &[], UnitType::Target),
            unit("unrelated.service", &[], &[], UnitType::Service),
        ];
        let graph = DependencyGraph::build(&configs).unwrap();
        let closure = graph.closure_of(&name("all.target"));
        assert!(closure.contains(&name("a.service")), "{closure:?}");
        assert!(closure.contains(&name("b.service")), "{closure:?}");
        assert!(closure.contains(&name("all.target")), "{closure:?}");
        assert!(!closure.contains(&name("unrelated.service")), "{closure:?}");
    }

    /// Plan §1.1: `After=` is ordering-only, so it must *not* activate the
    /// referenced unit. A unit that only says `After=db.service` must not drag
    /// `db.service` into the boot - it is only ordered after it, if it happens
    /// to be started for some other reason.
    #[test]
    fn after_does_not_activate_the_referenced_unit() {
        let configs = vec![
            unit("db.service", &[], &[], UnitType::Service),
            // Only ordering: no Requires/Wants.
            unit("app.service", &[], &["db.service"], UnitType::Service),
        ];
        let graph = DependencyGraph::build(&configs).unwrap();

        // Ordering is still recorded...
        assert_eq!(
            graph.dependencies_of(&name("app.service")),
            [name("db.service")],
            "After= still orders app after db"
        );
        // ...but app does not activate db.
        let closure = graph.closure_of(&name("app.service"));
        assert_eq!(
            closure,
            [name("app.service")],
            "After= must not pull the referenced unit in: {closure:?}"
        );
    }

    /// `Wants=` is a weak *activation*, so unlike `After=` it does pull the
    /// referenced unit in.
    #[test]
    fn wants_activates_the_referenced_unit() {
        let mut app = unit("app.service", &[], &[], UnitType::Service);
        app.deps.wants = vec![name("metrics.service")];
        let configs = vec![app, unit("metrics.service", &[], &[], UnitType::Service)];
        let graph = DependencyGraph::build(&configs).unwrap();

        let closure = graph.closure_of(&name("app.service"));
        assert!(
            closure.contains(&name("metrics.service")),
            "Wants= must activate: {closure:?}"
        );
    }

    /// Plan §1.1: `Wants=` implies ordering too, not just activation.
    #[test]
    fn wants_also_orders() {
        let mut app = unit("app.service", &[], &[], UnitType::Service);
        app.deps.wants = vec![name("metrics.service")];
        let configs = vec![app, unit("metrics.service", &[], &[], UnitType::Service)];
        let graph = DependencyGraph::build(&configs).unwrap();

        assert_eq!(
            graph.dependencies_of(&name("app.service")),
            [name("metrics.service")],
            "Wants= must order app after metrics"
        );
    }

    /// Plan §1.1: `Conflicts=` is not transitive - only a directly declared
    /// pair is mutually exclusive.
    #[test]
    fn conflicts_is_not_transitive() {
        let with_conflict = |me: &str, other: &str| {
            let mut cfg = unit(me, &[], &[], UnitType::Service);
            cfg.deps.conflicts = vec![name(other)];
            cfg
        };
        let configs = vec![
            with_conflict("a.service", "b.service"),
            with_conflict("b.service", "c.service"),
            unit("c.service", &[], &[], UnitType::Service),
        ];
        let graph = DependencyGraph::build(&configs).unwrap();

        // a and c never mention each other, so they are free to coexist: no
        // edge of any kind.
        assert_eq!(
            graph.closure_of(&name("a.service")),
            [name("a.service")],
            "a Conflicts b must not drag c in"
        );
        assert!(graph.dependencies_of(&name("a.service")).is_empty());
        assert!(graph.dependencies_of(&name("c.service")).is_empty());
    }

    #[test]
    fn empty_graph_is_valid() {
        let graph = DependencyGraph::build([]).unwrap();
        assert!(graph.units().is_empty());
        assert!(graph.topological_order().unwrap().is_empty());
        assert!(graph.levels().unwrap().is_empty());
    }

    /// Plan §1.1: two definitions of one unit must be rejected, not merged.
    #[test]
    fn a_duplicate_unit_is_an_error() {
        let configs = vec![
            unit("dup.service", &[], &[], UnitType::Service),
            unit("dup.service", &["a.service"], &[], UnitType::Service),
        ];
        let err = DependencyGraph::build(&configs).unwrap_err();
        assert_eq!(
            err,
            DependencyError::DuplicateUnit {
                unit: name("dup.service")
            }
        );
        assert!(format!("{err}").contains("dup.service"), "got: {err}");
    }

    #[test]
    fn schedule_rejects_an_unknown_boot_target() {
        let configs = diamond();
        let by_ref: Vec<&UnitConfig> = configs.iter().collect();
        let err = schedule(by_ref, &name("nope.target")).unwrap_err();
        assert_eq!(
            err,
            DependencyError::UnknownDefaultTarget {
                target: name("nope.target")
            }
        );
    }

    /// Only services need launching; targets must not be exec'd (plan §7.2).
    #[test]
    fn only_services_spawn_a_process() {
        let target = unit("x.target", &[], &[], UnitType::Target);
        let service = unit("y.service", &[], &[], UnitType::Service);
        assert!(!spawns_process(&target));
        assert!(spawns_process(&service));
    }

    /// A `Conflicts` pair must be schedulable: mutual exclusion is not a start
    /// edge, so neither unit may end up waiting for the other.
    #[test]
    fn conflicting_units_can_both_be_scheduled() {
        let configs = vec![
            UnitConfig {
                deps: super::super::config::Dependencies {
                    conflicts: vec![name("b.service")],
                    ..Default::default()
                },
                ..unit("a.service", &[], &[], UnitType::Service)
            },
            UnitConfig {
                deps: super::super::config::Dependencies {
                    conflicts: vec![name("a.service")],
                    ..Default::default()
                },
                ..unit("b.service", &[], &[], UnitType::Service)
            },
        ];
        let levels = schedule(&configs, &name("a.service")).unwrap();
        assert_eq!(
            levels[0].len(),
            2,
            "neither may wait for the other: {levels:?}"
        );
    }
}
