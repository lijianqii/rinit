//! cgroups v2 management.
//!
//! On Linux, cgroups v2 is mounted at /sys/fs/cgroup by default.
//! Each service gets its own sub-directory under the init.slice hierarchy.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// Handle to a service-specific cgroup.
#[derive(Debug)]
pub struct CgroupHandle {
    pub name: String,
    pub path: PathBuf,
}

impl CgroupHandle {
    /// Build the cgroup directory path for a service.
    pub fn new(service_name: &str) -> Self {
        let name = format!("init-{}.slice", service_name);
        let path = PathBuf::from("/sys/fs/cgroup").join(&name);
        CgroupHandle { name, path }
    }

    /// Create the cgroup directory (mkdir).
    pub fn create(&self) -> Result<()> {
        std::fs::create_dir_all(&self.path)
            .with_context(|| format!("failed to create cgroup: {}", self.path.display()))?;
        Ok(())
    }

    /// Write a value to a cgroup controller file.
    pub fn write_control(&self, controller: &str, value: &str) -> Result<()> {
        let ctl_path = self.path.join(controller);
        std::fs::write(&ctl_path, value).with_context(|| {
            format!(
                "failed to write {} = {} to {}",
                controller,
                value,
                ctl_path.display()
            )
        })
    }

    /// Add a process to this cgroup by writing its PID to cgroup.procs.
    pub fn attach_process(&self, pid: libc::pid_t) -> Result<()> {
        let procs_path = self.path.join("cgroup.procs");
        std::fs::write(&procs_path, pid.to_string())
            .with_context(|| format!("failed to attach pid {} to cgroup", pid))
    }

    /// Set memory limit (e.g., "512M").
    pub fn set_memory_max(&self, limit: &str) -> Result<()> {
        self.write_control("memory.max", limit)
    }

    /// Set CPU weight (1..10000, default 100).
    pub fn set_cpu_weight(&self, weight: u32) -> Result<()> {
        self.write_control("cpu.weight", &weight.to_string())
    }

    /// Set max number of tasks/threads.
    pub fn set_pids_max(&self, max: u32) -> Result<()> {
        self.write_control("pids.max", &max.to_string())
    }

    /// Remove the cgroup when the service stops.
    pub fn remove(&self) -> Result<()> {
        if self.path.exists() {
            std::fs::remove_dir(&self.path)
                .with_context(|| format!("failed to remove cgroup: {}", self.path.display()))?;
        }
        Ok(())
    }
}

/// Ensure cgroups v2 is mounted and writable.
pub fn ensure_cgroup_root() -> Result<()> {
    let root = PathBuf::from("/sys/fs/cgroup");
    if !root.exists() {
        std::fs::create_dir_all(&root)?;
    }
    Ok(())
}
