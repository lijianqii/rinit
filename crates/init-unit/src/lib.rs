//! init-unit: Unit file types, parsing, and dependency resolution.
//!
//! Units are the fundamental configuration objects in rinit:
//!   .service  — a daemon process to supervise
//!   .socket   — socket activation (listener fd to service)
//!   .timer    — time-based activation
//!   .mount    — filesystem mount points
//!   .target   — grouping / synchronization point (like systemd target)
//!
//! File search path (in order, later overrides earlier):
//!   1. /usr/lib/rinit/units/      (system defaults, installed by packages)
//!   2. /etc/rinit/units/          (admin overrides)

pub mod deps;
pub mod parse;
pub mod types;

use std::collections::HashMap;
use tracing::debug;

/// Registry holding all loaded units, keyed by unit name.
pub type UnitRegistry = HashMap<String, types::Unit>;

/// Load all unit files from config directories.
pub fn load_all_units() -> anyhow::Result<UnitRegistry> {
    let mut registry = UnitRegistry::new();

    for dir in &["/usr/lib/rinit/units", "/etc/rinit/units"] {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries {
                let entry = entry?;
                let path = entry.path();

                if path.extension().map_or(true, |ext| ext != "toml") {
                    continue;
                }

                let content = std::fs::read_to_string(&path)?;
                match parse::parse_unit_file(&content) {
                    Ok(unit) => {
                        debug!(
                            name = %unit.name,
                            path = %path.display(),
                            "loaded unit"
                        );
                        registry.insert(unit.name.clone(), unit);
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "failed to parse unit file, skipping"
                        );
                    }
                }
            }
        }
    }

    Ok(registry)
}
