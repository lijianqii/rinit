//! init-core — Safe Linux kernel interface abstractions for rinit.
//!
//! Modules:
//! - child:   fork + execve, child reaping, fd cleanup
//! - cgroup:  cgroups v2 unified hierarchy management
//! - fs:      mount(2), mknod(2), hostname, runtime dirs
//! - signal:  signalfd creation / blocking / reading
//! - uevent:  netlink kernel device hotplug listener
//! - net:     ioctl-based static IP + native DHCP client
//! - tty:     terminal session (setsid, TIOCSCTTY, baud rate)
//! - login:   username/password prompt with /etc/passwd validation

pub mod cgroup;
pub mod child;
pub mod fs;
pub mod signal;
pub mod uevent;
pub mod tty;
pub mod login;
pub mod net;

/// Spawned child process tracking info.
#[derive(Debug, Clone)]
pub struct ChildInfo {
    pub pid: libc::pid_t,
}

/// Exit status of a reaped child.
#[derive(Debug, Clone)]
pub struct ChildExit {
    pub pid: libc::pid_t,
    pub status: i32,
    pub was_signaled: bool,
}

/// Lightweight signalfd wrapper (fd managed by caller).
pub struct SignalFd {
    pub fd: std::os::unix::io::RawFd,
}
