//! Configuration data model for rnet.
//!
//! The model is deliberately IPv4-only for the first version. The parser in
//! [`crate::config`] rejects IPv6 literals rather than silently ignoring them,
//! so an operator who writes an IPv6 address gets a useful error instead of a
//! half-applied configuration.

use std::fmt;
use std::net::Ipv4Addr;
use std::str::FromStr;

/// Linux's `RT_TABLE_MAIN`.
pub const RT_TABLE_MAIN: u32 = 254;

/// An IPv4 address together with its prefix length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ipv4Net {
    /// The address as written by the operator.
    pub addr: Ipv4Addr,
    /// Prefix length, 0..=32.
    pub prefix: u8,
}

impl Ipv4Net {
    /// Build a network from an address and prefix.
    pub const fn new(addr: Ipv4Addr, prefix: u8) -> Self {
        Ipv4Net { addr, prefix }
    }

    /// The network address (host bits cleared).
    pub fn network(self) -> Ipv4Addr {
        if self.prefix == 0 {
            return Ipv4Addr::UNSPECIFIED;
        }
        let bits = u32::from(self.addr);
        let mask = u32::MAX << (32 - self.prefix);
        Ipv4Addr::from(bits & mask)
    }

    /// The netmask as an IPv4 address.
    pub fn netmask(self) -> Ipv4Addr {
        if self.prefix == 0 {
            return Ipv4Addr::UNSPECIFIED;
        }
        Ipv4Addr::from(u32::MAX << (32 - self.prefix))
    }

    /// Whether `address` lies inside this network.
    pub fn contains(self, address: Ipv4Addr) -> bool {
        let mask = u32::from(self.netmask());
        u32::from(address) & mask == u32::from(self.network())
    }
}

impl fmt::Display for Ipv4Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

impl FromStr for Ipv4Net {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.contains(':') {
            return Err("IPv6 is not supported in this version".to_string());
        }
        let (addr, prefix) = value
            .split_once('/')
            .ok_or_else(|| "expected an IPv4 address with a /prefix".to_string())?;
        let addr =
            Ipv4Addr::from_str(addr).map_err(|_| format!("'{addr}' is not an IPv4 address"))?;
        let prefix = prefix
            .parse::<u8>()
            .map_err(|_| format!("'{prefix}' is not a prefix length"))?;
        if prefix > 32 {
            return Err(format!("prefix {prefix} is outside 0..=32"));
        }
        Ok(Ipv4Net { addr, prefix })
    }
}

/// What kind of link an interface section describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterfaceKind {
    /// A physical or otherwise pre-existing Ethernet link.
    Ethernet,
    /// A bridge created and owned by rnet.
    Bridge,
}

impl InterfaceKind {
    /// Parse the `type` value.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "ethernet" => Ok(InterfaceKind::Ethernet),
            "bridge" => Ok(InterfaceKind::Bridge),
            other => Err(format!("unknown interface type '{other}'")),
        }
    }
}

/// The policy `rncli wait-online` applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOnline {
    /// At least one eligible interface must be online.
    Any,
    /// Every eligible interface must be online.
    All,
    /// Do not wait.
    None,
}

impl WaitOnline {
    /// Parse the `wait-online` value.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "any" => Ok(WaitOnline::Any),
            "all" => Ok(WaitOnline::All),
            "none" => Ok(WaitOnline::None),
            other => Err(format!("unknown wait-online policy '{other}'")),
        }
    }
}

/// Global settings from `[global]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Global {
    /// Write `/etc/resolv.conf` from static and DHCP DNS.
    pub manage_resolv_conf: bool,
    /// Ensure `lo` is up.
    pub manage_loopback: bool,
    /// What `wait-online` requires.
    pub wait_online: WaitOnline,
    /// Default `wait-online` timeout, in seconds.
    pub wait_online_timeout: u64,
    /// How long a DHCP client keeps retrying; zero means forever.
    pub dhcp_timeout: u64,
    /// Optional DHCP hostname (option 12).
    pub hostname: Option<String>,
    /// Search domains for `/etc/resolv.conf`.
    pub search: Vec<String>,
    /// Global DNS servers.
    pub dns: Vec<Ipv4Addr>,
}

impl Default for Global {
    fn default() -> Self {
        Global {
            manage_resolv_conf: true,
            manage_loopback: true,
            wait_online: WaitOnline::Any,
            wait_online_timeout: 30,
            dhcp_timeout: 0,
            hostname: None,
            search: Vec::new(),
            dns: Vec::new(),
        }
    }
}

/// One `[interface NAME]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    /// Linux interface name.
    pub name: String,
    /// Ethernet or bridge.
    pub kind: InterfaceKind,
    /// Whether rnet manages the link state.
    pub enabled: bool,
    /// Whether `wait-online` treats a missing link as a failure.
    pub required: bool,
    /// Run the built-in DHCPv4 client.
    pub dhcp4: bool,
    /// Static IPv4 addresses.
    pub addresses: Vec<Ipv4Net>,
    /// Default IPv4 gateway.
    pub gateway: Option<Ipv4Addr>,
    /// Extra DNS servers for this interface.
    pub dns: Vec<Ipv4Addr>,
    /// Optional MTU.
    pub mtu: Option<u32>,
    /// Bridge ports; empty for a non-bridge.
    pub ports: Vec<String>,
    /// Bridge STP state.
    pub stp: bool,
    /// Bridge forwarding delay, in seconds.
    pub forward_delay: u32,
    /// Bridge hello time, in seconds.
    pub hello_time: u32,
    /// Bridge max age, in seconds.
    pub max_age: u32,
    /// Per-interface wait-online override.
    pub wait_online: Option<bool>,
}

impl Interface {
    /// A new interface with the documented defaults.
    pub fn new(name: String, kind: InterfaceKind) -> Self {
        Interface {
            name,
            kind,
            enabled: true,
            required: false,
            dhcp4: false,
            addresses: Vec::new(),
            gateway: None,
            dns: Vec::new(),
            mtu: None,
            ports: Vec::new(),
            stp: false,
            forward_delay: 15,
            hello_time: 2,
            max_age: 20,
            wait_online: None,
        }
    }

    /// Whether `wait-online` should consider this interface.
    pub fn wait_online_enabled(&self, global: &Global) -> bool {
        self.wait_online
            .unwrap_or(global.wait_online != WaitOnline::None)
    }
}

/// One `[route NAME]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// Unique section name, used in diagnostics and state.
    pub name: String,
    /// Destination; `None` is the default route.
    pub to: Option<Ipv4Net>,
    /// Gateway.
    pub via: Option<Ipv4Addr>,
    /// Output interface.
    pub dev: Option<String>,
    /// Route metric.
    pub metric: u32,
    /// Routing table; defaults to `main` (254).
    pub table: u32,
    /// Preferred source address.
    pub src: Option<Ipv4Addr>,
    /// Treat the gateway as on-link.
    pub onlink: bool,
}

impl Route {
    /// A new route with the documented defaults.
    pub fn new(name: String) -> Self {
        Route {
            name,
            to: None,
            via: None,
            dev: None,
            metric: 0,
            table: RT_TABLE_MAIN,
            src: None,
            onlink: false,
        }
    }

    /// The destination prefix length (zero for the default route).
    pub fn prefix(&self) -> u8 {
        self.to.map_or(0, |net| net.prefix)
    }

    /// The destination address (unspecified for the default route).
    pub fn destination(&self) -> Ipv4Addr {
        self.to.map_or(Ipv4Addr::UNSPECIFIED, |net| net.network())
    }
}

/// The complete parsed configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    /// `[global]`.
    pub global: Global,
    /// `[interface]` sections, in file order.
    pub interfaces: Vec<Interface>,
    /// `[route]` sections, in file order.
    pub routes: Vec<Route>,
}

impl Config {
    /// Find an interface by name.
    pub fn interface(&self, name: &str) -> Option<&Interface> {
        self.interfaces.iter().find(|iface| iface.name == name)
    }

    /// Find an interface by name, mutably.
    pub fn interface_mut(&mut self, name: &str) -> Option<&mut Interface> {
        self.interfaces.iter_mut().find(|iface| iface.name == name)
    }

    /// Find a route by section name.
    pub fn route(&self, name: &str) -> Option<&Route> {
        self.routes.iter().find(|route| route.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_net_parses_and_computes_network() {
        let net: Ipv4Net = "192.168.10.42/24".parse().expect("net");
        assert_eq!(net.addr, Ipv4Addr::new(192, 168, 10, 42));
        assert_eq!(net.network(), Ipv4Addr::new(192, 168, 10, 0));
        assert_eq!(net.netmask(), Ipv4Addr::new(255, 255, 255, 0));
        assert!(net.contains(Ipv4Addr::new(192, 168, 10, 99)));
        assert!(!net.contains(Ipv4Addr::new(192, 168, 11, 1)));
    }

    #[test]
    fn ipv6_and_bad_prefixes_are_rejected() {
        assert!("fd00::1/64".parse::<Ipv4Net>().is_err());
        assert!("192.0.2.1".parse::<Ipv4Net>().is_err());
        assert!("192.0.2.1/33".parse::<Ipv4Net>().is_err());
    }

    #[test]
    fn interface_and_route_helpers_are_consistent() {
        let iface = Interface::new("eth0".to_string(), InterfaceKind::Ethernet);
        let global = Global::default();
        assert!(iface.wait_online_enabled(&global));
        let mut route = Route::new("lab".to_string());
        assert_eq!(route.prefix(), 0);
        assert_eq!(route.destination(), Ipv4Addr::UNSPECIFIED);
        route.to = Some("10.0.0.0/8".parse().expect("net"));
        assert_eq!(route.prefix(), 8);
        assert_eq!(route.destination(), Ipv4Addr::new(10, 0, 0, 0));
    }

    #[test]
    fn wait_online_policy_parses() {
        assert_eq!(WaitOnline::parse("any"), Ok(WaitOnline::Any));
        assert_eq!(WaitOnline::parse("all"), Ok(WaitOnline::All));
        assert_eq!(WaitOnline::parse("none"), Ok(WaitOnline::None));
        assert!(WaitOnline::parse("some").is_err());
    }
}
