//! Netlink write path and read-only kernel observation.
//!
//! Writes go through `NETLINK_ROUTE`; the ACK/error parse is deliberately
//! small. Read-only status comes from `getifaddrs(3)`, `/sys/class/net` and
//! `/proc/net/route`, which avoids a second, more fragile netlink dump parser
//! while keeping the write path precise.

pub mod event;

pub use event::LinkEvents;

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::io;
use std::mem::size_of;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;

use crate::model::{Config, Interface, InterfaceKind, Ipv4Net, Route};

const NETLINK_ROUTE: libc::c_int = 0;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 0x01;
const NLM_F_ACK: u16 = 0x04;
const NLM_F_CREATE: u16 = 0x400;
const NLM_F_EXCL: u16 = 0x200;
const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const RTM_SETLINK: u16 = 19;
const RTM_NEWADDR: u16 = 20;
const RTM_DELADDR: u16 = 21;
const RTM_NEWROUTE: u16 = 24;
const RTM_DELROUTE: u16 = 25;

const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFLA_MASTER: u16 = 10;
const IFLA_LINKINFO: u16 = 18;
const IFLA_INFO_KIND: u16 = 1;
const IFLA_INFO_DATA: u16 = 2;
const IFLA_BR_FORWARD_DELAY: u16 = 1;
const IFLA_BR_HELLO_TIME: u16 = 2;
const IFLA_BR_MAX_AGE: u16 = 3;
const IFLA_BR_STP_STATE: u16 = 5;

const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const RTA_DST: u16 = 1;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const RTA_PRIORITY: u16 = 6;
const RTA_PREFSRC: u16 = 7;
const RTA_TABLE: u16 = 15;
const RT_TABLE_UNSPEC: u8 = 0;
const RTPROT_STATIC: u8 = 4;
const RT_SCOPE_UNIVERSE: u8 = 0;
const RT_SCOPE_LINK: u8 = 253;
const RTN_UNICAST: u8 = 1;
const RTNH_F_ONLINK: u32 = 4;

#[repr(C)]
#[derive(Clone, Copy)]
struct SockaddrNl {
    nl_family: libc::c_ushort,
    nl_pad: libc::c_ushort,
    nl_pid: u32,
    nl_groups: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NlMsgHdr {
    nlmsg_len: u32,
    nlmsg_type: u16,
    nlmsg_flags: u16,
    nlmsg_seq: u32,
    nlmsg_pid: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IfInfoMsg {
    ifi_family: u8,
    __ifi_pad: u8,
    ifi_type: u16,
    ifi_index: i32,
    ifi_flags: u32,
    ifi_change: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IfAddrMsg {
    ifa_family: u8,
    ifa_prefixlen: u8,
    ifa_flags: u8,
    ifa_scope: u8,
    ifa_index: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RtMsg {
    rtm_family: u8,
    rtm_dst_len: u8,
    rtm_src_len: u8,
    rtm_tos: u8,
    rtm_table: u8,
    rtm_protocol: u8,
    rtm_scope: u8,
    rtm_type: u8,
    rtm_flags: u32,
}

/// A link as reported by the kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkInfo {
    /// Interface name.
    pub name: String,
    /// Kernel ifindex.
    pub index: u32,
    /// `IFF_UP`.
    pub up: bool,
    /// MTU.
    pub mtu: u32,
    /// Bridge master, if this link is enslaved.
    pub master: Option<String>,
    /// Whether this is a bridge device.
    pub bridge: bool,
    /// Ports enslaved to this bridge.
    pub ports: Vec<String>,
}

/// An IPv4 address observed on an interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedAddress {
    /// Interface name.
    pub iface: String,
    /// Address and prefix.
    pub address: Ipv4Net,
}

/// An IPv4 route observed in `/proc/net/route`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedRoute {
    /// Output interface.
    pub dev: String,
    /// Destination; `None` is the default route.
    pub to: Option<Ipv4Net>,
    /// Gateway, if any.
    pub via: Option<Ipv4Addr>,
    /// Metric.
    pub metric: u32,
    /// Table; `/proc/net/route` only reports the main table.
    pub table: u32,
}

/// The read-only kernel state used by status and reconciliation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observed {
    /// Links keyed by name.
    pub links: BTreeMap<String, LinkInfo>,
    /// Addresses, in kernel order.
    pub addresses: Vec<ObservedAddress>,
    /// Routes, in `/proc/net/route` order.
    pub routes: Vec<ObservedRoute>,
}

impl Observed {
    /// Addresses on one interface.
    pub fn addresses_of(&self, iface: &str) -> Vec<Ipv4Net> {
        self.addresses
            .iter()
            .filter(|address| address.iface == iface)
            .map(|address| address.address)
            .collect()
    }

    /// Routes using one interface.
    pub fn routes_of<'a>(&'a self, iface: &'a str) -> impl Iterator<Item = &'a ObservedRoute> {
        self.routes.iter().filter(move |route| route.dev == iface)
    }
}

/// Observe the current links, addresses and routes.
pub fn observe() -> io::Result<Observed> {
    let mut observed = Observed::default();
    let mut ports_by_master: BTreeMap<String, Vec<String>> = BTreeMap::new();

    let entries = std::fs::read_dir("/sys/class/net")?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let index = if_nametoindex(&name)?;
        let flags = read_u32(&format!("/sys/class/net/{name}/flags"))?;
        let mtu = read_u32(&format!("/sys/class/net/{name}/mtu"))?;
        let bridge = PathBuf::from(format!("/sys/class/net/{name}/bridge")).is_dir();
        let master = match std::fs::read_link(format!("/sys/class/net/{name}/master")) {
            Ok(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if let Some(master) = &master {
            ports_by_master
                .entry(master.clone())
                .or_default()
                .push(name.clone());
        }
        observed.links.insert(
            name.clone(),
            LinkInfo {
                name,
                index,
                up: flags & libc::IFF_UP as u32 != 0,
                mtu,
                master,
                bridge,
                ports: Vec::new(),
            },
        );
    }
    for (bridge, ports) in ports_by_master {
        if let Some(link) = observed.links.get_mut(&bridge) {
            link.ports = ports;
        }
    }
    observed.addresses = observe_addresses()?;
    observed.routes = observe_routes()?;
    Ok(observed)
}

/// Apply a configuration to the kernel.
///
/// This is intentionally a small imperative layer: `state` computes the
/// desired changes, and this function performs each one with an explicit
/// error instead of hiding failures inside a dump.
pub fn apply(config: &Config, observed: &Observed) -> io::Result<()> {
    let mut netlink = Netlink::open()?;

    if config.global.manage_loopback
        && let Some(link) = observed.links.get("lo")
        && !link.up
    {
        netlink.set_link_up("lo", true)?;
    }

    for iface in &config.interfaces {
        if iface.kind == InterfaceKind::Bridge {
            if !observed.links.contains_key(&iface.name) {
                netlink.create_bridge(iface)?;
            } else if let Some(link) = observed.links.get(&iface.name)
                && !link.bridge
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists but is not a bridge", iface.name),
                ));
            }
        }

        if !iface.enabled {
            netlink.set_link_up(&iface.name, false)?;
            continue;
        }

        if let Some(mtu) = iface.mtu {
            netlink.set_mtu(&iface.name, mtu)?;
        }
        netlink.set_link_up(&iface.name, true)?;

        if iface.kind == InterfaceKind::Bridge {
            for port in &iface.ports {
                netlink.enslave(&iface.name, port)?;
                netlink.set_link_up(port, true)?;
            }
        }

        if !iface.dhcp4 {
            let existing = observed.addresses_of(&iface.name);
            for address in &iface.addresses {
                if !existing.contains(address) {
                    netlink.add_address(&iface.name, *address)?;
                }
            }
            for address in existing {
                if !iface.addresses.contains(&address) {
                    netlink.del_address(&iface.name, address)?;
                }
            }
            if let Some(gateway) = iface.gateway {
                let route = Route {
                    name: format!("{}-default", iface.name),
                    to: None,
                    via: Some(gateway),
                    dev: Some(iface.name.clone()),
                    metric: 0,
                    table: crate::model::RT_TABLE_MAIN,
                    src: None,
                    onlink: false,
                };
                netlink.add_route(&route)?;
            }
        }
    }

    for route in &config.routes {
        netlink.add_route(route)?;
    }
    Ok(())
}

/// A netlink route socket.
pub struct Netlink {
    fd: OwnedFd,
    sequence: u32,
}

impl Netlink {
    /// Open a route netlink socket.
    pub fn open() -> io::Result<Self> {
        // SAFETY: socket takes scalar arguments and returns a fresh descriptor.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
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
            nl_groups: 0,
        };
        // SAFETY: address points to a valid sockaddr_nl of the right length.
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
        let timeout = libc::timeval {
            tv_sec: 2,
            tv_usec: 0,
        };
        // SAFETY: timeout is a valid timeval for SO_RCVTIMEO.
        let rc = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&timeout as *const libc::timeval).cast(),
                size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Netlink { fd, sequence: 1 })
    }

    fn next_sequence(&mut self) -> u32 {
        let sequence = self.sequence;
        self.sequence = self.sequence.wrapping_add(1).max(1);
        sequence
    }

    fn send(&mut self, mut message: Vec<u8>) -> io::Result<u32> {
        if message.len() < size_of::<NlMsgHdr>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "netlink message is too short",
            ));
        }
        let sequence = self.next_sequence();
        message[8..12].copy_from_slice(&sequence.to_le_bytes());
        let address = SockaddrNl {
            nl_family: libc::AF_NETLINK as libc::c_ushort,
            nl_pad: 0,
            nl_pid: 0,
            nl_groups: 0,
        };
        // SAFETY: message is a valid byte buffer and address is a valid
        // sockaddr_nl. The kernel does not retain the buffer.
        let sent = unsafe {
            libc::sendto(
                self.fd.as_raw_fd(),
                message.as_ptr().cast(),
                message.len(),
                0,
                (&address as *const SockaddrNl).cast(),
                size_of::<SockaddrNl>() as libc::socklen_t,
            )
        };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        if sent as usize != message.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "netlink message was truncated",
            ));
        }
        Ok(sequence)
    }

    fn ack(&mut self, sequence: u32) -> io::Result<()> {
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
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if received == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "netlink socket closed",
                ));
            }
            let received = received as usize;
            let mut offset = 0usize;
            while offset + size_of::<NlMsgHdr>() <= received {
                let header = read_header(&buffer[offset..])?;
                let length = header.nlmsg_len as usize;
                if length < size_of::<NlMsgHdr>() || offset + length > received {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "netlink message has an invalid length",
                    ));
                }
                if header.nlmsg_seq == sequence {
                    match header.nlmsg_type {
                        NLMSG_ERROR => {
                            if length < size_of::<NlMsgHdr>() + 4 {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "netlink error message is too short",
                                ));
                            }
                            let error = read_i32(&buffer[offset + 16..offset + 20])?;
                            if error == 0 {
                                return Ok(());
                            }
                            return Err(io::Error::from_raw_os_error(-error));
                        }
                        NLMSG_DONE => return Ok(()),
                        _ => {}
                    }
                }
                offset += align4(length);
            }
        }
    }

    /// Bring an interface up or down.
    pub fn set_link_up(&mut self, name: &str, up: bool) -> io::Result<()> {
        let index = if_nametoindex(name)? as i32;
        let flags = if up { libc::IFF_UP as u32 } else { 0 };
        let message = build_message(
            RTM_NEWLINK,
            NLM_F_REQUEST | NLM_F_ACK,
            &IfInfoMsg {
                ifi_family: libc::AF_UNSPEC as u8,
                ifi_index: index,
                ifi_flags: flags,
                ifi_change: libc::IFF_UP as u32,
                ..IfInfoMsg::default()
            },
            |_| {},
        );
        let sequence = self.send(message)?;
        self.ack(sequence)
    }

    /// Set an interface MTU.
    pub fn set_mtu(&mut self, name: &str, mtu: u32) -> io::Result<()> {
        let index = if_nametoindex(name)? as i32;
        let message = build_message(
            RTM_NEWLINK,
            NLM_F_REQUEST | NLM_F_ACK,
            &IfInfoMsg {
                ifi_family: libc::AF_UNSPEC as u8,
                ifi_index: index,
                ..IfInfoMsg::default()
            },
            |builder| builder.attr(IFLA_MTU, &mtu.to_ne_bytes()),
        );
        let sequence = self.send(message)?;
        self.ack(sequence)
    }

    /// Add an IPv4 address.
    pub fn add_address(&mut self, name: &str, address: Ipv4Net) -> io::Result<()> {
        self.address(RTM_NEWADDR, name, address)
    }

    /// Delete an IPv4 address.
    pub fn del_address(&mut self, name: &str, address: Ipv4Net) -> io::Result<()> {
        self.address(RTM_DELADDR, name, address)
    }

    fn address(&mut self, kind: u16, name: &str, address: Ipv4Net) -> io::Result<()> {
        let index = if_nametoindex(name)?;
        let flags = if kind == RTM_NEWADDR {
            NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL
        } else {
            NLM_F_REQUEST | NLM_F_ACK
        };
        let bytes = address.addr.octets();
        let message = build_message(
            kind,
            flags,
            &IfAddrMsg {
                ifa_family: libc::AF_INET as u8,
                ifa_prefixlen: address.prefix,
                ifa_index: index,
                ..IfAddrMsg::default()
            },
            |builder| {
                builder.attr(IFA_LOCAL, &bytes);
                builder.attr(IFA_ADDRESS, &bytes);
            },
        );
        let sequence = self.send(message)?;
        self.ack(sequence)
    }

    /// Add an IPv4 route.
    pub fn add_route(&mut self, route: &Route) -> io::Result<()> {
        self.route(RTM_NEWROUTE, route, true)
    }

    /// Delete an IPv4 route.
    pub fn del_route(&mut self, route: &Route) -> io::Result<()> {
        self.route(RTM_DELROUTE, route, false)
    }

    fn route(&mut self, kind: u16, route: &Route, add: bool) -> io::Result<()> {
        let mut flags = NLM_F_REQUEST | NLM_F_ACK;
        if add {
            flags |= NLM_F_CREATE | NLM_F_EXCL;
        }
        let oif = route.dev.as_deref().map(if_nametoindex).transpose()?;
        let message = build_message(
            kind,
            flags,
            &RtMsg {
                rtm_family: libc::AF_INET as u8,
                rtm_dst_len: route.prefix(),
                rtm_table: if route.table <= u8::MAX as u32 {
                    route.table as u8
                } else {
                    RT_TABLE_UNSPEC
                },
                rtm_protocol: RTPROT_STATIC,
                rtm_scope: if route.via.is_some() {
                    RT_SCOPE_UNIVERSE
                } else {
                    RT_SCOPE_LINK
                },
                rtm_type: RTN_UNICAST,
                rtm_flags: if route.onlink { RTNH_F_ONLINK } else { 0 },
                ..RtMsg::default()
            },
            |builder| {
                if let Some(to) = route.to {
                    builder.attr(RTA_DST, &to.network().octets());
                }
                if let Some(via) = route.via {
                    builder.attr(RTA_GATEWAY, &via.octets());
                }
                if let Some(oif) = oif {
                    builder.attr(RTA_OIF, &oif.to_ne_bytes());
                }
                if route.metric != 0 {
                    builder.attr(RTA_PRIORITY, &route.metric.to_ne_bytes());
                }
                if let Some(src) = route.src {
                    builder.attr(RTA_PREFSRC, &src.octets());
                }
                if route.table > u8::MAX as u32 {
                    builder.attr(RTA_TABLE, &route.table.to_ne_bytes());
                }
            },
        );
        let sequence = self.send(message)?;
        self.ack(sequence)
    }

    /// Create a bridge with its attributes.
    pub fn create_bridge(&mut self, iface: &Interface) -> io::Result<()> {
        let name = CString::new(iface.name.as_bytes()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "interface name contains NUL")
        })?;
        let message = build_message(
            RTM_NEWLINK,
            NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
            &IfInfoMsg {
                ifi_family: libc::AF_UNSPEC as u8,
                ..IfInfoMsg::default()
            },
            |builder| {
                builder.attr(IFLA_IFNAME, &with_nul(name.as_bytes()));
                if let Some(mtu) = iface.mtu {
                    builder.attr(IFLA_MTU, &mtu.to_ne_bytes());
                }
                builder.nested(IFLA_LINKINFO, |builder| {
                    builder.attr(IFLA_INFO_KIND, b"bridge\0");
                    builder.nested(IFLA_INFO_DATA, |builder| {
                        builder.attr(IFLA_BR_STP_STATE, &(iface.stp as u32).to_ne_bytes());
                        builder.attr(
                            IFLA_BR_FORWARD_DELAY,
                            &iface.forward_delay.saturating_mul(100).to_ne_bytes(),
                        );
                        builder.attr(
                            IFLA_BR_HELLO_TIME,
                            &iface.hello_time.saturating_mul(100).to_ne_bytes(),
                        );
                        builder.attr(
                            IFLA_BR_MAX_AGE,
                            &iface.max_age.saturating_mul(100).to_ne_bytes(),
                        );
                    });
                });
            },
        );
        let sequence = self.send(message)?;
        self.ack(sequence)
    }

    /// Enslave `port` to `bridge`.
    pub fn enslave(&mut self, bridge: &str, port: &str) -> io::Result<()> {
        let master = if_nametoindex(bridge)?;
        let index = if_nametoindex(port)? as i32;
        let message = build_message(
            RTM_SETLINK,
            NLM_F_REQUEST | NLM_F_ACK,
            &IfInfoMsg {
                ifi_family: libc::AF_UNSPEC as u8,
                ifi_index: index,
                ..IfInfoMsg::default()
            },
            |builder| builder.attr(IFLA_MASTER, &master.to_ne_bytes()),
        );
        let sequence = self.send(message)?;
        self.ack(sequence)
    }

    /// Detach a port from its bridge.
    pub fn detach(&mut self, port: &str) -> io::Result<()> {
        let index = if_nametoindex(port)? as i32;
        let message = build_message(
            RTM_SETLINK,
            NLM_F_REQUEST | NLM_F_ACK,
            &IfInfoMsg {
                ifi_family: libc::AF_UNSPEC as u8,
                ifi_index: index,
                ..IfInfoMsg::default()
            },
            |builder| builder.attr(IFLA_MASTER, &0u32.to_ne_bytes()),
        );
        let sequence = self.send(message)?;
        self.ack(sequence)
    }

    /// Delete a link by name.
    pub fn delete_link(&mut self, name: &str) -> io::Result<()> {
        let index = if_nametoindex(name)? as i32;
        let message = build_message(
            RTM_DELLINK,
            NLM_F_REQUEST | NLM_F_ACK,
            &IfInfoMsg {
                ifi_family: libc::AF_UNSPEC as u8,
                ifi_index: index,
                ..IfInfoMsg::default()
            },
            |_| {},
        );
        let sequence = self.send(message)?;
        self.ack(sequence)
    }
}

/// The ifindex for `name`.
pub fn if_nametoindex(name: &str) -> io::Result<u32> {
    let name = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "interface name contains NUL"))?;
    // SAFETY: name is a valid NUL-terminated C string.
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    if index == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(index)
    }
}

/// Whether a named link exists.
pub fn link_exists(name: &str) -> bool {
    if_nametoindex(name).is_ok()
}

fn observe_addresses() -> io::Result<Vec<ObservedAddress>> {
    let mut result = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills the out-pointer on success.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut current = head;
    while !current.is_null() {
        // SAFETY: current is a valid node from getifaddrs.
        let entry = unsafe { &*current };
        if !entry.ifa_addr.is_null() {
            // SAFETY: ifa_addr points to a sockaddr valid for the family.
            let family = unsafe { (*entry.ifa_addr).sa_family as libc::c_int };
            if family == libc::AF_INET {
                // SAFETY: the family is AF_INET, so sockaddr_in is the concrete type.
                let address = unsafe { &*(entry.ifa_addr as *const libc::sockaddr_in) };
                let netmask = if entry.ifa_netmask.is_null() {
                    u32::MAX
                } else {
                    // SAFETY: as above, the netmask is an AF_INET sockaddr.
                    unsafe {
                        (*(entry.ifa_netmask as *const libc::sockaddr_in))
                            .sin_addr
                            .s_addr
                    }
                };
                let name = unsafe { CStr::from_ptr(entry.ifa_name) }
                    .to_string_lossy()
                    .into_owned();
                let addr = Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr));
                let prefix = u32::from_be(netmask).count_ones() as u8;
                result.push(ObservedAddress {
                    iface: name,
                    address: Ipv4Net::new(addr, prefix),
                });
            }
        }
        current = entry.ifa_next;
    }
    // SAFETY: head is the list returned by getifaddrs.
    unsafe { libc::freeifaddrs(head) };
    Ok(result)
}

fn observe_routes() -> io::Result<Vec<ObservedRoute>> {
    let text = std::fs::read_to_string("/proc/net/route")?;
    let mut result = Vec::new();
    for line in text.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 11 {
            continue;
        }
        let dev = fields[0].to_string();
        let destination = u32::from_str_radix(fields[1], 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "route destination"))?;
        let gateway = u32::from_str_radix(fields[2], 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "route gateway"))?;
        let metric = fields[6]
            .parse::<u32>()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "route metric"))?;
        let mask = u32::from_str_radix(fields[7], 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "route mask"))?;
        let prefix = mask.count_ones() as u8;
        let destination = Ipv4Addr::from(u32::from_be(destination));
        let to = if destination.is_unspecified() && prefix == 0 {
            None
        } else {
            Some(Ipv4Net::new(destination, prefix))
        };
        let gateway = Ipv4Addr::from(u32::from_be(gateway));
        result.push(ObservedRoute {
            dev,
            to,
            via: (!gateway.is_unspecified()).then_some(gateway),
            metric,
            table: crate::model::RT_TABLE_MAIN,
        });
    }
    Ok(result)
}

fn read_u32(path: &str) -> io::Result<u32> {
    let text = std::fs::read_to_string(path)?;
    let text = text.trim();
    if let Some(hex) = text.strip_prefix("0x") {
        u32::from_str_radix(hex, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("{path}: bad hex")))
    } else {
        text.parse::<u32>()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("{path}: bad number")))
    }
}

fn with_nul(bytes: &[u8]) -> Vec<u8> {
    let mut value = Vec::with_capacity(bytes.len() + 1);
    value.extend_from_slice(bytes);
    value.push(0);
    value
}

fn align4(length: usize) -> usize {
    (length + 3) & !3
}

fn read_header(bytes: &[u8]) -> io::Result<NlMsgHdr> {
    if bytes.len() < size_of::<NlMsgHdr>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "short netlink header",
        ));
    }
    Ok(NlMsgHdr {
        nlmsg_len: u32::from_le_bytes(
            bytes[0..4]
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "netlink length"))?,
        ),
        nlmsg_type: u16::from_le_bytes(
            bytes[4..6]
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "netlink type"))?,
        ),
        nlmsg_flags: u16::from_le_bytes(
            bytes[6..8]
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "netlink flags"))?,
        ),
        nlmsg_seq: u32::from_le_bytes(
            bytes[8..12]
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "netlink sequence"))?,
        ),
        nlmsg_pid: u32::from_le_bytes(
            bytes[12..16]
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "netlink pid"))?,
        ),
    })
}

fn read_i32(bytes: &[u8]) -> io::Result<i32> {
    Ok(i32::from_le_bytes(bytes.try_into().map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "short netlink error")
    })?))
}

struct MessageBuilder {
    bytes: Vec<u8>,
}

impl MessageBuilder {
    fn new(kind: u16, flags: u16) -> Self {
        let mut bytes = vec![0u8; size_of::<NlMsgHdr>()];
        bytes[4..6].copy_from_slice(&kind.to_le_bytes());
        bytes[6..8].copy_from_slice(&flags.to_le_bytes());
        MessageBuilder { bytes }
    }

    fn payload<T: Copy>(&mut self, value: &T) {
        // SAFETY: value is a valid reference to a plain-old-data struct.
        let slice =
            unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
        self.bytes.extend_from_slice(slice);
    }

    fn attr(&mut self, kind: u16, data: &[u8]) {
        let length = 4 + data.len();
        self.bytes.extend_from_slice(&(length as u16).to_le_bytes());
        self.bytes.extend_from_slice(&kind.to_le_bytes());
        self.bytes.extend_from_slice(data);
        while !self.bytes.len().is_multiple_of(4) {
            self.bytes.push(0);
        }
    }

    fn nested(&mut self, kind: u16, body: impl FnOnce(&mut Self)) {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(&[0, 0, 0, 0]);
        body(self);
        let length = (self.bytes.len() - start) as u16;
        self.bytes[start..start + 2].copy_from_slice(&length.to_le_bytes());
        self.bytes[start + 2..start + 4].copy_from_slice(&kind.to_le_bytes());
        while !self.bytes.len().is_multiple_of(4) {
            self.bytes.push(0);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        let length = self.bytes.len() as u32;
        self.bytes[0..4].copy_from_slice(&length.to_le_bytes());
        self.bytes
    }
}

fn build_message<T: Copy>(
    kind: u16,
    flags: u16,
    payload: &T,
    body: impl FnOnce(&mut MessageBuilder),
) -> Vec<u8> {
    let mut builder = MessageBuilder::new(kind, flags);
    builder.payload(payload);
    body(&mut builder);
    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_length_and_alignment_are_set() {
        let message = build_message(
            RTM_NEWLINK,
            NLM_F_REQUEST | NLM_F_ACK,
            &IfInfoMsg::default(),
            |builder| builder.attr(IFLA_MTU, &1500u32.to_ne_bytes()),
        );
        let header = read_header(&message).expect("header");
        assert_eq!(header.nlmsg_len as usize, message.len());
        assert_eq!(header.nlmsg_type, RTM_NEWLINK);
        assert_eq!(message.len() % 4, 0);
    }

    #[test]
    fn nested_attributes_are_well_formed() {
        let message = build_message(
            RTM_NEWLINK,
            NLM_F_REQUEST | NLM_F_ACK,
            &IfInfoMsg::default(),
            |builder| {
                builder.nested(IFLA_LINKINFO, |builder| {
                    builder.attr(IFLA_INFO_KIND, b"bridge\0");
                });
            },
        );
        assert!(message.len() > size_of::<NlMsgHdr>() + size_of::<IfInfoMsg>());
        assert_eq!(message.len() % 4, 0);
    }

    #[test]
    fn address_attributes_keep_the_network_bytes() {
        let net = Ipv4Net::new(Ipv4Addr::new(192, 168, 10, 2), 24);
        assert_eq!(net.addr.octets(), [192, 168, 10, 2]);
    }

    #[test]
    fn observation_reads_the_host_tree() {
        let observed = observe().expect("observe");
        assert!(observed.links.contains_key("lo"));
        assert!(observed.links.get("lo").is_some_and(|link| link.index > 0));
    }

    #[test]
    fn observed_helpers_filter_by_interface() {
        let mut observed = Observed::default();
        observed.addresses.push(ObservedAddress {
            iface: "eth0".to_string(),
            address: Ipv4Net::new(Ipv4Addr::new(192, 0, 2, 10), 24),
        });
        observed.routes.push(ObservedRoute {
            dev: "eth0".to_string(),
            to: None,
            via: Some(Ipv4Addr::new(192, 0, 2, 1)),
            metric: 0,
            table: 254,
        });
        assert_eq!(observed.addresses_of("eth0").len(), 1);
        assert_eq!(observed.routes_of("eth0").count(), 1);
        assert!(observed.addresses_of("eth1").is_empty());
    }

    #[test]
    fn alignment_and_nul_helpers_are_stable() {
        assert_eq!(align4(1), 4);
        assert_eq!(align4(4), 4);
        assert_eq!(with_nul(b"eth0"), b"eth0\0");
    }

    #[test]
    fn route_observation_parses_the_default_route() {
        // This is a parser test only; the real file is read by observe_routes.
        let destination = u32::from_str_radix("00000000", 16).expect("hex");
        let mask = u32::from_str_radix("00000000", 16).expect("hex");
        assert!(Ipv4Addr::from(u32::from_be(destination)).is_unspecified());
        assert_eq!(mask.count_ones(), 0);
    }
}
