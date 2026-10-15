//! Dependency DAG: construction, topological sort, cycle detection (plan §1.3(1));
//! lands with M1.
//!
//! M1 will build the directed graph here and drive parallel startup from it.
//!
//! Semantics to implement (plan §1.1, §1.3(1)):
//!
//! - `Requires=` — hard dependency; failure of the required unit fails the
//!   requirer.
//! - `Wants=` — weak dependency; a missing or failed wanted unit degrades to a
//!   warning.
//! - `After=` / `Before=` — ordering only, no liveness implication.
//! - `Conflicts=` — mutual exclusion.
//!
//! Required behaviours:
//!
//! - Topological sort (Kahn or DFS) with **cycle detection that names the
//!   cycle path** (`a -> b -> c -> a`), because a cycle in a unit directory is
//!   otherwise very hard to spot.
//! - Deterministic ordering: ties are broken with [`super::UnitName`]'s
//!   `Ord` so two boots produce the same startup sequence.
//! - Reference integrity checked at build time, before anything starts: a
//!   `Requires=`/`After=` pointing at a unit with no file is a
//!   `MissingDependency` error; the same case in `Wants=` downgrades to a
//!   warning.
