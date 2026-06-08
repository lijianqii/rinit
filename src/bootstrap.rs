//! Early bootstrap routines executed before entering the event loop.
//!
//! These are the first things PID 1 does after logging is set up:
//!   1. Mount virtual filesystems (/proc, /sys, /dev, /run)
//!   2. Set hostname
//!   3. Create runtime directories
//!   4. Set up default signal dispositions

use anyhow::{Context, Result};
use tracing::info;

/// Run all early initialisation steps.
pub fn early_init() -> Result<()> {
    info!("early bootstrap: mounting virtual filesystems");
    init_core::fs::mount_virtual_fs().context("mount_virtual_fs")?;

    info!("early bootstrap: creating runtime directories");
    init_core::fs::create_run_dirs().context("create_run_dirs")?;

    info!("early bootstrap: setting hostname");
    init_core::fs::set_hostname("localhost").context("set_hostname")?;

    init_core::cgroup::ensure_cgroup_root().ok();

    info!("early bootstrap: blocking PID 1 signals");
    init_core::signal::block_default_signals()?;

    info!("early bootstrap: setting PR_SET_CHILD_SUBREAPER");
    // PID 1 must be a subreaper so orphan grandchildren are
    // reaped by init instead of becoming zombies.
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) } != 0 {
        anyhow::bail!("prctl(PR_SET_CHILD_SUBREAPER) failed: {}", std::io::Error::last_os_error());
    }

    info!("early bootstrap complete");
    Ok(())
}
