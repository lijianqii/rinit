//! Kernel uevent (device hotplug) listener via netlink.
//!
//! Listens on a NETLINK_KOBJECT_UEVENT socket for kernel device
//! events (add/remove/change). A minimal udev replacement for the
//! initramfs — creates /dev nodes and sets permissions.
//!
//! Message format (null-separated key=value pairs):
//!   ACTION=add\0DEVPATH=/devices/...\0SUBSYSTEM=tty\0
//!   DEVNAME=ttyAMA0\0MAJOR=204\0MINOR=64\0SEQNUM=42\0\0

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::os::unix::io::RawFd;
use tracing::debug;

/// Parsed kernel uevent.
#[derive(Debug, Clone, Default)]
pub struct Uevent {
    pub action: String,
    pub devpath: String,
    pub subsystem: String,
    pub devname: Option<String>,
    pub devtype: Option<String>,
    pub major: Option<u32>,
    pub minor: Option<u32>,
    pub seqnum: u64,
}

/// A bound netlink socket for receiving kernel uevents.
///
/// The fd is CLOEXEC and NONBLOCK so it can be wrapped in tokio AsyncFd.
pub struct UeventSocket {
    fd: RawFd,
}

impl UeventSocket {
    /// Create and bind a new NETLINK_KOBJECT_UEVENT socket.
    pub fn new() -> Result<Self> {
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                libc::NETLINK_KOBJECT_UEVENT,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .context("socket(AF_NETLINK, NETLINK_KOBJECT_UEVENT) failed");
        }

        // We want uevents from all subsystems.
        let groups = 1u32;

        let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        sa.nl_family = libc::AF_NETLINK as u16;
        sa.nl_pid = unsafe { libc::getpid() } as u32;
        sa.nl_groups = groups;

        let ret = unsafe {
            libc::bind(
                fd,
                &sa as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_nl>() as u32,
            )
        };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(err).context("bind(netlink) failed");
        }

        debug!(fd, "uevent netlink socket created");
        Ok(UeventSocket { fd })
    }

    /// Wrap an existing raw fd (used by init-event to borrow the fd from AsyncFd).
    pub fn from_raw_fd(fd: RawFd) -> Self {
        UeventSocket { fd }
    }

    /// The raw fd for integration with tokio AsyncFd.
    pub fn raw_fd(&self) -> RawFd {
        self.fd
    }

    /// Try to receive and parse a single uevent message.
    /// Returns None if no data is available (EAGAIN).
    pub fn recv(&self) -> Result<Option<Uevent>> {
        let mut buf = [0u8; 4096];

        let n = unsafe {
            libc::recv(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0)
        };

        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EAGAIN) {
                return Ok(None);
            }
            return Err(err).context("recv(netlink) failed");
        }

        if n == 0 {
            return Ok(None);
        }

        let data = &buf[..n as usize];
        let uevent = parse_uevent(data)?;

        debug!(
            action = %uevent.action,
            devpath = %uevent.devpath,
            devname = ?uevent.devname,
            "uevent received"
        );

        Ok(Some(uevent))
    }
}

impl Drop for UeventSocket {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
        debug!(fd = self.fd, "uevent socket closed");
    }
}

/// Parse a raw uevent buffer (null-separated key=value pairs ending with \0\0).
fn parse_uevent(buf: &[u8]) -> Result<Uevent> {
    let text = std::str::from_utf8(buf)
        .context("uevent: invalid UTF-8")?
        .trim_end_matches('\0');

    let mut kv = HashMap::new();
    for pair in text.split('\0') {
        if let Some((key, value)) = pair.split_once('=') {
            kv.insert(key.to_string(), value.to_string());
        }
    }

    let major = kv.get("MAJOR").and_then(|v| v.parse().ok());
    let minor = kv.get("MINOR").and_then(|v| v.parse().ok());
    let seqnum = kv.get("SEQNUM").and_then(|v| v.parse().ok()).unwrap_or(0);

    Ok(Uevent {
        action: kv.remove("ACTION").unwrap_or_default(),
        devpath: kv.remove("DEVPATH").unwrap_or_default(),
        subsystem: kv.remove("SUBSYSTEM").unwrap_or_default(),
        devname: kv.remove("DEVNAME"),
        devtype: kv.remove("DEVTYPE"),
        major,
        minor,
        seqnum,
    })
}
