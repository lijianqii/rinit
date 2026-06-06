//! Dependency resolver — builds a DAG and computes the activation order
//! using Kahn topological sorting algorithm.

use crate::types::Unit;
use std::collections::{HashMap, VecDeque};

/// Build a dependency graph from loaded units.
pub fn build_dep_graph(units: &HashMap<String, Unit>) -> HashMap<String, Vec<String>> {
    let mut graph: HashMap<String, Vec<String>> = HashMap::new();

    for (name, unit) in units {
        for dep in &unit.unit.requires {
            graph.entry(dep.clone()).or_default().push(name.clone());
        }
        for dep in &unit.unit.wants {
            graph.entry(dep.clone()).or_default().push(name.clone());
        }
        graph.entry(name.clone()).or_default();
    }

    graph
}

/// Resolve startup order using Kahn algorithm.
/// Returns layers — units within the same layer can start in parallel.
pub fn resolve_startup_order(
    units: &HashMap<String, Unit>,
) -> Result<Vec<Vec<String>>, DepError> {
    let graph = build_dep_graph(units);

    let mut in_degree: HashMap<String, usize> = HashMap::new();
    for name in units.keys() {
        in_degree.entry(name.clone()).or_insert(0);
    }
    for deps in graph.values() {
        for dep in deps {
            *in_degree.entry(dep.clone()).or_insert(0) += 1;
        }
    }

    let mut queue: VecDeque<String> = in_degree
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(n, _)| n.clone())
        .collect();

    let mut layers = Vec::new();
    let mut visited = 0;

    while !queue.is_empty() {
        let layer: Vec<String> = queue.drain(..).collect();
        visited += layer.len();

        for node in &layer {
            if let Some(deps) = graph.get(node) {
                for dep in deps {
                    if let Some(deg) = in_degree.get_mut(dep) {
                        *deg -= 1;
                        if *deg == 0 {
                            queue.push_back(dep.clone());
                        }
                    }
                }
            }
        }

        layers.push(layer);
    }

    if visited != units.len() {
        let unvisited: Vec<_> = units
            .keys()
            .filter(|k| in_degree.get(*k).map_or(true, |&d| d > 0))
            .cloned()
            .collect();
        return Err(DepError::Cycle(unvisited));
    }

    Ok(layers)
}

/// Detect conflicting units that should not run simultaneously.
pub fn detect_conflicts(units: &HashMap<String, Unit>) -> Vec<(String, String)> {
    let mut conflicts = Vec::new();
    for (name, unit) in units {
        for conflict in &unit.unit.conflicts {
            if units.contains_key(conflict) {
                conflicts.push((name.clone(), conflict.clone()));
            }
        }
    }
    conflicts
}

#[derive(Debug, thiserror::Error)]
pub enum DepError {
    #[error("circular dependency detected involving: {0:?}")]
    Cycle(Vec<String>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Unit, UnitSection};

    fn make_unit(name: &str, requires: &[&str]) -> Unit {
        Unit {
            name: name.to_string(),
            unit: UnitSection {
                requires: requires.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
            service: None,
            socket: None,
            mount: None,
        }
    }

    #[test]
    fn linear_chain() {
        let mut units = HashMap::new();
        units.insert("a.service".into(), make_unit("a.service", &[]));
        units.insert("b.service".into(), make_unit("b.service", &["a.service"]));
        units.insert("c.service".into(), make_unit("c.service", &["b.service"]));

        let layers = resolve_startup_order(&units).unwrap();
        assert_eq!(layers.len(), 3);
    }

    #[test]
    fn parallel_layer() {
        let mut units = HashMap::new();
        units.insert("a.service".into(), make_unit("a.service", &[]));
        units.insert("b.service".into(), make_unit("b.service", &[]));
        units.insert("c.service".into(), make_unit("c.service", &["a.service", "b.service"]));

        let layers = resolve_startup_order(&units).unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].len(), 2);
    }

    #[test]
    fn cycle_detection() {
        let mut units = HashMap::new();
        units.insert("a.service".into(), make_unit("a.service", &["b.service"]));
        units.insert("b.service".into(), make_unit("b.service", &["a.service"]));

        let result = resolve_startup_order(&units);
        assert!(result.is_err());
    }
}
