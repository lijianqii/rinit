//! init-core: Safe syscall wrappers and kernel interface abstractions.
//!
//! This crate provides the PID 1 with:
//! - Process spawning (fork + execve)
//! - Child reaping (waitpid in non-blocking mode)
//! - Signal handling (signalfd integration)
//! - Filesystem mount operations
//! - cgroups v2 management
//! - Capability management

pub mod cgroup;
pub mod child;
pub mod fs;
pub mod signal;
pub mod uevent;

use anyhow::Result;

/// Represents a spawned child process.
#[derive(Debug, Clone)]
pub struct ChildInfo {
    pub pid: libc::pid_t,
    pub unit_name: String,
}

/// Exit status of a reaped child.
#[derive(Debug, Clone)]
pub struct ChildExit {
    pub pid: libc::pid_t,
    pub status: i32,
    /// True if the child was killed by a signal (as opposed to normal exit).
    pub was_signaled: bool,
}

/// File descriptor for signalfd.
///
/// This is a lightweight wrapper around a raw fd. The fd is NOT closed
/// when `SignalFd` is dropped — ownership of the fd is transferred to
/// the caller (typically `Runtime` in init-event), which is responsible
/// for closing it.
pub struct SignalFd {
    pub fd: std::os::unix::io::RawFd,
}

/// Capabilities configuration for spawned services.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    pub bounding: Vec<String>,
    pub ambient: Vec<String>,
}

/// Operations that require kernel interaction.
pub trait KernelOps: Send + Sync {
    /// Fork + exec a new service process.
    fn spawn_service(
        &self,
        path: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<ChildInfo>;

    /// Reap all exited children (WNOHANG).
    fn reap_children(&self) -> Result<Vec<ChildExit>>;

    /// Create a signalfd for the given signal set.
    fn create_signal_fd(&self, signals: &[i32]) -> Result<SignalFd>;

    /// Send a signal to a process.
    fn kill(&self, pid: libc::pid_t, sig: i32) -> Result<()>;

    /// Mount a filesystem.
    fn mount(
        &self,
        source: Option<&str>,
        target: &str,
        fstype: &str,
        flags: nix::mount::MsFlags,
        data: Option<&str>,
    ) -> Result<()>;

    /// Create and enter a cgroup v2 hierarchy for a service.
    fn create_service_cgroup(&self, name: &str) -> Result<cgroup::CgroupHandle>;

    /// Set process capabilities before exec.
    fn set_capabilities(&self, caps: &Capabilities) -> Result<()>;

    /// Move current process into a new session (setsid).
    fn create_session(&self) -> Result<libc::pid_t>;
}
