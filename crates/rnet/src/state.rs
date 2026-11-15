//! Desired/observed reconciliation and the `/run/rnet/state` record.
//!
//! `state` is deliberately small: it does not own DHCP leases and it does not
//! try to remove kernel state it did not add. It records the addresses, routes,
//! bridges and ports applied by rnet, so a reload or daemon restart can remove
//! exactly those objects.

use std::collections::BTreeMap;
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use crate::model::{Config, InterfaceKind, Ipv4Net, Route};
use crate::netlink::{Netlink, Observed};

/// Objects rnet applied and may need to remove later.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// Static addresses, by interface.
    pub addresses: Vec<(String, Ipv4Net)>,
    /// Static and default routes.
    pub routes: Vec<Route>,
    /// Bridges created by rnet.
    pub bridges: Vec<String>,
    /// Ports enslaved by rnet.
    pub ports: Vec<(String, String)>,
}

impl Applied {
    /// The desired state implied by `config`, filtered to links that are
    /// actually present. Bridges are kept because rnet creates them; a missing
    /// physical port is kept out until it appears.
    pub fn desired_for(config: &Config, observed: &Observed) -> Self {
        let mut applied = Applied::default();
        for iface in &config.interfaces {
            if !iface.enabled {
                continue;
            }
            let present = observed.links.contains_key(&iface.name);
            if iface.kind == InterfaceKind::Ethernet && !present {
                continue;
            }
            if iface.kind == InterfaceKind::Bridge {
                applied.bridges.push(iface.name.clone());
                for port in &iface.ports {
                    if observed.links.contains_key(port) {
                        applied.ports.push((iface.name.clone(), port.clone()));
                    }
                }
            }
            if !iface.dhcp4 {
                for address in &iface.addresses {
                    applied.addresses.push((iface.name.clone(), *address));
                }
                if let Some(gateway) = iface.gateway {
                    applied.routes.push(Route {
                        name: format!("{}-default", iface.name),
                        to: None,
                        via: Some(gateway),
                        dev: Some(iface.name.clone()),
                        metric: 0,
                        table: crate::model::RT_TABLE_MAIN,
                        src: None,
                        onlink: false,
                    });
                }
            }
        }
        applied.routes.extend(config.routes.iter().cloned());
        applied
    }

    /// The desired state implied by `config`, without presence filtering.
    pub fn desired(config: &Config) -> Self {
        let mut applied = Applied::default();
        for iface in &config.interfaces {
            if !iface.enabled {
                continue;
            }
            if iface.kind == InterfaceKind::Bridge {
                applied.bridges.push(iface.name.clone());
                for port in &iface.ports {
                    applied.ports.push((iface.name.clone(), port.clone()));
                }
            }
            if !iface.dhcp4 {
                for address in &iface.addresses {
                    applied.addresses.push((iface.name.clone(), *address));
                }
                if let Some(gateway) = iface.gateway {
                    applied.routes.push(Route {
                        name: format!("{}-default", iface.name),
                        to: None,
                        via: Some(gateway),
                        dev: Some(iface.name.clone()),
                        metric: 0,
                        table: crate::model::RT_TABLE_MAIN,
                        src: None,
                        onlink: false,
                    });
                }
            }
        }
        applied.routes.extend(config.routes.iter().cloned());
        applied
    }

    /// Read a state file; a missing file is an empty state.
    pub fn load(path: &Path) -> io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_text(&text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Applied::default()),
            Err(error) => Err(error),
        }
    }

    /// Write the state file atomically.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, self.to_text())?;
        std::fs::rename(&temporary, path)
    }

    /// Serialize the state.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for (iface, address) in &self.addresses {
            out.push_str(&format!("address {iface} {address}\n"));
        }
        for route in &self.routes {
            out.push_str(&format!(
                "route {} {} {} {} {} {} {} {}\n",
                route.name,
                route
                    .to
                    .map(|to| to.to_string())
                    .unwrap_or_else(|| "default".to_string()),
                route
                    .via
                    .map(|via| via.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                route.dev.as_deref().unwrap_or("-"),
                route.metric,
                route.table,
                route
                    .src
                    .map(|src| src.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                if route.onlink { "onlink" } else { "-" }
            ));
        }
        for bridge in &self.bridges {
            out.push_str(&format!("bridge {bridge}\n"));
        }
        for (bridge, port) in &self.ports {
            out.push_str(&format!("port {bridge} {port}\n"));
        }
        out
    }

    /// Parse the state.
    pub fn from_text(text: &str) -> io::Result<Self> {
        let mut applied = Applied::default();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            let bad = || {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("state line {}: malformed", index + 1),
                )
            };
            match fields.first().copied() {
                Some("address") if fields.len() == 3 => {
                    applied
                        .addresses
                        .push((fields[1].to_string(), fields[2].parse().map_err(|_| bad())?));
                }
                Some("route") if fields.len() == 9 => {
                    let to = if fields[2] == "default" {
                        None
                    } else {
                        Some(fields[2].parse().map_err(|_| bad())?)
                    };
                    let via = if fields[3] == "-" {
                        None
                    } else {
                        Some(fields[3].parse().map_err(|_| bad())?)
                    };
                    let dev = (fields[4] != "-").then(|| fields[4].to_string());
                    let metric = fields[5].parse().map_err(|_| bad())?;
                    let table = fields[6].parse().map_err(|_| bad())?;
                    let src = if fields[7] == "-" {
                        None
                    } else {
                        Some(fields[7].parse().map_err(|_| bad())?)
                    };
                    applied.routes.push(Route {
                        name: fields[1].to_string(),
                        to,
                        via,
                        dev,
                        metric,
                        table,
                        src,
                        onlink: fields[8] == "onlink",
                    });
                }
                Some("bridge") if fields.len() == 2 => {
                    applied.bridges.push(fields[1].to_string());
                }
                Some("port") if fields.len() == 3 => {
                    applied
                        .ports
                        .push((fields[1].to_string(), fields[2].to_string()));
                }
                _ => return Err(bad()),
            }
        }
        Ok(applied)
    }
}

/// Apply `config`, removing objects that were in `previous` but are no longer
/// desired. Returns the new applied state.
pub fn apply(config: &Config, observed: &Observed, previous: &Applied) -> io::Result<Applied> {
    let desired = Applied::desired_for(config, observed);
    let mut netlink = Netlink::open()?;

    if config.global.manage_loopback
        && let Some(link) = observed.links.get("lo")
        && !link.up
    {
        netlink.set_link_up("lo", true)?;
    }

    // Remove addresses and routes that rnet added before and no longer wants.
    for (iface, address) in &previous.addresses {
        if !desired.addresses.contains(&(iface.clone(), *address)) {
            ignore_missing(netlink.del_address(iface, *address))?;
        }
    }
    for route in &previous.routes {
        if !desired
            .routes
            .iter()
            .any(|candidate| same_route(candidate, route))
        {
            ignore_missing(netlink.del_route(route))?;
        }
    }
    for (bridge, port) in &previous.ports {
        if !desired.ports.contains(&(bridge.clone(), port.clone())) {
            ignore_missing(netlink.detach(port))?;
        }
    }
    for bridge in &previous.bridges {
        if !desired.bridges.contains(bridge)
            && observed.links.get(bridge).is_some_and(|link| link.bridge)
        {
            ignore_missing(netlink.delete_link(bridge))?;
        }
    }

    // Bring configured links into the desired state.
    for iface in &config.interfaces {
        if iface.kind == InterfaceKind::Ethernet && !observed.links.contains_key(&iface.name) {
            continue;
        }
        if iface.kind == InterfaceKind::Bridge {
            match observed.links.get(&iface.name) {
                None => {
                    ignore_existing(netlink.create_bridge(iface))?;
                }
                Some(link) if !link.bridge => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} exists but is not a bridge", iface.name),
                    ));
                }
                Some(_) => {}
            }
        }
        if !iface.enabled {
            ignore_missing(netlink.set_link_up(&iface.name, false))?;
            continue;
        }
        if let Some(mtu) = iface.mtu {
            netlink.set_mtu(&iface.name, mtu)?;
        }
        netlink.set_link_up(&iface.name, true)?;
    }

    // Add desired addresses/routes.
    for (iface, address) in &desired.addresses {
        ignore_existing(netlink.add_address(iface, *address))?;
    }
    for route in &desired.routes {
        ignore_existing(netlink.add_route(route))?;
    }

    // Ports after their bridge exists.
    for (bridge, port) in &desired.ports {
        if !observed.links.contains_key(port) {
            continue;
        }
        ignore_existing(netlink.enslave(bridge, port))?;
        netlink.set_link_up(port, true)?;
    }

    Ok(desired)
}

fn same_route(left: &Route, right: &Route) -> bool {
    left.to == right.to
        && left.via == right.via
        && left.dev == right.dev
        && left.metric == right.metric
        && left.table == right.table
        && left.src == right.src
        && left.onlink == right.onlink
}

fn ignore_existing(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        other => other,
    }
}

fn ignore_missing(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || matches!(
                    error.raw_os_error(),
                    Some(libc::ENOENT) | Some(libc::ESRCH) | Some(libc::EADDRNOTAVAIL)
                ) =>
        {
            Ok(())
        }
        other => other,
    }
}

/// A small wrapper that remembers the configuration and applied state.
#[derive(Debug)]
pub struct Reconciler {
    config: Config,
    applied: Applied,
    state_path: PathBuf,
}

impl Reconciler {
    /// Build a reconciler, loading the previous state from `state_path`.
    pub fn new(config: Config, state_path: impl Into<PathBuf>) -> io::Result<Self> {
        let state_path = state_path.into();
        let applied = Applied::load(&state_path)?;
        Ok(Reconciler {
            config,
            applied,
            state_path,
        })
    }

    /// The current configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The applied state.
    pub fn applied(&self) -> &Applied {
        &self.applied
    }

    /// Replace the configuration for the next apply.
    pub fn set_config(&mut self, config: Config) {
        self.config = config;
    }

    /// Apply the current configuration and persist the new state.
    pub fn apply(&mut self, observed: &Observed) -> io::Result<()> {
        let applied = apply(&self.config, observed, &self.applied)?;
        applied.save(&self.state_path)?;
        self.applied = applied;
        Ok(())
    }
}

/// The per-interface DHCP leases keyed by interface name.
pub type Leases = BTreeMap<String, crate::dhcp::lease::Lease>;

/// The default route used for a DHCP lease.
pub fn lease_route(iface: &str, gateway: Ipv4Addr) -> Route {
    Route {
        name: format!("{iface}-dhcp-default"),
        to: None,
        via: Some(gateway),
        dev: Some(iface.to_string()),
        metric: 0,
        table: crate::model::RT_TABLE_MAIN,
        src: None,
        onlink: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netlink::LinkInfo;

    fn config() -> Config {
        crate::config::parse(
            "[global]\nmanage-loopback = no\n\n[interface eth0]\ntype = ethernet\naddress = 192.0.2.10/24\n\n[interface br0]\ntype = bridge\nports = eth1\naddress = 192.0.2.20/24\n\n[route lab]\nto = 10.0.0.0/8\nvia = 192.0.2.1\ndev = eth0\n",
        )
        .expect("config")
    }

    #[test]
    fn applied_state_round_trips() {
        let applied = Applied::desired(&config());
        let text = applied.to_text();
        let parsed = Applied::from_text(&text).expect("parse");
        assert_eq!(parsed, applied);
    }

    #[test]
    fn desired_filters_missing_links_but_keeps_bridges() {
        let mut observed = Observed::default();
        observed.links.insert(
            "eth0".to_string(),
            LinkInfo {
                name: "eth0".to_string(),
                index: 2,
                up: true,
                mtu: 1500,
                master: None,
                bridge: false,
                ports: Vec::new(),
            },
        );
        let observed_config = config();
        let desired = Applied::desired_for(&observed_config, &observed);
        assert!(desired.addresses.iter().any(|(iface, _)| iface == "eth0"));
        assert!(desired.bridges.contains(&"br0".to_string()));
        assert!(desired.ports.is_empty());
    }

    #[test]
    fn same_route_ignores_the_section_name() {
        let left = Route {
            name: "a".to_string(),
            to: Some("10.0.0.0/8".parse().expect("net")),
            via: Some("192.0.2.1".parse().expect("ip")),
            dev: Some("eth0".to_string()),
            metric: 0,
            table: 254,
            src: None,
            onlink: false,
        };
        let mut right = left.clone();
        right.name = "b".to_string();
        assert!(same_route(&left, &right));
    }

    #[test]
    fn lease_route_is_a_default_route() {
        let route = lease_route("eth0", "192.0.2.1".parse().expect("ip"));
        assert!(route.to.is_none());
        assert_eq!(route.dev.as_deref(), Some("eth0"));
    }
}
