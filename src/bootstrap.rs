//! Early bootstrap routines executed before entering the event loop.
//!
//! These are the first things PID 1 does after logging is set up:
//!   1. Mount virtual filesystems (/proc, /sys, /dev, /run)
//!   2. Set hostname
//!   3. Create runtime directories
//!   4. Set up default signal dispositions

use anyhow::{Context, Result};
use tracing::{debug, info};

/// Run all early initialisation steps.
pub fn early_init() -> Result<()> {
    debug!("early bootstrap: mounting virtual filesystems");
    init_core::fs::mount_virtual_fs().context("mount_virtual_fs")?;

    debug!("early bootstrap: opening /dev/console");
    claim_console();

    debug!("early bootstrap: mounting /etc/fstab entries");
    init_core::fs::mount_fstab().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "fstab mount failed, continuing");
    });

    debug!("early bootstrap: creating runtime directories");
    init_core::fs::create_run_dirs().context("create_run_dirs")?;

    debug!("early bootstrap: setting hostname");
    let hostname = std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "localhost".to_string());
    init_core::fs::set_hostname(&hostname).context("set_hostname")?;

    init_core::cgroup::ensure_cgroup_root().ok();

    debug!("early bootstrap: blocking PID 1 signals");
    init_core::signal::block_default_signals()?;

    debug!("early bootstrap: setting PR_SET_CHILD_SUBREAPER");
    // PID 1 must be a subreaper so orphan grandchildren are
    // reaped by init instead of becoming zombies.
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) } != 0 {
        anyhow::bail!(
            "prctl(PR_SET_CHILD_SUBREAPER) failed: {}",
            std::io::Error::last_os_error()
        );
    }

    info!("early bootstrap complete");
    Ok(())
}

/// Open /dev/console for stdin/stdout/stderr.
/// Called after devtmpfs mount so /dev/console exists.
fn claim_console() {
    let fd = unsafe {
        libc::open(
            b"/dev/console\0".as_ptr() as *const libc::c_char,
            libc::O_RDWR | libc::O_NOCTTY,
        )
    };
    if fd < 0 {
        return;
    }
    for target in 0..=2 {
        if fd != target {
            unsafe { libc::dup2(fd, target) };
        }
    }
    if fd > 2 {
        unsafe { libc::close(fd) };
    }
}
