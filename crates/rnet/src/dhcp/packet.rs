//! DHCPv4 packet and option codec.
//!
//! Only the options the client consumes or sends are modelled. Everything is
//! length-checked: a malformed or truncated option is an error, never a panic.

use std::fmt;
use std::net::Ipv4Addr;

use crate::model::Ipv4Net;

/// BOOTP fixed header length.
pub const HEADER_LEN: usize = 236;
/// Magic cookie that starts the options.
pub const MAGIC_COOKIE: [u8; 4] = [99, 130, 83, 99];
/// Maximum DHCP message size we emit.
pub const MIN_MESSAGE_LEN: usize = 300;

/// DHCP message type (option 53).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    Discover = 1,
    Offer = 2,
    Request = 3,
    Decline = 4,
    Ack = 5,
    Nak = 6,
    Release = 7,
    Inform = 8,
}

impl MessageType {
    /// Parse the wire value.
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(MessageType::Discover),
            2 => Some(MessageType::Offer),
            3 => Some(MessageType::Request),
            4 => Some(MessageType::Decline),
            5 => Some(MessageType::Ack),
            6 => Some(MessageType::Nak),
            7 => Some(MessageType::Release),
            8 => Some(MessageType::Inform),
            _ => None,
        }
    }
}

/// A parsed DHCP option set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub message_type: Option<MessageType>,
    pub server_id: Option<Ipv4Addr>,
    pub requested_ip: Option<Ipv4Addr>,
    pub lease_time: Option<u32>,
    pub t1: Option<u32>,
    pub t2: Option<u32>,
    pub subnet_mask: Option<Ipv4Addr>,
    pub routers: Vec<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
    pub domain: Option<String>,
    pub mtu: Option<u16>,
    /// Option 121, or classful option 33 when 121 is absent.
    pub routes: Vec<(Ipv4Net, Ipv4Addr)>,
    pub hostname: Option<String>,
    pub client_id: Option<Vec<u8>>,
    pub parameter_request_list: Vec<u8>,
    pub max_message_size: Option<u16>,
}

/// A DHCPv4 packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub op: u8,
    pub htype: u8,
    pub hlen: u8,
    pub hops: u8,
    pub xid: u32,
    pub secs: u16,
    pub flags: u16,
    pub ciaddr: Ipv4Addr,
    pub yiaddr: Ipv4Addr,
    pub siaddr: Ipv4Addr,
    pub giaddr: Ipv4Addr,
    pub chaddr: [u8; 16],
    pub options: Options,
}

impl Packet {
    /// Build a client packet with the common header fields.
    pub fn client(message_type: MessageType, xid: u32, mac: [u8; 6], ciaddr: Ipv4Addr) -> Self {
        let mut chaddr = [0u8; 16];
        chaddr[..6].copy_from_slice(&mac);
        Packet {
            op: 1,
            htype: 1,
            hlen: 6,
            hops: 0,
            xid,
            secs: 0,
            flags: if ciaddr.is_unspecified() { 0x8000 } else { 0 },
            ciaddr,
            yiaddr: Ipv4Addr::UNSPECIFIED,
            siaddr: Ipv4Addr::UNSPECIFIED,
            giaddr: Ipv4Addr::UNSPECIFIED,
            chaddr,
            options: Options {
                message_type: Some(message_type),
                client_id: Some([&[1u8][..], &mac[..]].concat()),
                parameter_request_list: vec![1, 3, 6, 12, 15, 26, 28, 33, 121],
                max_message_size: Some(1500),
                ..Options::default()
            },
        }
    }

    /// The hardware address as a six-byte MAC, when `hlen` is six.
    pub fn mac(&self) -> Option<[u8; 6]> {
        if self.htype != 1 || self.hlen != 6 {
            return None;
        }
        let mut mac = [0u8; 6];
        mac.copy_from_slice(self.chaddr.get(..6)?);
        Some(mac)
    }

    /// Encode the packet, padding to the DHCP minimum of 300 bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![0u8; HEADER_LEN];
        out[0] = self.op;
        out[1] = self.htype;
        out[2] = self.hlen;
        out[3] = self.hops;
        out[4..8].copy_from_slice(&self.xid.to_be_bytes());
        out[8..10].copy_from_slice(&self.secs.to_be_bytes());
        out[10..12].copy_from_slice(&self.flags.to_be_bytes());
        out[12..16].copy_from_slice(&self.ciaddr.octets());
        out[16..20].copy_from_slice(&self.yiaddr.octets());
        out[20..24].copy_from_slice(&self.siaddr.octets());
        out[24..28].copy_from_slice(&self.giaddr.octets());
        out[28..44].copy_from_slice(&self.chaddr);
        out.extend_from_slice(&MAGIC_COOKIE);

        let mut options = Vec::new();
        if let Some(kind) = self.options.message_type {
            push_option(&mut options, 53, &[kind as u8]);
        }
        if let Some(client_id) = &self.options.client_id {
            push_option(&mut options, 61, client_id);
        }
        if let Some(requested) = self.options.requested_ip {
            push_option(&mut options, 50, &requested.octets());
        }
        if let Some(server) = self.options.server_id {
            push_option(&mut options, 54, &server.octets());
        }
        if let Some(hostname) = &self.options.hostname {
            push_option(&mut options, 12, hostname.as_bytes());
        }
        if !self.options.parameter_request_list.is_empty() {
            push_option(&mut options, 55, &self.options.parameter_request_list);
        }
        if let Some(max) = self.options.max_message_size {
            push_option(&mut options, 57, &max.to_be_bytes());
        }
        if let Some(mask) = self.options.subnet_mask {
            push_option(&mut options, 1, &mask.octets());
        }
        if !self.options.routers.is_empty() {
            let bytes = self
                .options
                .routers
                .iter()
                .flat_map(|router| router.octets())
                .collect::<Vec<_>>();
            push_option(&mut options, 3, &bytes);
        }
        if !self.options.dns.is_empty() {
            let bytes = self
                .options
                .dns
                .iter()
                .flat_map(|dns| dns.octets())
                .collect::<Vec<_>>();
            push_option(&mut options, 6, &bytes);
        }
        if let Some(domain) = &self.options.domain {
            push_option(&mut options, 15, domain.as_bytes());
        }
        if let Some(mtu) = self.options.mtu {
            push_option(&mut options, 26, &mtu.to_be_bytes());
        }
        if let Some(lease) = self.options.lease_time {
            push_option(&mut options, 51, &lease.to_be_bytes());
        }
        if let Some(t1) = self.options.t1 {
            push_option(&mut options, 58, &t1.to_be_bytes());
        }
        if let Some(t2) = self.options.t2 {
            push_option(&mut options, 59, &t2.to_be_bytes());
        }
        if !self.options.routes.is_empty() {
            let mut bytes = Vec::new();
            for (to, via) in &self.options.routes {
                let width = to.prefix.div_ceil(8) as usize;
                let network = to.network().octets();
                bytes.push(width as u8);
                bytes.extend_from_slice(&network[..width]);
                bytes.extend_from_slice(&via.octets());
            }
            push_option(&mut options, 121, &bytes);
        }
        options.push(255);
        out.extend_from_slice(&options);
        while out.len() < MIN_MESSAGE_LEN {
            out.push(0);
        }
        out
    }

    /// Decode and validate a packet.
    pub fn decode(bytes: &[u8]) -> Result<Self, DhcpError> {
        if bytes.len() < HEADER_LEN + 4 {
            return Err(DhcpError::new("packet shorter than the BOOTP header"));
        }
        if bytes[HEADER_LEN..HEADER_LEN + 4] != MAGIC_COOKIE {
            return Err(DhcpError::new("missing DHCP magic cookie"));
        }
        let mut chaddr = [0u8; 16];
        chaddr.copy_from_slice(&bytes[28..44]);
        let mut packet = Packet {
            op: bytes[0],
            htype: bytes[1],
            hlen: bytes[2],
            hops: bytes[3],
            xid: u32::from_be_bytes(
                bytes[4..8]
                    .try_into()
                    .map_err(|_| DhcpError::new("truncated transaction id"))?,
            ),
            secs: u16::from_be_bytes(
                bytes[8..10]
                    .try_into()
                    .map_err(|_| DhcpError::new("truncated seconds"))?,
            ),
            flags: u16::from_be_bytes(
                bytes[10..12]
                    .try_into()
                    .map_err(|_| DhcpError::new("truncated flags"))?,
            ),
            ciaddr: Ipv4Addr::new(bytes[12], bytes[13], bytes[14], bytes[15]),
            yiaddr: Ipv4Addr::new(bytes[16], bytes[17], bytes[18], bytes[19]),
            siaddr: Ipv4Addr::new(bytes[20], bytes[21], bytes[22], bytes[23]),
            giaddr: Ipv4Addr::new(bytes[24], bytes[25], bytes[26], bytes[27]),
            chaddr,
            options: Options::default(),
        };
        if packet.hlen > 16 {
            return Err(DhcpError::new("hardware address length exceeds 16"));
        }
        let options = &bytes[HEADER_LEN + 4..];
        let mut offset = 0usize;
        let mut option_121: Option<Vec<(Ipv4Net, Ipv4Addr)>> = None;
        let mut option_33: Option<Vec<(Ipv4Net, Ipv4Addr)>> = None;
        while offset < options.len() {
            let code = options[offset];
            offset += 1;
            match code {
                0 => continue,
                255 => break,
                _ => {}
            }
            if offset >= options.len() {
                return Err(DhcpError::new("option length is missing"));
            }
            let length = options[offset] as usize;
            offset += 1;
            if offset + length > options.len() {
                return Err(DhcpError::new("option value is truncated"));
            }
            let value = &options[offset..offset + length];
            offset += length;
            match code {
                1 => packet.options.subnet_mask = parse_ip(value, 4, "subnet mask")?,
                3 => packet.options.routers = parse_ips(value, "router")?,
                6 => packet.options.dns = parse_ips(value, "DNS")?,
                12 => packet.options.hostname = Some(String::from_utf8_lossy(value).into_owned()),
                15 => packet.options.domain = Some(String::from_utf8_lossy(value).into_owned()),
                26 => {
                    packet.options.mtu =
                        Some(u16::from_be_bytes(value.try_into().map_err(|_| {
                            DhcpError::new("MTU option has the wrong length")
                        })?))
                }
                33 => option_33 = Some(parse_classful_routes(value)?),
                50 => packet.options.requested_ip = parse_ip(value, 4, "requested IP")?,
                51 => {
                    packet.options.lease_time =
                        Some(u32::from_be_bytes(value.try_into().map_err(|_| {
                            DhcpError::new("lease option has the wrong length")
                        })?))
                }
                53 => {
                    let kind = value
                        .first()
                        .copied()
                        .and_then(MessageType::from_u8)
                        .ok_or_else(|| DhcpError::new("unknown DHCP message type"))?;
                    packet.options.message_type = Some(kind);
                }
                54 => packet.options.server_id = parse_ip(value, 4, "server id")?,
                55 => packet.options.parameter_request_list = value.to_vec(),
                57 => {
                    packet.options.max_message_size =
                        Some(u16::from_be_bytes(value.try_into().map_err(|_| {
                            DhcpError::new("maximum message size has the wrong length")
                        })?))
                }
                58 => {
                    packet.options.t1 =
                        Some(u32::from_be_bytes(value.try_into().map_err(|_| {
                            DhcpError::new("T1 option has the wrong length")
                        })?))
                }
                59 => {
                    packet.options.t2 =
                        Some(u32::from_be_bytes(value.try_into().map_err(|_| {
                            DhcpError::new("T2 option has the wrong length")
                        })?))
                }
                61 => packet.options.client_id = Some(value.to_vec()),
                121 => option_121 = Some(parse_classless_routes(value)?),
                _ => {}
            }
        }
        packet.options.routes = option_121.or(option_33).unwrap_or_default();
        Ok(packet)
    }
}

fn parse_ip(value: &[u8], expected: usize, name: &str) -> Result<Option<Ipv4Addr>, DhcpError> {
    if value.len() != expected {
        return Err(DhcpError::new(format!(
            "{name} option has the wrong length"
        )));
    }
    Ok(Some(Ipv4Addr::new(value[0], value[1], value[2], value[3])))
}

fn parse_ips(value: &[u8], name: &str) -> Result<Vec<Ipv4Addr>, DhcpError> {
    if !value.len().is_multiple_of(4) {
        return Err(DhcpError::new(format!(
            "{name} option is not a list of IPv4 addresses"
        )));
    }
    Ok(value
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]))
        .collect())
}

fn parse_classless_routes(value: &[u8]) -> Result<Vec<(Ipv4Net, Ipv4Addr)>, DhcpError> {
    let mut routes = Vec::new();
    let mut offset = 0usize;
    while offset < value.len() {
        let width = value[offset] as usize;
        offset += 1;
        if width > 4 || offset + width + 4 > value.len() {
            return Err(DhcpError::new("classless route option is malformed"));
        }
        let mut bytes = [0u8; 4];
        bytes[..width].copy_from_slice(&value[offset..offset + width]);
        offset += width;
        let via = Ipv4Addr::new(
            value[offset],
            value[offset + 1],
            value[offset + 2],
            value[offset + 3],
        );
        offset += 4;
        routes.push((Ipv4Net::new(Ipv4Addr::from(bytes), (width * 8) as u8), via));
    }
    Ok(routes)
}

fn parse_classful_routes(value: &[u8]) -> Result<Vec<(Ipv4Net, Ipv4Addr)>, DhcpError> {
    if !value.len().is_multiple_of(8) {
        return Err(DhcpError::new("classful route option is malformed"));
    }
    Ok(value
        .as_chunks::<8>()
        .0
        .iter()
        .map(|chunk| {
            let destination = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
            let via = Ipv4Addr::new(chunk[4], chunk[5], chunk[6], chunk[7]);
            let prefix = match destination.octets()[0] {
                0..=127 => 8,
                128..=191 => 16,
                _ => 24,
            };
            (Ipv4Net::new(destination, prefix), via)
        })
        .collect())
}

fn push_option(out: &mut Vec<u8>, code: u8, value: &[u8]) {
    out.push(code);
    out.push(value.len() as u8);
    out.extend_from_slice(value);
}

/// A DHCP codec error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DhcpError {
    message: String,
}

impl DhcpError {
    /// Build an error.
    pub fn new(message: impl Into<String>) -> Self {
        DhcpError {
            message: message.into(),
        }
    }
}

impl fmt::Display for DhcpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DhcpError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_packets_round_trip() {
        let mut packet = Packet::client(
            MessageType::Discover,
            0x1234_5678,
            [2, 0, 0, 0, 0, 1],
            Ipv4Addr::UNSPECIFIED,
        );
        packet.options.hostname = Some("board".to_string());
        let bytes = packet.encode();
        let decoded = Packet::decode(&bytes).expect("decode");
        assert_eq!(decoded.options.message_type, Some(MessageType::Discover));
        assert_eq!(decoded.xid, 0x1234_5678);
        assert_eq!(decoded.mac(), Some([2, 0, 0, 0, 0, 1]));
        assert_eq!(decoded.options.hostname.as_deref(), Some("board"));
    }

    #[test]
    fn offer_and_ack_options_are_parsed() {
        let mut packet = Packet {
            op: 2,
            htype: 1,
            hlen: 6,
            hops: 0,
            xid: 42,
            secs: 0,
            flags: 0,
            ciaddr: Ipv4Addr::UNSPECIFIED,
            yiaddr: Ipv4Addr::new(192, 168, 1, 50),
            siaddr: Ipv4Addr::new(192, 168, 1, 1),
            giaddr: Ipv4Addr::UNSPECIFIED,
            chaddr: [2, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            options: Options {
                message_type: Some(MessageType::Ack),
                server_id: Some(Ipv4Addr::new(192, 168, 1, 1)),
                lease_time: Some(3600),
                t1: Some(1800),
                t2: Some(3150),
                subnet_mask: Some(Ipv4Addr::new(255, 255, 255, 0)),
                routers: vec![Ipv4Addr::new(192, 168, 1, 1)],
                dns: vec![Ipv4Addr::new(9, 9, 9, 9)],
                domain: Some("lan".to_string()),
                mtu: Some(1500),
                ..Options::default()
            },
        };
        packet.options.routes = vec![(
            "10.20.0.0/16".parse().expect("net"),
            Ipv4Addr::new(192, 168, 1, 1),
        )];
        let bytes = packet.encode();
        let decoded = Packet::decode(&bytes).expect("decode");
        assert_eq!(decoded.options.message_type, Some(MessageType::Ack));
        assert_eq!(decoded.yiaddr, Ipv4Addr::new(192, 168, 1, 50));
        assert_eq!(decoded.options.lease_time, Some(3600));
        assert_eq!(decoded.options.dns, vec![Ipv4Addr::new(9, 9, 9, 9)]);
        assert_eq!(decoded.options.routes.len(), 1);
        assert_eq!(decoded.options.routes[0].0.to_string(), "10.20.0.0/16");
    }

    #[test]
    fn arbitrary_bytes_do_not_panic() {
        let mut state = 0x9e37_79b9u32;
        for length in 0..512 {
            let mut buffer = vec![0u8; length];
            for byte in &mut buffer {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                *byte = (state >> 16) as u8;
            }
            let _ = Packet::decode(&buffer);
        }
    }

    #[test]
    fn malformed_packets_are_rejected() {
        assert!(Packet::decode(&[]).is_err());
        let mut packet = Packet::client(
            MessageType::Discover,
            1,
            [2, 0, 0, 0, 0, 1],
            Ipv4Addr::UNSPECIFIED,
        )
        .encode();
        packet[HEADER_LEN] = 0;
        assert!(Packet::decode(&packet).is_err());
    }

    #[test]
    fn malformed_known_options_are_rejected() {
        fn with_options(options: &[u8]) -> Vec<u8> {
            let mut packet = vec![0u8; HEADER_LEN];
            packet[2] = 6;
            packet.extend_from_slice(&MAGIC_COOKIE);
            packet.extend_from_slice(options);
            packet
        }

        let cases = [
            (&[53u8][..], "option length is missing"),
            (&[53, 1][..], "option value is truncated"),
            (&[3, 5, 192, 0, 2, 1, 0][..], "list of IPv4 addresses"),
            (&[26, 1, 0][..], "MTU option has the wrong length"),
            (
                &[121, 3, 24, 10, 0][..],
                "classless route option is malformed",
            ),
        ];
        for (options, expected) in cases {
            let error = Packet::decode(&with_options(options)).expect_err("malformed option");
            assert!(error.to_string().contains(expected), "{options:?}: {error}");
        }
    }

    #[test]
    fn an_oversized_hardware_address_is_rejected() {
        let mut packet = Packet::client(
            MessageType::Discover,
            1,
            [2, 0, 0, 0, 0, 1],
            Ipv4Addr::UNSPECIFIED,
        )
        .encode();
        packet[2] = 17;
        let error = Packet::decode(&packet).expect_err("hardware address length");
        assert!(error.to_string().contains("exceeds 16"), "{error}");
    }
}
