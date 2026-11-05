//! Unit manager: turns a unit directory into a running process set.
//!
//! The plan itself lives in `rinit-runtime` so `rinit-check` validates exactly
//! the graph PID 1 boots. This module keeps the boot-time reporting that is
//! specific to the daemon's log.

use std::path::Path;

use rinit_runtime::RuntimeError;

pub use rinit_runtime::plan::{BootPlan, commands_in_level, config_of, describe_plan, plan_boot};

use crate::log::rlog;

/// Load and log a plan, reporting the level structure on the console.
///
/// Reading every unit file is blocking filesystem I/O, so the load itself runs
/// on the blocking pool and only the logging stays on the async thread.
///
/// # Errors
/// As [`plan_boot`].
pub async fn announce_plan(
    unit_dir: &Path,
    default_target: &str,
) -> Result<BootPlan, RuntimeError> {
    let unit_dir = unit_dir.to_path_buf();
    let default_target = default_target.to_string();
    let plan = tokio::task::spawn_blocking(move || plan_boot(&unit_dir, &default_target))
        .await
        .map_err(|error| RuntimeError::Join {
            op: "unit plan",
            detail: error.to_string(),
        })??;
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
