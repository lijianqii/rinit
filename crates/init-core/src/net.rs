//! Network interface configuration via ioctl and native DHCP.
//!
//! Two modes:
//! - Static: ioctl SIOCSIFADDR/SIOCSIFNETMASK + route + resolv.conf
//! - DHCP: native UDP-based DHCP client (no external udhcpc needed)

use anyhow::{Context, Result};
use std::net::UdpSocket;
use std::os::unix::io::AsRawFd;
use tracing::{debug, warn};

/// DHCP lease information returned by a successful negotiation.
#[derive(Debug, Clone)]
pub struct DhcpLease {
    pub ip: u32,
    pub netmask: u32,
    pub gateway: Option<u32>,
    pub dns: Vec<u32>,
}
const DHCP_MAGIC: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
const DHCP_OPT_DISCOVER: u8 = 1;
const DHCP_OPT_REQUEST: u8 = 3;
const DHCP_OPT_SUBNET: u8 = 1;
const DHCP_OPT_ROUTER: u8 = 3;
const DHCP_OPT_DNS: u8 = 6;
const DHCP_OPT_MSG_TYPE: u8 = 53;
const DHCP_OPT_SERVER_ID: u8 = 54;
const DHCP_OPT_REQ_LIST: u8 = 55;
const DHCP_OPT_END: u8 = 255;

/// Bring a network interface up (IFF_UP | IFF_RUNNING).
fn ifup(ifname: &str) -> Result<libc::c_int> {
    let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if sock < 0 {
        return Err(std::io::Error::last_os_error()).context("socket() for ioctl");
    }
    let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
    let ifname_bytes = ifname.as_bytes();
    let copy_len = ifname_bytes.len().min(libc::IFNAMSIZ - 1);
    for (i, &b) in ifname_bytes[..copy_len].iter().enumerate() {
        ifr.ifr_name[i] = b as libc::c_char;
    }
    unsafe { ioctl(sock, libc::SIOCGIFFLAGS, &mut ifr) };
    let flags = unsafe { ifr.ifr_ifru.ifru_flags } as i16;
    ifr.ifr_ifru.ifru_flags = flags | libc::IFF_UP as i16 | libc::IFF_RUNNING as i16;
    unsafe { ioctl(sock, libc::SIOCSIFFLAGS, &mut ifr) };
    debug!(ifname, "interface brought up");
    Ok(sock)
}

/// Configure static IP on an interface.
pub fn configure_static(
    ifname: &str,
    address: &str,
    gateway: Option<&str>,
    dns: &[String],
) -> Result<()> {
    let (addr_str, prefix_len) = parse_cidr(address)?;
    let sock = ifup(ifname)?;
    let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
    let b = ifname.as_bytes();
    let n = b.len().min(libc::IFNAMSIZ - 1);
    for (i, &c) in b[..n].iter().enumerate() {
        ifr.ifr_name[i] = c as libc::c_char;
    }
    let addr = parse_ipv4(addr_str)?;
    unsafe { set_sin(&mut ifr.ifr_ifru.ifru_addr, addr, 0) };
    unsafe { ioctl(sock, libc::SIOCSIFADDR, &mut ifr) };
    let mask = prefix_to_netmask(prefix_len);
    unsafe { set_sin(&mut ifr.ifr_ifru.ifru_addr, mask, 0) };
    unsafe { ioctl(sock, libc::SIOCSIFNETMASK, &mut ifr) };
    unsafe { libc::close(sock) };
    debug!(ifname, addr=%addr_str, "static IP");
    if let Some(gw) = gateway {
        add_default_route(gw)?;
    }
    if !dns.is_empty() {
        write_resolv_conf_str(dns)?;
    }
    Ok(())
}

/// Run a native DHCP client on the given interface.
/// Blocks until a lease is obtained, then configures the interface.
pub fn run_dhcp(ifname: &str) -> Result<DhcpLease> {
    let _sock = ifup(ifname)?;
    unsafe { libc::close(_sock) };

    // Create UDP socket bound to port 68
    let socket = UdpSocket::bind("0.0.0.0:68").context("bind DHCP socket")?;
    socket.set_broadcast(true).context("set broadcast")?;

    // Bind to specific interface
    let ifname_c = std::ffi::CString::new(ifname).unwrap();
    let ret = unsafe {
        libc::setsockopt(
            socket.as_raw_fd() as libc::c_int,
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            ifname_c.as_ptr() as *const libc::c_void,
            ifname.len() as libc::socklen_t,
        )
    };
    if ret < 0 {
        return Err(std::io::Error::last_os_error()).context("SO_BINDTODEVICE");
    }

    socket
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .ok();
    socket
        .set_write_timeout(Some(std::time::Duration::from_secs(1)))
        .ok();

    // Generate transaction ID from PID + time
    let xid: u32 = (unsafe { libc::getpid() } as u32)
        ^ (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as u32);

    // Build and send DHCPDISCOVER
    debug!(ifname, xid, "DHCP DISCOVER");
    let mac = get_iface_mac(ifname).unwrap_or([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
    let discover = build_dhcp_packet(1, xid, &mac, DHCP_OPT_DISCOVER, None);
    socket.send_to(&discover, "255.255.255.255:67")?;

    // Receive DHCPOFFER
    let mut buf = [0u8; 1500];
    let (len, _) = socket.recv_from(&mut buf)?;
    let offer = parse_dhcp_packet(&buf[..len])?;
    let server_id = offer
        .server_id
        .with_context(|| "DHCPOFFER missing server-id")?;
    let yiaddr = u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]);
    debug!(ifname, addr=%fmt_ip(yiaddr), "DHCP OFFER");

    // Send DHCPREQUEST
    let request = build_dhcp_packet(1, xid, &mac, DHCP_OPT_REQUEST, Some(&server_id));
    socket.send_to(&request, "255.255.255.255:67")?;

    // Receive DHCPACK
    let (len, _) = socket.recv_from(&mut buf)?;
    let ack = parse_dhcp_packet(&buf[..len])?;
    debug!(ifname, "DHCP ACK");

    // Extract lease
    let ip = u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]);
    let netmask = ack.subnet_mask.unwrap_or(0xFFFFFF00);
    let gateway = ack.router;
    let dns = ack.dns;

    // Apply configuration to interface
    apply_lease(ifname, ip, netmask, gateway, &dns)?;

    Ok(DhcpLease {
        ip,
        netmask,
        gateway,
        dns,
    })
}

// ---- DHCP packet building ----

fn build_dhcp_packet(
    op: u8,
    xid: u32,
    chaddr: &[u8; 6],
    msg_type: u8,
    server_id: Option<&[u8; 4]>,
) -> Vec<u8> {
    let mut pkt = vec![0u8; 240];
    pkt[0] = op; // op
    pkt[1] = 1; // htype: Ethernet
    pkt[2] = 6; // hlen
    pkt[4..8].copy_from_slice(&xid.to_be_bytes());
    pkt[10..12].copy_from_slice(&0x8000u16.to_be_bytes()); // flags: broadcast
    pkt[28..34].copy_from_slice(chaddr);

    // Magic cookie
    pkt[236..240].copy_from_slice(&DHCP_MAGIC);

    // Options
    let mut opt = vec![DHCP_OPT_MSG_TYPE, 1, msg_type];
    // Parameter request list
    opt.extend_from_slice(&[
        DHCP_OPT_REQ_LIST,
        4,
        DHCP_OPT_SUBNET,
        DHCP_OPT_ROUTER,
        DHCP_OPT_DNS,
        6,
    ]);
    // Server identifier for REQUEST
    if let Some(sid) = server_id {
        opt.extend_from_slice(&[DHCP_OPT_SERVER_ID, 4]);
        opt.extend_from_slice(sid);
    }
    opt.push(DHCP_OPT_END);

    pkt.extend_from_slice(&opt);
    pkt.resize(std::cmp::max(pkt.len(), 300), 0);
    pkt
}

struct ParsedDhcp {
    server_id: Option<[u8; 4]>,
    subnet_mask: Option<u32>,
    router: Option<u32>,
    dns: Vec<u32>,
}

fn parse_dhcp_packet(data: &[u8]) -> Result<ParsedDhcp> {
    if data.len() < 240 {
        anyhow::bail!("DHCP packet too short");
    }
    let mut r = ParsedDhcp {
        server_id: None,
        subnet_mask: None,
        router: None,
        dns: vec![],
    };
    let opts_start = data.iter().position(|&b| b == 0x63).unwrap_or(240);
    let opts = &data[opts_start + 4..]; // skip magic
    let mut i = 0;
    while i < opts.len() {
        let code = opts[i];
        if code == 255 {
            break;
        }
        if i + 1 >= opts.len() {
            break;
        }
        let len = opts[i + 1] as usize;
        if i + 2 + len > opts.len() {
            break;
        }
        let val = &opts[i + 2..i + 2 + len];
        match code {
            DHCP_OPT_SERVER_ID if len == 4 => {
                r.server_id = Some([val[0], val[1], val[2], val[3]]);
            }
            DHCP_OPT_SUBNET if len == 4 => {
                r.subnet_mask = Some(u32::from_be_bytes([val[0], val[1], val[2], val[3]]));
            }
            DHCP_OPT_ROUTER if len >= 4 => {
                r.router = Some(u32::from_be_bytes([val[0], val[1], val[2], val[3]]));
            }
            DHCP_OPT_DNS => {
                for chunk in val.chunks(4) {
                    if chunk.len() == 4 {
                        r.dns
                            .push(u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
                    }
                }
            }
            _ => {}
        }
        i += 2 + len;
    }
    Ok(r)
}

fn get_iface_mac(ifname: &str) -> Result<[u8; 6]> {
    let path = format!("/sys/class/net/{}/address", ifname);
    let addr = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path))?
        .trim()
        .to_string();
    let parts: Vec<&str> = addr.split(':').collect();
    anyhow::ensure!(parts.len() == 6, "invalid MAC");
    let mut mac = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        mac[i] = u8::from_str_radix(p, 16)?;
    }
    Ok(mac)
}

fn apply_lease(
    ifname: &str,
    ip: u32,
    netmask: u32,
    gateway: Option<u32>,
    dns: &[u32],
) -> Result<()> {
    let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if sock < 0 {
        return Err(std::io::Error::last_os_error()).context("socket");
    }

    let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
    let b = ifname.as_bytes();
    let n = b.len().min(libc::IFNAMSIZ - 1);
    for (i, &c) in b[..n].iter().enumerate() {
        ifr.ifr_name[i] = c as libc::c_char;
    }

    unsafe { set_sin(&mut ifr.ifr_ifru.ifru_addr, ip, 0) };
    unsafe { ioctl(sock, libc::SIOCSIFADDR, &mut ifr) };
    unsafe { set_sin(&mut ifr.ifr_ifru.ifru_addr, netmask, 0) };
    unsafe { ioctl(sock, libc::SIOCSIFNETMASK, &mut ifr) };
    unsafe { libc::close(sock) };

    debug!(ifname, addr=%fmt_ip(ip), mask=%fmt_ip(netmask), "DHCP lease applied");

    if let Some(gw) = gateway {
        add_default_route(&fmt_ip(gw))?;
    }

    if !dns.is_empty() {
        let servers: Vec<String> = dns.iter().map(|d| fmt_ip(*d)).collect();
        write_resolv_conf_str(&servers)?;
    }
    Ok(())
}

// ---- helpers ----

fn fmt_ip(addr: u32) -> String {
    let b = addr.to_be_bytes();
    format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
}

fn parse_cidr(input: &str) -> Result<(&str, u8)> {
    if let Some((a, p)) = input.split_once("/") {
        let p: u8 = p.parse()?;
        anyhow::ensure!(p <= 32, "CIDR prefix > 32");
        Ok((a, p))
    } else {
        Ok((input, 24))
    }
}

fn parse_ipv4(s: &str) -> Result<u32> {
    let octets: Vec<u32> = s
        .split('.')
        .map(|o| o.parse::<u32>())
        .collect::<Result<_, _>>()?;
    anyhow::ensure!(
        octets.len() == 4 && octets.iter().all(|&o| o <= 255),
        "bad IPv4"
    );
    Ok((octets[0] << 24) | (octets[1] << 16) | (octets[2] << 8) | octets[3])
}

fn prefix_to_netmask(p: u8) -> u32 {
    if p == 0 {
        0
    } else {
        !0u32 << (32 - p)
    }
}

unsafe fn set_sin(sa: &mut libc::sockaddr, addr: u32, port: u16) {
    let sin = sa as *mut _ as *mut libc::sockaddr_in;
    (*sin).sin_family = libc::AF_INET as u16;
    (*sin).sin_port = port.to_be();
    (*sin).sin_addr.s_addr = addr.to_be();
}

unsafe fn ioctl(fd: libc::c_int, req: libc::c_ulong, ifr: &mut libc::ifreq) {
    let ret = libc::ioctl(fd, req as _, ifr as *mut _);
    if ret < 0 {
        warn!("ioctl err: {}", std::io::Error::last_os_error());
    }
}

fn add_default_route(gw: &str) -> Result<()> {
    let gw = parse_ipv4(gw)?;
    let mut rt: libc::rtentry = unsafe { std::mem::zeroed() };
    unsafe { set_sin(&mut rt.rt_dst, 0, 0) };
    unsafe { set_sin(&mut rt.rt_gateway, gw, 0) };
    unsafe { set_sin(&mut rt.rt_genmask, 0, 0) };
    rt.rt_flags = (libc::RTF_UP | libc::RTF_GATEWAY) as u16;
    let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    anyhow::ensure!(sock >= 0, "socket failed");
    let ret = unsafe { libc::ioctl(sock, libc::SIOCADDRT as _, &rt as *const _) };
    unsafe { libc::close(sock) };
    if ret < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
        return Err(std::io::Error::last_os_error()).context("SIOCADDRT");
    }
    Ok(())
}

fn write_resolv_conf_str(servers: &[String]) -> Result<()> {
    let mut f = std::fs::File::create("/etc/resolv.conf").context("resolv.conf")?;
    for ns in servers {
        use std::io::Write;
        writeln!(f, "nameserver {}", ns)?;
    }
    Ok(())
}
