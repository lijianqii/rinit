//! Link-event subscription for hotplug.
//!
//! The daemon only needs a wake-up: any `RTNLGRP_LINK` message means "re-read
//! the link set and reconcile". The event body is intentionally not parsed
//! here; `/sys/class/net` is the source of truth for the link set.

use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const NETLINK_ROUTE: libc::c_int = 0;
const RTNLGRP_LINK: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct SockaddrNl {
    nl_family: libc::c_ushort,
    nl_pad: libc::c_ushort,
    nl_pid: u32,
    nl_groups: u32,
}

/// A non-blocking `NETLINK_ROUTE` socket subscribed to link events.
pub struct LinkEvents {
    fd: OwnedFd,
}

impl LinkEvents {
    /// Open the subscription.
    pub fn open() -> io::Result<Self> {
        // SAFETY: socket takes scalar arguments and returns a fresh descriptor.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                NETLINK_ROUTE,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is fresh and owned from here on.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let address = SockaddrNl {
            nl_family: libc::AF_NETLINK as libc::c_ushort,
            nl_pad: 0,
            nl_pid: 0,
            nl_groups: RTNLGRP_LINK,
        };
        // SAFETY: address is a valid sockaddr_nl of the right size.
        let rc = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&address as *const SockaddrNl).cast(),
                size_of::<SockaddrNl>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(LinkEvents { fd })
    }

    /// The descriptor for `poll(2)`.
    pub fn as_raw_fd(&self) -> libc::c_int {
        self.fd.as_raw_fd()
    }

    /// Drain pending link events without parsing them.
    pub fn drain(&self) -> io::Result<()> {
        let mut buffer = [0u8; 8192];
        loop {
            // SAFETY: buffer is a valid writable slice.
            let received = unsafe {
                libc::recv(
                    self.fd.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    0,
                )
            };
            if received < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Ok(());
                }
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if received == 0 {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_event_socket_opens() {
        let events = LinkEvents::open().expect("link event socket");
        assert!(events.as_raw_fd() >= 0);
        events.drain().expect("drain");
    }
}
