//! DHCPv4 lease model and `/run/rnet/leases/<iface>` persistence.

use std::fmt;
use std::net::Ipv4Addr;
use std::path::Path;

use crate::dhcp::packet::{DhcpError, MessageType, Packet};
use crate::model::Ipv4Net;

/// A DHCPv4 lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    /// Assigned address.
    pub address: Ipv4Addr,
    /// Prefix length derived from the subnet mask.
    pub prefix: u8,
    /// Server identifier.
    pub server: Ipv4Addr,
    /// Lease duration in seconds.
    pub lease_time: u32,
    /// Renewal timer in seconds.
    pub t1: u32,
    /// Rebind timer in seconds.
    pub t2: u32,
    /// Routers from option 3.
    pub routers: Vec<Ipv4Addr>,
    /// DNS servers from option 6.
    pub dns: Vec<Ipv4Addr>,
    /// Domain from option 15.
    pub domain: Option<String>,
    /// Classless/static routes from options 121/33.
    pub routes: Vec<(Ipv4Net, Ipv4Addr)>,
}

impl Lease {
    /// Build a lease from an ACK.
    pub fn from_ack(packet: &Packet, fallback_lease: u32) -> Result<Self, DhcpError> {
        if packet.options.message_type != Some(MessageType::Ack) {
            return Err(DhcpError::new("lease requires a DHCPACK"));
        }
        if packet.yiaddr.is_unspecified() {
            return Err(DhcpError::new("DHCPACK has no assigned address"));
        }
        let server = packet
            .options
            .server_id
            .or_else(|| (!packet.siaddr.is_unspecified()).then_some(packet.siaddr))
            .ok_or_else(|| DhcpError::new("DHCPACK has no server identifier"))?;
        let lease_time = packet
            .options
            .lease_time
            .filter(|lease| *lease > 0)
            .unwrap_or(fallback_lease)
            .max(1);
        let prefix = packet
            .options
            .subnet_mask
            .map_or(24, |mask| u32::from(mask).count_ones() as u8);
        let mut t1 = packet.options.t1.unwrap_or(lease_time / 2);
        let mut t2 = packet
            .options
            .t2
            .unwrap_or(lease_time.saturating_mul(7) / 8);
        t1 = t1.min(lease_time);
        t2 = t2.min(lease_time).max(t1);
        Ok(Lease {
            address: packet.yiaddr,
            prefix,
            server,
            lease_time,
            t1,
            t2,
            routers: packet.options.routers.clone(),
            dns: packet.options.dns.clone(),
            domain: packet.options.domain.clone(),
            routes: packet.options.routes.clone(),
        })
    }

    /// The assigned address with its prefix.
    pub fn network(&self) -> Ipv4Net {
        Ipv4Net::new(self.address, self.prefix)
    }

    /// Whether the lease still has time at `elapsed` seconds into its life.
    pub fn is_valid_at(&self, elapsed: u32) -> bool {
        elapsed < self.lease_time
    }

    /// Serialize the lease for `/run/rnet/leases/<iface>`.
    pub fn to_text(&self) -> String {
        let mut out = format!(
            "address={}\nprefix={}\nserver={}\nlease={}\nt1={}\nt2={}\n",
            self.address, self.prefix, self.server, self.lease_time, self.t1, self.t2
        );
        if !self.routers.is_empty() {
            out.push_str(&format!(
                "router={}\n",
                self.routers
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
        if !self.dns.is_empty() {
            out.push_str(&format!(
                "dns={}\n",
                self.dns
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
        if let Some(domain) = &self.domain {
            out.push_str(&format!("domain={domain}\n"));
        }
        for (to, via) in &self.routes {
            out.push_str(&format!("route={to} {via}\n"));
        }
        out
    }

    /// Parse the lease file.
    pub fn from_text(text: &str) -> Result<Self, LeaseError> {
        let mut address = None;
        let mut prefix = None;
        let mut server = None;
        let mut lease_time = None;
        let mut t1 = None;
        let mut t2 = None;
        let mut routers = Vec::new();
        let mut dns = Vec::new();
        let mut domain = None;
        let mut routes = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| LeaseError::new(index + 1, "expected key=value"))?;
            let value = value.trim();
            match key {
                "address" => address = Some(parse_ip(value, index + 1, "address")?),
                "prefix" => {
                    let parsed = value
                        .parse::<u8>()
                        .map_err(|_| LeaseError::new(index + 1, "prefix is not a number"))?;
                    if parsed > 32 {
                        return Err(LeaseError::new(index + 1, "prefix is outside 0..=32"));
                    }
                    prefix = Some(parsed);
                }
                "server" => server = Some(parse_ip(value, index + 1, "server")?),
                "lease" => lease_time = Some(parse_u32(value, index + 1, "lease")?),
                "t1" => t1 = Some(parse_u32(value, index + 1, "t1")?),
                "t2" => t2 = Some(parse_u32(value, index + 1, "t2")?),
                "router" => {
                    for item in value.split_whitespace() {
                        routers.push(parse_ip(item, index + 1, "router")?);
                    }
                }
                "dns" => {
                    for item in value.split_whitespace() {
                        dns.push(parse_ip(item, index + 1, "dns")?);
                    }
                }
                "domain" => domain = Some(value.to_string()),
                "route" => {
                    let mut parts = value.split_whitespace();
                    let to = parts
                        .next()
                        .ok_or_else(|| LeaseError::new(index + 1, "route needs a destination"))?;
                    let via = parts
                        .next()
                        .ok_or_else(|| LeaseError::new(index + 1, "route needs a gateway"))?;
                    if parts.next().is_some() {
                        return Err(LeaseError::new(index + 1, "route has extra fields"));
                    }
                    let to = to
                        .parse::<Ipv4Net>()
                        .map_err(|e| LeaseError::new(index + 1, e))?;
                    routes.push((to, parse_ip(via, index + 1, "route gateway")?));
                }
                other => {
                    return Err(LeaseError::new(
                        index + 1,
                        format!("unknown lease key '{other}'"),
                    ));
                }
            }
        }
        Ok(Lease {
            address: address.ok_or_else(|| LeaseError::new(0, "missing address"))?,
            prefix: prefix.ok_or_else(|| LeaseError::new(0, "missing prefix"))?,
            server: server.ok_or_else(|| LeaseError::new(0, "missing server"))?,
            lease_time: lease_time.ok_or_else(|| LeaseError::new(0, "missing lease"))?,
            t1: t1.unwrap_or(lease_time.unwrap_or(0) / 2),
            t2: t2.unwrap_or(lease_time.unwrap_or(0).saturating_mul(7) / 8),
            routers,
            dns,
            domain,
            routes,
        })
    }

    /// Write the lease atomically.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, self.to_text())?;
        std::fs::rename(&temporary, path)
    }

    /// Read a lease file.
    pub fn read(path: &Path) -> Result<Self, LeaseError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| LeaseError::new(0, format!("cannot read {}: {e}", path.display())))?;
        Self::from_text(&text)
    }
}

impl fmt::Display for Lease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{} via {} lease {}s",
            self.address, self.prefix, self.server, self.lease_time
        )
    }
}

fn parse_ip(value: &str, line: usize, name: &str) -> Result<Ipv4Addr, LeaseError> {
    value
        .parse()
        .map_err(|_| LeaseError::new(line, format!("{name} is not an IPv4 address")))
}

fn parse_u32(value: &str, line: usize, name: &str) -> Result<u32, LeaseError> {
    value
        .parse()
        .map_err(|_| LeaseError::new(line, format!("{name} is not a number")))
}

/// A lease-file parse error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseError {
    /// One-based line, or zero for a whole-file error.
    pub line: usize,
    /// Description.
    pub message: String,
}

impl LeaseError {
    /// Build an error.
    pub fn new(line: usize, message: impl Into<String>) -> Self {
        LeaseError {
            line,
            message: message.into(),
        }
    }
}

impl fmt::Display for LeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            f.write_str(&self.message)
        } else {
            write!(f, "line {}: {}", self.line, self.message)
        }
    }
}

impl std::error::Error for LeaseError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn ack() -> Packet {
        Packet {
            op: 2,
            htype: 1,
            hlen: 6,
            hops: 0,
            xid: 1,
            secs: 0,
            flags: 0,
            ciaddr: Ipv4Addr::UNSPECIFIED,
            yiaddr: Ipv4Addr::new(192, 168, 1, 50),
            siaddr: Ipv4Addr::new(192, 168, 1, 1),
            giaddr: Ipv4Addr::UNSPECIFIED,
            chaddr: [2, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            options: crate::dhcp::packet::Options {
                message_type: Some(MessageType::Ack),
                server_id: Some(Ipv4Addr::new(192, 168, 1, 1)),
                lease_time: Some(3600),
                t1: Some(1800),
                t2: Some(3150),
                subnet_mask: Some(Ipv4Addr::new(255, 255, 255, 0)),
                routers: vec![Ipv4Addr::new(192, 168, 1, 1)],
                dns: vec![Ipv4Addr::new(9, 9, 9, 9)],
                domain: Some("lan".to_string()),
                ..crate::dhcp::packet::Options::default()
            },
        }
    }

    #[test]
    fn lease_from_ack_and_roundtrip() {
        let lease = Lease::from_ack(&ack(), 3600).expect("lease");
        assert_eq!(lease.address, Ipv4Addr::new(192, 168, 1, 50));
        assert_eq!(lease.prefix, 24);
        let text = lease.to_text();
        assert_eq!(Lease::from_text(&text).expect("parse"), lease);
    }

    #[test]
    fn malformed_lease_is_rejected() {
        assert!(Lease::from_text("address=192.0.2.1\n").is_err());
        assert!(Lease::from_text("address=192.0.2.1\nprefix=33\n").is_err());
    }
}
