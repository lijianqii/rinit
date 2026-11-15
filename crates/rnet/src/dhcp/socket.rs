//! DHCPv4 UDP transport.
//!
//! One socket per DHCP interface, bound to `0.0.0.0:68` and restricted with
//! `SO_BINDTODEVICE`. The socket is non-blocking; `rnetd` includes its
//! descriptor in the main `poll(2)` set.

use std::ffi::CString;
use std::io;
use std::mem::size_of;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::dhcp::packet::{DhcpError, Packet};

const SO_REUSEADDR: libc::c_int = 2;
const SO_BROADCAST: libc::c_int = 6;
const SO_BINDTODEVICE: libc::c_int = 25;
const CLIENT_PORT: u16 = 68;
const SERVER_PORT: u16 = 67;

/// A per-interface DHCPv4 socket.
pub struct DhcpSocket {
    fd: OwnedFd,
    iface: String,
}

impl DhcpSocket {
    /// Bind port 68 on `iface`.
    pub fn open(iface: &str) -> io::Result<Self> {
        // SAFETY: socket takes scalar arguments and returns a fresh descriptor.
        let fd = unsafe {
            libc::socket(
                libc::AF_INET,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                libc::IPPROTO_UDP,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is fresh and owned from here on.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        set_int_option(fd.as_raw_fd(), SO_REUSEADDR, 1)?;
        set_int_option(fd.as_raw_fd(), SO_BROADCAST, 1)?;
        let device = CString::new(iface).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "interface name contains NUL")
        })?;
        // SAFETY: device is NUL-terminated and the option copies it.
        let rc = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                SO_BINDTODEVICE,
                device.as_ptr().cast(),
                (device.as_bytes().len() + 1) as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        let address = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: CLIENT_PORT.to_be(),
            sin_addr: libc::in_addr {
                s_addr: u32::from(Ipv4Addr::UNSPECIFIED).to_be(),
            },
            sin_zero: [0; 8],
        };
        // SAFETY: address is a valid sockaddr_in of the right length.
        let rc = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&address as *const libc::sockaddr_in).cast(),
                size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(DhcpSocket {
            fd,
            iface: iface.to_string(),
        })
    }

    /// The interface this socket is bound to.
    pub fn iface(&self) -> &str {
        &self.iface
    }

    /// The descriptor for `poll(2)`.
    pub fn as_raw_fd(&self) -> libc::c_int {
        self.fd.as_raw_fd()
    }

    /// Send a packet to `destination`.
    pub fn send(&self, packet: &Packet, destination: Ipv4Addr) -> io::Result<()> {
        let bytes = packet.encode();
        let address = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: SERVER_PORT.to_be(),
            sin_addr: libc::in_addr {
                s_addr: u32::from(destination).to_be(),
            },
            sin_zero: [0; 8],
        };
        // SAFETY: bytes and address are valid for the duration of the call.
        let sent = unsafe {
            libc::sendto(
                self.fd.as_raw_fd(),
                bytes.as_ptr().cast(),
                bytes.len(),
                0,
                (&address as *const libc::sockaddr_in).cast(),
                size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Receive one packet, if one is ready.
    pub fn recv(&self, buffer: &mut [u8]) -> io::Result<Option<(Packet, Ipv4Addr)>> {
        let mut address = libc::sockaddr_in {
            sin_family: 0,
            sin_port: 0,
            sin_addr: libc::in_addr { s_addr: 0 },
            sin_zero: [0; 8],
        };
        let mut length = size_of::<libc::sockaddr_in>() as libc::socklen_t;
        // SAFETY: buffer and address are valid writable storage.
        let received = unsafe {
            libc::recvfrom(
                self.fd.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0,
                (&mut address as *mut libc::sockaddr_in).cast(),
                &mut length,
            )
        };
        if received < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error);
        }
        let bytes = &buffer[..received as usize];
        let packet = Packet::decode(bytes).map_err(|error: DhcpError| {
            io::Error::new(io::ErrorKind::InvalidData, error.to_string())
        })?;
        let source = Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr));
        Ok(Some((packet, source)))
    }
}

fn set_int_option(fd: libc::c_int, option: libc::c_int, value: libc::c_int) -> io::Result<()> {
    // SAFETY: value is a valid int for the socket option.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            (&value as *const libc::c_int).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Read an interface MAC from sysfs.
pub fn read_mac(iface: &str) -> io::Result<[u8; 6]> {
    let text = std::fs::read_to_string(format!("/sys/class/net/{iface}/address"))?;
    parse_mac(text.trim())
}

fn parse_mac(text: &str) -> io::Result<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = text.split(':');
    for slot in &mut mac {
        let part = parts
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "MAC has too few octets"))?;
        *slot = u8::from_str_radix(part, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "MAC has a bad octet"))?;
    }
    if parts.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "MAC has too many octets",
        ));
    }
    Ok(mac)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_parser_accepts_the_sysfs_form() {
        assert_eq!(
            parse_mac("02:00:00:00:00:01").expect("mac"),
            [2, 0, 0, 0, 0, 1]
        );
        assert!(parse_mac("02:00:00:00:00").is_err());
        assert!(parse_mac("02:00:00:00:00:01:02").is_err());
        assert!(parse_mac("zz:00:00:00:00:01").is_err());
    }
}
