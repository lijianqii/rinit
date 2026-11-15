//! `rnetd` daemon loop.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::io::{self, Read, Write};
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config;
use crate::control::{Request, Response, decode_request, encode_response, frame, frame_len};
use crate::dhcp::client::{Action, Client};
use crate::dhcp::lease::Lease;
use crate::dhcp::packet::Packet;
use crate::dhcp::socket::{DhcpSocket, read_mac};
use crate::dns;
use crate::model::{Config, InterfaceKind, Route};
use crate::netlink::{LinkEvents, Netlink, observe};
use crate::state::{Leases, Reconciler, lease_route};

const CONTROL_MAX: usize = 64 * 1024;

/// Run the daemon.
pub fn run(
    config_path: &Path,
    control_path: &Path,
    state_path: &Path,
    leases_dir: &Path,
    resolv_path: &Path,
    once: bool,
) -> Result<(), String> {
    let config =
        config::load_effective(config_path).map_err(|e| format!("{config_path:?}: {e}"))?;
    let mut daemon = Daemon::new(
        config_path.to_path_buf(),
        control_path.to_path_buf(),
        state_path.to_path_buf(),
        leases_dir.to_path_buf(),
        resolv_path.to_path_buf(),
        config,
    )?;

    if once {
        daemon.apply().map_err(|e| e.to_string())?;
        for (key, value) in daemon.status_fields(None, true) {
            println!("{key}={value}");
        }
        return Ok(());
    }

    daemon.start_dhcp();
    daemon.apply().map_err(|e| e.to_string())?;
    daemon
        .open_control()
        .map_err(|e| format!("control socket: {e}"))?;
    daemon.notify_ready();
    daemon.event_loop()
}

struct DhcpRuntime {
    client: Client,
    socket: DhcpSocket,
}

struct Daemon {
    config_path: PathBuf,
    control_path: PathBuf,
    leases_dir: PathBuf,
    resolv_path: PathBuf,
    config: Config,
    reconciler: Reconciler,
    leases: Leases,
    dhcp: BTreeMap<String, DhcpRuntime>,
    listener: Option<UnixListener>,
    signals: Option<Signals>,
    link_events: Option<LinkEvents>,
    last_links: BTreeSet<String>,
    next_reconcile: Instant,
}

impl Daemon {
    fn new(
        config_path: PathBuf,
        control_path: PathBuf,
        state_path: PathBuf,
        leases_dir: PathBuf,
        resolv_path: PathBuf,
        config: Config,
    ) -> Result<Self, String> {
        let reconciler = Reconciler::new(config.clone(), state_path.clone())
            .map_err(|e| format!("state: {e}"))?;
        let leases = load_leases(&leases_dir);
        Ok(Daemon {
            config_path,
            control_path,
            leases_dir,
            resolv_path,
            config,
            reconciler,
            leases,
            dhcp: BTreeMap::new(),
            listener: None,
            signals: None,
            link_events: None,
            last_links: BTreeSet::new(),
            next_reconcile: Instant::now(),
        })
    }

    fn open_control(&mut self) -> io::Result<()> {
        if let Some(parent) = self.control_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::remove_file(&self.control_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(&self.control_path)?;
        listener.set_nonblocking(true)?;
        std::fs::set_permissions(&self.control_path, std::fs::Permissions::from_mode(0o660))?;
        match rnet_gid() {
            Some(gid) => {
                let path =
                    CString::new(self.control_path.as_os_str().as_bytes()).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidInput, "control path contains NUL")
                    })?;
                // SAFETY: path is a valid NUL-terminated string and gid comes
                // from the image's /etc/group.
                if unsafe { libc::chown(path.as_ptr(), 0, gid) } != 0 {
                    log(format!(
                        "control socket chown: {}",
                        io::Error::last_os_error()
                    ));
                }
            }
            None => log("rnet group not found; control socket stays root:root"),
        }
        self.listener = Some(listener);
        Ok(())
    }

    fn event_loop(&mut self) -> Result<(), String> {
        if self.listener.is_none() {
            self.open_control()
                .map_err(|e| format!("control socket: {e}"))?;
        }
        if self.signals.is_none() {
            self.signals = Some(Signals::new().map_err(|e| format!("signalfd: {e}"))?);
        }
        if self.link_events.is_none() {
            match LinkEvents::open() {
                Ok(events) => self.link_events = Some(events),
                Err(error) => log(format!("link events unavailable, polling: {error}")),
            }
        }
        loop {
            let now = Instant::now();
            let timeout = self.poll_timeout(now);
            let mut fds = Vec::new();
            let listener_fd = self
                .listener
                .as_ref()
                .map(|listener| listener.as_raw_fd())
                .unwrap_or(-1);
            if listener_fd >= 0 {
                fds.push(libc::pollfd {
                    fd: listener_fd,
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
            let signal_fd = self
                .signals
                .as_ref()
                .map(|signals| signals.as_raw_fd())
                .unwrap_or(-1);
            if signal_fd >= 0 {
                fds.push(libc::pollfd {
                    fd: signal_fd,
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
            let event_fd = self
                .link_events
                .as_ref()
                .map(LinkEvents::as_raw_fd)
                .unwrap_or(-1);
            if event_fd >= 0 {
                fds.push(libc::pollfd {
                    fd: event_fd,
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
            let mut dhcp_index = BTreeMap::new();
            for (name, runtime) in &self.dhcp {
                dhcp_index.insert(runtime.socket.as_raw_fd(), name.clone());
                fds.push(libc::pollfd {
                    fd: runtime.socket.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }

            // SAFETY: fds is a valid pollfd array for the duration of the call.
            let ready = unsafe {
                libc::poll(
                    fds.as_mut_ptr(),
                    fds.len() as libc::nfds_t,
                    timeout.as_millis().min(i32::MAX as u128) as libc::c_int,
                )
            };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(format!("poll: {error}"));
            }

            for descriptor in &fds {
                if descriptor.revents == 0 {
                    continue;
                }
                if descriptor.fd == listener_fd {
                    self.accept_control();
                } else if descriptor.fd == signal_fd {
                    if self.handle_signals()? {
                        return Ok(());
                    }
                } else if descriptor.fd == event_fd {
                    if let Some(events) = &self.link_events
                        && let Err(error) = events.drain()
                    {
                        log(format!("link event drain: {error}"));
                    }
                    self.reconcile_links_now();
                } else if let Some(name) = dhcp_index.get(&descriptor.fd).cloned() {
                    self.receive_dhcp(&name);
                }
            }
            self.tick_dhcp();
            self.reconcile_links();
        }
    }

    /// Fallback link check when `RTNLGRP_LINK` is unavailable.
    fn reconcile_links(&mut self) {
        if self.link_events.is_some() {
            return;
        }
        let now = Instant::now();
        if now < self.next_reconcile {
            return;
        }
        self.next_reconcile = now + Duration::from_secs(1);
        self.reconcile_links_now();
    }

    /// Re-read the link set and apply any change.
    fn reconcile_links_now(&mut self) {
        let Ok(observed) = observe() else {
            return;
        };
        let links: BTreeSet<String> = observed.links.keys().cloned().collect();
        if links == self.last_links {
            return;
        }
        self.last_links = links;
        if let Err(error) = self.reconciler.apply(&observed) {
            log(format!("link reconcile: {error}"));
        }
        self.write_dns();
    }

    fn poll_timeout(&self, now: Instant) -> Duration {
        let mut deadline: Option<Instant> = if self.link_events.is_none() {
            Some(self.next_reconcile)
        } else {
            None
        };
        for runtime in self.dhcp.values() {
            if let Some(next) = runtime.client.next_deadline(now) {
                deadline = Some(deadline.map_or(next, |current| current.min(next)));
            }
        }
        let timeout = deadline.map_or(Duration::from_secs(1), |deadline| {
            deadline.saturating_duration_since(now)
        });
        timeout
            .min(Duration::from_secs(1))
            .max(Duration::from_millis(10))
    }

    fn accept_control(&mut self) {
        let Some(listener) = self.listener.take() else {
            return;
        };
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
                    let _ = self.handle_client(stream);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    log(format!("control accept: {error}"));
                    break;
                }
            }
        }
        self.listener = Some(listener);
    }

    fn handle_client(&mut self, mut stream: UnixStream) -> io::Result<()> {
        let body = match read_frame(&mut stream) {
            Ok(body) => body,
            Err(error) => {
                let response = Response::err(2, format!("bad request: {error}"));
                let _ = write_frame(&mut stream, &encode_response(&response));
                return Ok(());
            }
        };
        let response = match decode_request(&body) {
            Ok(request) => self.dispatch(request),
            Err(error) => Response::err(2, error),
        };
        write_frame(&mut stream, &encode_response(&response))
    }

    fn dispatch(&mut self, request: Request) -> Response {
        match request {
            Request::Status { iface } => {
                Response::ok_with(self.status_fields(iface.as_deref(), false))
            }
            Request::Show { iface } => {
                if self.config.interface(&iface).is_none() {
                    return Response::err(2, format!("no such interface: {iface}"));
                }
                Response::ok_with(self.status_fields(Some(&iface), true))
            }
            Request::Reload => self.reload(),
            Request::Iface { iface, up } => {
                self.with_netlink(|netlink| netlink.set_link_up(&iface, up))
            }
            Request::Address {
                iface,
                address,
                add,
            } => self.with_netlink(|netlink| {
                if add {
                    netlink.add_address(&iface, address)
                } else {
                    netlink.del_address(&iface, address)
                }
            }),
            Request::Route { route, add } => self.with_netlink(|netlink| {
                if add {
                    netlink.add_route(&route)
                } else {
                    netlink.del_route(&route)
                }
            }),
            Request::BridgeCreate { bridge } => {
                self.with_netlink(|netlink| netlink.create_bridge(&bridge))
            }
            Request::BridgeDelete { name } => {
                self.with_netlink(|netlink| netlink.delete_link(&name))
            }
            Request::BridgePort { bridge, port, add } => self.with_netlink(|netlink| {
                if add {
                    netlink.enslave(&bridge, &port)
                } else {
                    netlink.detach(&port)
                }
            }),
            Request::Dhcp { iface, renew } => self.control_dhcp(&iface, renew),
        }
    }

    fn with_netlink(&self, operation: impl FnOnce(&mut Netlink) -> io::Result<()>) -> Response {
        match Netlink::open().and_then(|mut netlink| operation(&mut netlink)) {
            Ok(()) => Response::ok(),
            Err(error) => Response::err(1, error.to_string()),
        }
    }

    fn status_fields(&self, filter: Option<&str>, verbose: bool) -> Vec<(String, String)> {
        let observed = observe().unwrap_or_default();
        let mut fields = Vec::new();
        for (name, link) in &observed.links {
            if name == "lo" {
                continue;
            }
            if let Some(filter) = filter
                && filter != name
            {
                continue;
            }
            if let Some(iface) = self.config.interface(name) {
                fields.push(("iface".to_string(), name.clone()));
                fields.push(("configured".to_string(), "yes".to_string()));
                fields.push((
                    "kind".to_string(),
                    match iface.kind {
                        InterfaceKind::Ethernet => "ethernet",
                        InterfaceKind::Bridge => "bridge",
                    }
                    .to_string(),
                ));
                fields.push((
                    "state".to_string(),
                    if link.up { "up" } else { "down" }.to_string(),
                ));
                fields.push(("mtu".to_string(), link.mtu.to_string()));
                fields.push(("required".to_string(), yes_no(iface.required)));
                fields.push(("dhcp4".to_string(), yes_no(iface.dhcp4)));
                let has_address = !observed.addresses_of(name).is_empty();
                let has_default = observed
                    .routes
                    .iter()
                    .any(|route| route.to.is_none() && route.dev == *name);
                let needs_route = iface.dhcp4 || iface.gateway.is_some();
                fields.push((
                    "online".to_string(),
                    yes_no(link.up && has_address && (!needs_route || has_default)),
                ));
                for address in observed.addresses_of(name) {
                    fields.push(("address".to_string(), address.to_string()));
                }
                if let Some(master) = &link.master {
                    fields.push(("master".to_string(), master.clone()));
                }
                for port in &link.ports {
                    fields.push(("port".to_string(), port.clone()));
                }
                if let Some(lease) = self.leases.get(name) {
                    fields.push(("lease".to_string(), lease.network().to_string()));
                    fields.push(("lease_server".to_string(), lease.server.to_string()));
                    fields.push(("lease_time".to_string(), lease.lease_time.to_string()));
                }
                if verbose {
                    if let Some(gateway) = iface.gateway {
                        fields.push(("gateway".to_string(), gateway.to_string()));
                    }
                    for dns in &iface.dns {
                        fields.push(("dns".to_string(), dns.to_string()));
                    }
                    for route in self
                        .config
                        .routes
                        .iter()
                        .filter(|route| route.dev.as_deref() == Some(name))
                    {
                        fields.push(("route".to_string(), describe_route(route)));
                    }
                }
            } else {
                fields.push(("iface".to_string(), name.clone()));
                fields.push(("configured".to_string(), "no".to_string()));
                fields.push((
                    "state".to_string(),
                    if link.up { "up" } else { "down" }.to_string(),
                ));
            }
        }
        if let Some(filter) = filter
            && !fields
                .iter()
                .any(|(key, value)| key == "iface" && value == filter)
        {
            fields.push(("iface".to_string(), filter.to_string()));
            if let Some(iface) = self.config.interface(filter) {
                fields.push(("configured".to_string(), "yes".to_string()));
                fields.push((
                    "kind".to_string(),
                    match iface.kind {
                        InterfaceKind::Ethernet => "ethernet",
                        InterfaceKind::Bridge => "bridge",
                    }
                    .to_string(),
                ));
                fields.push(("state".to_string(), "missing".to_string()));
                fields.push(("required".to_string(), yes_no(iface.required)));
                fields.push(("dhcp4".to_string(), yes_no(iface.dhcp4)));
                fields.push(("online".to_string(), "no".to_string()));
                for address in &iface.addresses {
                    fields.push(("address".to_string(), address.to_string()));
                }
                if let Some(gateway) = iface.gateway {
                    fields.push(("gateway".to_string(), gateway.to_string()));
                }
            } else {
                fields.push(("configured".to_string(), "no".to_string()));
                fields.push(("state".to_string(), "missing".to_string()));
            }
        }
        fields
    }

    fn reload(&mut self) -> Response {
        let config = match config::load_effective(&self.config_path) {
            Ok(config) => config,
            Err(error) => return Response::err(2, error.to_string()),
        };
        self.config = config;
        self.reconciler.set_config(self.config.clone());
        self.restart_dhcp();
        match self.apply() {
            Ok(()) => Response::ok(),
            Err(error) => Response::err(1, error.to_string()),
        }
    }

    fn control_dhcp(&mut self, iface: &str, renew: bool) -> Response {
        let Some(runtime) = self.dhcp.get_mut(iface) else {
            return Response::err(2, format!("no DHCP client on {iface}"));
        };
        let now = Instant::now();
        let actions = if renew {
            runtime.client.start(now)
        } else {
            runtime.client.release()
        };
        self.execute_actions(iface, actions);
        Response::ok()
    }

    fn apply(&mut self) -> io::Result<()> {
        let observed = observe()?;
        self.reconciler.apply(&observed)?;
        self.last_links = observed.links.keys().cloned().collect();
        self.next_reconcile = Instant::now() + Duration::from_secs(1);
        self.write_dns();
        Ok(())
    }

    fn write_dns(&self) {
        if self.config.global.manage_resolv_conf
            && let Err(error) = dns::write(&self.resolv_path, &self.config, &self.leases)
        {
            log(format!("resolv.conf: {error}"));
        }
    }

    fn notify_ready(&self) {
        let Some(socket) = std::env::var_os("NOTIFY_SOCKET") else {
            return;
        };
        let path = PathBuf::from(socket);
        if path.as_os_str().to_string_lossy().starts_with('@') {
            log("abstract NOTIFY_SOCKET is not supported");
            return;
        }
        if let Ok(datagram) = UnixDatagram::unbound()
            && let Err(error) = datagram.send_to(b"READY=1", &path)
        {
            log(format!("notify READY: {error}"));
        }
    }

    fn start_dhcp(&mut self) {
        let names: Vec<String> = self
            .config
            .interfaces
            .iter()
            .filter(|iface| iface.dhcp4)
            .map(|iface| iface.name.clone())
            .collect();
        for name in names {
            self.add_dhcp(&name);
        }
    }

    fn restart_dhcp(&mut self) {
        let wanted: BTreeSet<String> = self
            .config
            .interfaces
            .iter()
            .filter(|iface| iface.dhcp4)
            .map(|iface| iface.name.clone())
            .collect();
        self.dhcp.retain(|name, _| wanted.contains(name));
        for name in wanted {
            self.add_dhcp(&name);
        }
    }

    fn add_dhcp(&mut self, iface: &str) {
        if self.dhcp.contains_key(iface) {
            return;
        }
        let mac = match read_mac(iface) {
            Ok(mac) => mac,
            Err(error) => {
                log(format!("{iface}: cannot read MAC: {error}"));
                return;
            }
        };
        let socket = match DhcpSocket::open(iface) {
            Ok(socket) => socket,
            Err(error) => {
                log(format!("{iface}: cannot open DHCP socket: {error}"));
                return;
            }
        };
        let client = Client::new(iface, mac).with_hostname(self.config.global.hostname.clone());
        self.dhcp
            .insert(iface.to_string(), DhcpRuntime { client, socket });
    }

    fn receive_dhcp(&mut self, iface: &str) {
        loop {
            let actions = {
                let Some(runtime) = self.dhcp.get_mut(iface) else {
                    return;
                };
                let mut buffer = [0u8; 1500];
                match runtime.socket.recv(&mut buffer) {
                    Ok(Some((packet, _source))) => {
                        let now = Instant::now();
                        match runtime.client.handle(&packet, now) {
                            Ok(actions) => actions,
                            Err(error) => {
                                log(format!("{iface}: DHCP packet rejected: {error}"));
                                Vec::new()
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        log(format!("{iface}: DHCP receive: {error}"));
                        break;
                    }
                }
            };
            if !actions.is_empty() {
                self.execute_actions(iface, actions);
            }
        }
    }

    fn tick_dhcp(&mut self) {
        let now = Instant::now();
        let names: Vec<String> = self.dhcp.keys().cloned().collect();
        for name in names {
            let actions = {
                let Some(runtime) = self.dhcp.get_mut(&name) else {
                    continue;
                };
                runtime.client.tick(now)
            };
            if !actions.is_empty() {
                self.execute_actions(&name, actions);
            }
        }
    }

    fn execute_actions(&mut self, iface: &str, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Send(packet) => {
                    let Some(runtime) = self.dhcp.get(iface) else {
                        continue;
                    };
                    let destination = send_destination(&packet);
                    if let Err(error) = runtime.socket.send(&packet, destination) {
                        log(format!("{iface}: DHCP send: {error}"));
                    }
                }
                Action::Lease(lease) => {
                    if let Err(error) = self.apply_lease(iface, &lease) {
                        log(format!("{iface}: apply lease: {error}"));
                    }
                    if let Err(error) = lease.write(&self.leases_dir.join(iface)) {
                        log(format!("{iface}: write lease: {error}"));
                    }
                    self.leases.insert(iface.to_string(), lease);
                    self.write_dns();
                }
                Action::Deconfigure => {
                    if let Err(error) = self.remove_lease(iface) {
                        log(format!("{iface}: remove lease: {error}"));
                    }
                    self.leases.remove(iface);
                    let _ = std::fs::remove_file(self.leases_dir.join(iface));
                    self.write_dns();
                }
            }
        }
    }

    fn apply_lease(&mut self, iface: &str, lease: &Lease) -> io::Result<()> {
        self.remove_lease_objects(iface)?;
        let mut netlink = Netlink::open()?;
        ignore_existing(netlink.add_address(iface, lease.network()))?;
        if let Some(router) = lease.routers.first().copied() {
            let default = lease_route(iface, router);
            ignore_existing(netlink.add_route(&default))?;
        }
        for (index, (to, via)) in lease.routes.iter().enumerate() {
            let route = Route {
                name: format!("{iface}-dhcp-{index}"),
                to: Some(*to),
                via: Some(*via),
                dev: Some(iface.to_string()),
                metric: 0,
                table: crate::model::RT_TABLE_MAIN,
                src: None,
                onlink: false,
            };
            ignore_existing(netlink.add_route(&route))?;
        }
        Ok(())
    }

    fn remove_lease(&mut self, iface: &str) -> io::Result<()> {
        self.remove_lease_objects(iface)
    }

    fn remove_lease_objects(&self, iface: &str) -> io::Result<()> {
        let Some(lease) = self.leases.get(iface) else {
            return Ok(());
        };
        let mut netlink = Netlink::open()?;
        ignore_missing(netlink.del_address(iface, lease.network()))?;
        if let Some(router) = lease.routers.first().copied() {
            ignore_missing(netlink.del_route(&lease_route(iface, router)))?;
        }
        for (index, (to, via)) in lease.routes.iter().enumerate() {
            let route = Route {
                name: format!("{iface}-dhcp-{index}"),
                to: Some(*to),
                via: Some(*via),
                dev: Some(iface.to_string()),
                metric: 0,
                table: crate::model::RT_TABLE_MAIN,
                src: None,
                onlink: false,
            };
            ignore_missing(netlink.del_route(&route))?;
        }
        Ok(())
    }

    fn handle_signals(&mut self) -> Result<bool, String> {
        let Some(signals) = &self.signals else {
            return Ok(false);
        };
        for signal in signals.read().map_err(|e| format!("signalfd: {e}"))? {
            match signal {
                libc::SIGHUP => {
                    let response = self.reload();
                    if !response.ok {
                        log(format!("reload: {}", response.message));
                    }
                }
                libc::SIGTERM | libc::SIGINT => return Ok(true),
                other => log(format!("ignoring signal {other}")),
            }
        }
        Ok(false)
    }
}

fn send_destination(packet: &Packet) -> Ipv4Addr {
    if !packet.ciaddr.is_unspecified()
        && let Some(server) = packet.options.server_id
    {
        return server;
    }
    Ipv4Addr::BROADCAST
}

fn yes_no(value: bool) -> String {
    if value { "yes" } else { "no" }.to_string()
}

fn describe_route(route: &Route) -> String {
    format!(
        "{} via {} dev {} metric {}",
        route
            .to
            .map(|to| to.to_string())
            .unwrap_or_else(|| "default".to_string()),
        route
            .via
            .map(|via| via.to_string())
            .unwrap_or_else(|| "-".to_string()),
        route.dev.as_deref().unwrap_or("-"),
        route.metric
    )
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

fn rnet_gid() -> Option<libc::gid_t> {
    let text = std::fs::read_to_string("/etc/group").ok()?;
    parse_group_gid(&text, "rnet")
}

fn parse_group_gid(text: &str, name: &str) -> Option<libc::gid_t> {
    for line in text.lines() {
        let mut fields = line.split(':');
        if fields.next() != Some(name) {
            continue;
        }
        let _password = fields.next()?;
        let gid = fields.next()?.parse::<libc::gid_t>().ok()?;
        return Some(gid);
    }
    None
}

fn load_leases(dir: &Path) -> Leases {
    let mut leases = Leases::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return leases;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Ok(lease) = Lease::read(&entry.path()) {
            leases.insert(name, lease);
        }
    }
    leases
}

fn read_frame(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut prefix = [0u8; 4];
    stream.read_exact(&mut prefix)?;
    let length = frame_len(prefix).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body)?;
    Ok(body)
}

fn write_frame(stream: &mut UnixStream, body: &[u8]) -> io::Result<()> {
    if body.len() > CONTROL_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "response is too large",
        ));
    }
    stream.write_all(&frame(body))?;
    stream.flush()
}

fn log(message: impl AsRef<str>) {
    eprintln!("rnetd: {}", message.as_ref());
}

/// A signalfd wrapper for TERM/INT/HUP.
struct Signals {
    fd: OwnedFd,
}

impl Signals {
    fn new() -> io::Result<Self> {
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: set is valid storage for the sigset helpers.
        unsafe {
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGTERM);
            libc::sigaddset(&mut set, libc::SIGINT);
            libc::sigaddset(&mut set, libc::SIGHUP);
            if libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // SAFETY: set is a valid signal mask.
        let fd = unsafe { libc::signalfd(-1, &set, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is fresh and owned from here on.
        Ok(Signals {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    fn as_raw_fd(&self) -> libc::c_int {
        self.fd.as_raw_fd()
    }

    fn read(&self) -> io::Result<Vec<libc::c_int>> {
        let mut signals = Vec::new();
        let mut buffer = [0u8; 128];
        loop {
            // SAFETY: buffer is valid writable storage.
            let read = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if read < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                return Err(error);
            }
            if read == 0 {
                break;
            }
            if read >= 4 {
                signals.push(i32::from_ne_bytes([
                    buffer[0], buffer[1], buffer[2], buffer[3],
                ]));
            }
        }
        Ok(signals)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dhcp::packet::{MessageType, Packet};
    use crate::model::RT_TABLE_MAIN;

    #[test]
    fn dhcp_destination_is_broadcast_or_unicast() {
        let mut discover = Packet::client(
            MessageType::Discover,
            1,
            [2, 0, 0, 0, 0, 1],
            Ipv4Addr::UNSPECIFIED,
        );
        assert_eq!(send_destination(&discover), Ipv4Addr::BROADCAST);
        discover.ciaddr = Ipv4Addr::new(192, 0, 2, 10);
        discover.options.server_id = Some(Ipv4Addr::new(192, 0, 2, 1));
        assert_eq!(send_destination(&discover), Ipv4Addr::new(192, 0, 2, 1));
    }

    #[test]
    fn route_description_names_the_destination() {
        let route = Route {
            name: "lab".to_string(),
            to: Some("10.0.0.0/8".parse().expect("net")),
            via: Some("192.0.2.1".parse().expect("ip")),
            dev: Some("eth0".to_string()),
            metric: 100,
            table: RT_TABLE_MAIN,
            src: None,
            onlink: false,
        };
        let text = describe_route(&route);
        assert!(text.contains("10.0.0.0/8"));
        assert!(text.contains("192.0.2.1"));
    }

    #[test]
    fn missing_state_is_tolerated() {
        let missing = io::Error::from_raw_os_error(libc::ESRCH);
        assert!(ignore_missing(Err(missing)).is_ok());
        let exists = io::Error::from_raw_os_error(libc::EEXIST);
        assert!(ignore_existing(Err(exists)).is_ok());
        let other = io::Error::from_raw_os_error(libc::EINVAL);
        assert!(ignore_missing(Err(other)).is_err());
    }

    #[test]
    fn leases_load_from_the_directory() {
        let root = std::env::temp_dir().join(format!("rnet-runtime-leases-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp");
        let lease = Lease {
            address: Ipv4Addr::new(192, 0, 2, 10),
            prefix: 24,
            server: Ipv4Addr::new(192, 0, 2, 1),
            lease_time: 3600,
            t1: 1800,
            t2: 3150,
            routers: vec![Ipv4Addr::new(192, 0, 2, 1)],
            dns: Vec::new(),
            domain: None,
            routes: Vec::new(),
        };
        lease.write(&root.join("eth0")).expect("write");
        let leases = load_leases(&root);
        assert_eq!(leases.get("eth0"), Some(&lease));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn control_socket_is_group_writable_only() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("rnet-runtime-socket-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp");
        let mut daemon = Daemon::new(
            root.join("config"),
            root.join("control.sock"),
            root.join("state"),
            root.join("leases"),
            root.join("resolv.conf"),
            Config::default(),
        )
        .expect("daemon");
        daemon.open_control().expect("control socket");
        let mode = std::fs::metadata(root.join("control.sock"))
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o660);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn group_gid_is_parsed_from_the_local_file() {
        assert_eq!(
            parse_group_gid("root:x:0:\nrnet:x:1000:\n", "rnet"),
            Some(1000)
        );
        assert_eq!(parse_group_gid("root:x:0:\n", "rnet"), None);
    }

    #[test]
    fn yes_no_is_stable() {
        assert_eq!(yes_no(true), "yes");
        assert_eq!(yes_no(false), "no");
    }
}
