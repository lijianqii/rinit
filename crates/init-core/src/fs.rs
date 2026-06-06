//! Filesystem operations required by PID 1 during early bootstrap.

use anyhow::{Context, Result};
use nix::mount::{mount, MsFlags};
use std::path::Path;
use tracing::info;

/// Mount the essential virtual filesystems.
///
/// Called only once during early init, before any services start.
pub fn mount_virtual_fs() -> Result<()> {
    mount_fs(Some("proc"), "/proc", "proc", MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC, None)?;
    mount_fs(Some("sysfs"), "/sys", "sysfs", MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC, None)?;
    mount_fs(Some("devtmpfs"), "/dev", "devtmpfs", MsFlags::MS_NOSUID, Some("mode=0755"))?;
    mount_fs(Some("tmpfs"), "/run", "tmpfs", MsFlags::MS_NOSUID | MsFlags::MS_NODEV, Some("mode=0755,size=10%"))?;

    std::fs::create_dir_all("/dev/pts").ok();
    mount_fs(Some("devpts"), "/dev/pts", "devpts", MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC, Some("gid=5,mode=0620"))?;

    info!("virtual filesystems mounted");
    Ok(())
}

/// Generic mount helper.
pub fn mount_fs(
    source: Option<&str>,
    target: &str,
    fstype: &str,
    flags: MsFlags,
    data: Option<&str>,
) -> Result<()> {
    let target_path = Path::new(target);
    if !target_path.exists() {
        std::fs::create_dir_all(target_path)
            .with_context(|| format!("failed to create mount point: {}", target))?;
    }

    mount(source, target, Some(fstype), flags, data)
        .with_context(|| format!("failed to mount {} ({})", target, fstype))
}

/// Set the system hostname.
pub fn set_hostname(name: &str) -> Result<()> {
    nix::unistd::sethostname(name)
        .context("failed to set hostname")?;
    info!(hostname = %name, "hostname set");
    Ok(())
}

/// Create essential directories under /run/rinit.
pub fn create_run_dirs() -> Result<()> {
    let dirs = [
        "/run/rinit",
        "/run/rinit/lock",
        "/run/rinit/units",
    ];
    for dir in &dirs {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir))?;
    }
    Ok(())
}
