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

/// Create a device node via mknod(2).
///
/// Used by the uevent handler to create /dev entries when the kernel
/// reports a new device (ACTION=add with MAJOR/MINOR).
pub fn create_device_node(name: &str, devtype: char, major: u32, minor: u32) -> Result<()> {
    use anyhow::Context;
    let path = format!("/dev/{}", name);

    let mode = match devtype {
        'c' => libc::S_IFCHR,
        'b' => libc::S_IFBLK,
        _ => libc::S_IFCHR,
    } | 0o600;

    let dev = libc::makedev(major, minor);
    let ret = unsafe {
        libc::mknod(
            path.as_ptr() as *const libc::c_char,
            mode,
            dev,
        )
    };
    if ret < 0 {
        let err = std::io::Error::last_os_error();
        // EEXIST is fine — the node may have been created by devtmpfs
        if err.raw_os_error() != Some(libc::EEXIST) {
            return Err(err).with_context(|| format!("mknod({}) failed", path));
        }
    }

    tracing::debug!(path = %path, devtype = %devtype, major, minor, "device node created");
    Ok(())
}

// ---- /etc/fstab parsing and mounting ----

/// A single mount entry from /etc/fstab.
#[derive(Debug, Clone)]
pub struct FstabEntry {
    pub device: String,
    pub mountpoint: String,
    pub fstype: String,
    pub options: String,
    pub dump: i32,
    pub pass: i32,
}

/// Parse /etc/fstab and return all mountable entries.
pub fn parse_fstab() -> Vec<FstabEntry> {
    let content = match std::fs::read_to_string("/etc/fstab") {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let virtual_fs: &[&str] = &["proc", "sysfs", "devtmpfs", "tmpfs", "devpts"];
    let mut entries = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("#") {
            continue;
        }
        let fields: Vec<&str> = trimmed.split_whitespace().collect();
        if fields.len() < 4 {
            continue;
        }
        let fstype = fields[2];
        if virtual_fs.contains(&fstype) {
            continue;
        }
        entries.push(FstabEntry {
            device: fields[0].to_string(),
            mountpoint: fields[1].to_string(),
            fstype: fstype.to_string(),
            options: fields[3].to_string(),
            dump: fields.get(4).and_then(|s| s.parse().ok()).unwrap_or(0),
            pass: fields.get(5).and_then(|s| s.parse().ok()).unwrap_or(0),
        });
    }
    entries
}

/// Mount all entries from /etc/fstab.
pub fn mount_fstab() -> Result<()> {
    let entries = parse_fstab();
    if entries.is_empty() {
        tracing::debug!("no fstab entries to mount");
        return Ok(());
    }
    tracing::info!(count = entries.len(), "mounting filesystems from /etc/fstab");
    for entry in &entries {
        let flags = parse_mount_options(&entry.options);
        let source = if entry.device == "none" { None } else { Some(entry.device.as_str()) };
        match mount_fs(source, &entry.mountpoint, &entry.fstype, flags, None) {
            Ok(()) => tracing::info!(target = %entry.mountpoint, fstype = %entry.fstype, "mounted"),
            Err(e) => tracing::warn!(target = %entry.mountpoint, error = %e, "failed to mount fstab entry"),
        }
    }
    Ok(())
}

fn parse_mount_options(options: &str) -> MsFlags {
    let mut flags = MsFlags::empty();
    for opt in options.split(",") {
        match opt.trim() {
            "defaults" => flags |= MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC | MsFlags::MS_RELATIME,
            "ro" => flags |= MsFlags::MS_RDONLY,
            "noexec" => flags |= MsFlags::MS_NOEXEC,
            "nosuid" => flags |= MsFlags::MS_NOSUID,
            "nodev" => flags |= MsFlags::MS_NODEV,
            "noatime" => flags |= MsFlags::MS_NOATIME,
            "relatime" => flags |= MsFlags::MS_RELATIME,
            "sync" => flags |= MsFlags::MS_SYNCHRONOUS,
            "dirsync" => flags |= MsFlags::MS_DIRSYNC,
            "mand" => flags |= MsFlags::MS_MANDLOCK,
            _ => {}
        }
    }
    flags
}

