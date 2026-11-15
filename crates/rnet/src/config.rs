//! Strict configuration parser for `/etc/rnet/config`.
//!
//! The syntax is intentionally small: INI sections, `key = value`, `#`
//! comments, `\` continuation, and whitespace-separated lists. There is no
//! shell, no environment expansion, and no implicit IPv6 fallback: an IPv6
//! literal is an error in this version.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::model::{Config, Interface, InterfaceKind, Ipv4Net, RT_TABLE_MAIN, Route, WaitOnline};

/// A configuration error with the one-based source line that caused it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// One-based line number, or zero for an I/O/whole-file error.
    pub line: usize,
    /// Human-readable description.
    pub message: String,
}

impl ConfigError {
    /// Build an error for `line`.
    pub fn new(line: usize, message: impl Into<String>) -> Self {
        ConfigError {
            line,
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(f, "{}", self.message)
        } else {
            write!(f, "line {}: {}", self.line, self.message)
        }
    }
}

impl std::error::Error for ConfigError {}

/// Largest configuration file accepted.
pub const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
/// Maximum number of interface sections.
pub const MAX_INTERFACES: usize = 256;
/// Maximum number of route sections.
pub const MAX_ROUTES: usize = 1024;
/// Maximum addresses on one interface.
pub const MAX_ADDRESSES: usize = 64;
/// Maximum DNS servers on one interface or in `[global]`.
pub const MAX_DNS: usize = 16;
/// Maximum bridge ports.
pub const MAX_PORTS: usize = 64;

/// Parse a configuration body.
pub fn parse(text: &str) -> Result<Config, ConfigError> {
    if text.len() as u64 > MAX_CONFIG_BYTES {
        return Err(ConfigError::new(
            0,
            format!("configuration is larger than {MAX_CONFIG_BYTES} bytes"),
        ));
    }
    Parser::new().parse(text)
}

/// Read and parse a configuration file.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let text = read_text(path)?;
    parse(&text)
}

/// Read the main file plus `config.d/*.conf` drop-ins.
///
/// Drop-ins are applied in lexical order. An `[interface NAME]` or
/// `[route NAME]` in a later file replaces the same section in the effective
/// configuration; a `[global]` section in a drop-in replaces the global
/// section. All files are validated individually before merging.
pub fn load_effective(path: &Path) -> Result<Config, ConfigError> {
    let mut config = load(path)?;
    let directory = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("config.d");
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(config),
        Err(error) => {
            return Err(ConfigError::new(
                0,
                format!("cannot read {}: {error}", directory.display()),
            ));
        }
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "conf"))
        .collect();
    files.sort();
    for file in files {
        let text = read_text(&file)?;
        let has_global = text.lines().any(|line| line.trim().starts_with("[global]"));
        let overlay = parse(&text)?;
        if has_global {
            config.global = overlay.global;
        }
        for iface in overlay.interfaces {
            if let Some(existing) = config.interface_mut(&iface.name) {
                *existing = iface;
            } else {
                config.interfaces.push(iface);
            }
        }
        for route in overlay.routes {
            if let Some(existing) = config
                .routes
                .iter_mut()
                .find(|item| item.name == route.name)
            {
                *existing = route;
            } else {
                config.routes.push(route);
            }
        }
    }
    Ok(config)
}

fn read_text(path: &Path) -> Result<String, ConfigError> {
    let metadata = std::fs::metadata(path)
        .map_err(|e| ConfigError::new(0, format!("cannot read {}: {e}", path.display())))?;
    if !metadata.is_file() {
        return Err(ConfigError::new(
            0,
            format!("{} is not a regular file", path.display()),
        ));
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::new(
            0,
            format!("{} is larger than {MAX_CONFIG_BYTES} bytes", path.display()),
        ));
    }
    std::fs::read_to_string(path)
        .map_err(|e| ConfigError::new(0, format!("cannot read {}: {e}", path.display())))
}

/// Render a complete effective configuration.
pub fn render(config: &Config) -> String {
    let mut out = String::new();
    out.push_str("[global]\n");
    out.push_str(&format!(
        "manage-resolv-conf = {}\nmanage-loopback = {}\nwait-online = {}\nwait-online-timeout = {}\ndhcp-timeout = {}\n",
        yes_no(config.global.manage_resolv_conf),
        yes_no(config.global.manage_loopback),
        match config.global.wait_online {
            WaitOnline::Any => "any",
            WaitOnline::All => "all",
            WaitOnline::None => "none",
        },
        config.global.wait_online_timeout,
        config.global.dhcp_timeout
    ));
    if let Some(hostname) = &config.global.hostname {
        out.push_str(&format!("hostname = {hostname}\n"));
    }
    for domain in &config.global.search {
        out.push_str(&format!("search = {domain}\n"));
    }
    for dns in &config.global.dns {
        out.push_str(&format!("dns = {dns}\n"));
    }
    for iface in &config.interfaces {
        out.push_str(&render_interface(iface));
    }
    for route in &config.routes {
        out.push_str(&render_route(route));
    }
    out
}

/// Render only interface and route sections, for a machine-owned drop-in.
pub fn render_sections(config: &Config) -> String {
    let mut out = String::new();
    for iface in &config.interfaces {
        out.push_str(&render_interface(iface));
    }
    for route in &config.routes {
        out.push_str(&render_route(route));
    }
    out
}

/// Render one interface section.
pub fn render_interface(iface: &Interface) -> String {
    let mut out = format!("[interface {}]\n", iface.name);
    out.push_str(&format!(
        "type = {}\nenabled = {}\nrequired = {}\ndhcp4 = {}\n",
        match iface.kind {
            InterfaceKind::Ethernet => "ethernet",
            InterfaceKind::Bridge => "bridge",
        },
        yes_no(iface.enabled),
        yes_no(iface.required),
        yes_no(iface.dhcp4)
    ));
    for address in &iface.addresses {
        out.push_str(&format!("address = {address}\n"));
    }
    if let Some(gateway) = iface.gateway {
        out.push_str(&format!("gateway = {gateway}\n"));
    }
    for dns in &iface.dns {
        out.push_str(&format!("dns = {dns}\n"));
    }
    if let Some(mtu) = iface.mtu {
        out.push_str(&format!("mtu = {mtu}\n"));
    }
    if iface.kind == InterfaceKind::Bridge {
        if !iface.ports.is_empty() {
            out.push_str(&format!("ports = {}\n", iface.ports.join(" ")));
        }
        out.push_str(&format!(
            "stp = {}\nforward-delay = {}\nhello-time = {}\nmax-age = {}\n",
            yes_no(iface.stp),
            iface.forward_delay,
            iface.hello_time,
            iface.max_age
        ));
    }
    if let Some(wait) = iface.wait_online {
        out.push_str(&format!("wait-online = {}\n", yes_no(wait)));
    }
    out.push('\n');
    out
}

/// Render one route section.
pub fn render_route(route: &Route) -> String {
    let mut out = format!("[route {}]\n", route.name);
    out.push_str(&format!(
        "to = {}\n",
        route
            .to
            .map(|to| to.to_string())
            .unwrap_or_else(|| "default".to_string())
    ));
    if let Some(via) = route.via {
        out.push_str(&format!("via = {via}\n"));
    }
    if let Some(dev) = &route.dev {
        out.push_str(&format!("dev = {dev}\n"));
    }
    out.push_str(&format!("metric = {}\n", route.metric));
    out.push_str(&format!(
        "table = {}\n",
        if route.table == RT_TABLE_MAIN {
            "main".to_string()
        } else {
            route.table.to_string()
        }
    ));
    if let Some(src) = route.src {
        out.push_str(&format!("src = {src}\n"));
    }
    out.push_str(&format!("onlink = {}\n\n", yes_no(route.onlink)));
    out
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[derive(Debug, Clone, Copy)]
enum Section {
    Global,
    Interface(usize),
    Route(usize),
}

struct Parser {
    config: Config,
    interface_lines: Vec<usize>,
    route_lines: Vec<usize>,
    bridge_keys_seen: Vec<bool>,
    route_has_to: Vec<bool>,
    global_seen: bool,
    seen: HashSet<String>,
    section: Option<Section>,
}

impl Parser {
    fn new() -> Self {
        Parser {
            config: Config::default(),
            interface_lines: Vec::new(),
            route_lines: Vec::new(),
            bridge_keys_seen: Vec::new(),
            route_has_to: Vec::new(),
            global_seen: false,
            seen: HashSet::new(),
            section: None,
        }
    }

    fn parse(mut self, text: &str) -> Result<Config, ConfigError> {
        let mut logical = String::new();
        let mut start_line = 0usize;
        for (index, raw) in text.lines().enumerate() {
            let line_no = index + 1;
            if raw.len() > 4096 {
                return Err(ConfigError::new(line_no, "line is longer than 4096 bytes"));
            }
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() && logical.is_empty() {
                continue;
            }
            if logical.is_empty() {
                start_line = line_no;
            }
            let continued = line.ends_with('\\');
            let part = if continued {
                line[..line.len() - 1].trim_end()
            } else {
                line
            };
            if !logical.is_empty() {
                logical.push(' ');
            }
            logical.push_str(part);
            if continued {
                continue;
            }
            self.line(start_line, &logical)?;
            logical.clear();
        }
        if !logical.is_empty() {
            self.line(start_line, &logical)?;
        }
        self.finish()
    }

    fn line(&mut self, line_no: usize, line: &str) -> Result<(), ConfigError> {
        if line.starts_with('[') {
            return self.section_header(line_no, line);
        }
        let (key, value) = line.split_once('=').ok_or_else(|| {
            ConfigError::new(line_no, "expected 'key = value' or a [section] header")
        })?;
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() {
            return Err(ConfigError::new(line_no, "empty key"));
        }
        let repeatable = matches!(key, "address" | "dns" | "search" | "ports");
        if !repeatable && !self.seen.insert(key.to_string()) {
            return Err(ConfigError::new(
                line_no,
                format!("duplicate key '{key}' in this section"),
            ));
        }
        match self.section {
            Some(Section::Global) => self.global(line_no, key, value),
            Some(Section::Interface(index)) => self.interface(line_no, index, key, value),
            Some(Section::Route(index)) => self.route(line_no, index, key, value),
            None => Err(ConfigError::new(
                line_no,
                "key outside a [global], [interface] or [route] section",
            )),
        }
    }

    fn section_header(&mut self, line_no: usize, line: &str) -> Result<(), ConfigError> {
        if !line.ends_with(']') {
            return Err(ConfigError::new(line_no, "unterminated section header"));
        }
        let inner = &line[1..line.len() - 1];
        let mut parts = inner.split_whitespace();
        let kind = parts.next().unwrap_or("");
        let name = parts.next();
        if parts.next().is_some() {
            return Err(ConfigError::new(
                line_no,
                "section header has too many fields",
            ));
        }
        self.seen.clear();
        match (kind, name) {
            ("global", None) => {
                if self.global_seen {
                    return Err(ConfigError::new(line_no, "duplicate [global] section"));
                }
                self.global_seen = true;
                self.section = Some(Section::Global);
            }
            ("interface", Some(name)) => {
                validate_interface_name(name).map_err(|e| ConfigError::new(line_no, e))?;
                if self.config.interface(name).is_some() {
                    return Err(ConfigError::new(
                        line_no,
                        format!("duplicate interface '{name}'"),
                    ));
                }
                if self.config.interfaces.len() >= MAX_INTERFACES {
                    return Err(ConfigError::new(
                        line_no,
                        format!("more than {MAX_INTERFACES} interfaces"),
                    ));
                }
                let index = self.config.interfaces.len();
                self.config
                    .interfaces
                    .push(Interface::new(name.to_string(), InterfaceKind::Ethernet));
                self.interface_lines.push(line_no);
                self.bridge_keys_seen.push(false);
                self.section = Some(Section::Interface(index));
            }
            ("route", Some(name)) => {
                if name.is_empty() {
                    return Err(ConfigError::new(line_no, "route section needs a name"));
                }
                if self.config.route(name).is_some() {
                    return Err(ConfigError::new(
                        line_no,
                        format!("duplicate route '{name}'"),
                    ));
                }
                if self.config.routes.len() >= MAX_ROUTES {
                    return Err(ConfigError::new(
                        line_no,
                        format!("more than {MAX_ROUTES} routes"),
                    ));
                }
                let index = self.config.routes.len();
                self.config.routes.push(Route::new(name.to_string()));
                self.route_lines.push(line_no);
                self.route_has_to.push(false);
                self.section = Some(Section::Route(index));
            }
            _ => {
                return Err(ConfigError::new(
                    line_no,
                    "expected [global], [interface NAME] or [route NAME]",
                ));
            }
        }
        Ok(())
    }

    fn global(&mut self, line_no: usize, key: &str, value: &str) -> Result<(), ConfigError> {
        let global = &mut self.config.global;
        match key {
            "manage-resolv-conf" => global.manage_resolv_conf = parse_bool(line_no, key, value)?,
            "manage-loopback" => global.manage_loopback = parse_bool(line_no, key, value)?,
            "wait-online" => {
                global.wait_online =
                    WaitOnline::parse(value).map_err(|e| ConfigError::new(line_no, e))?
            }
            "wait-online-timeout" => global.wait_online_timeout = parse_u64(line_no, key, value)?,
            "dhcp-timeout" => global.dhcp_timeout = parse_u64(line_no, key, value)?,
            "hostname" => {
                if value.len() > 64 || value.is_empty() || value.contains(char::is_whitespace) {
                    return Err(ConfigError::new(
                        line_no,
                        "hostname must be 1..=64 characters without whitespace",
                    ));
                }
                global.hostname = Some(value.to_string());
            }
            "search" => {
                for domain in value.split_whitespace() {
                    validate_domain(line_no, domain)?;
                    global.search.push(domain.to_string());
                }
            }
            "dns" => {
                for item in value.split_whitespace() {
                    global.dns.push(parse_ipv4(line_no, "dns", item)?);
                }
                if global.dns.len() > MAX_DNS {
                    return Err(ConfigError::new(
                        line_no,
                        format!("more than {MAX_DNS} DNS servers"),
                    ));
                }
            }
            other => {
                return Err(ConfigError::new(
                    line_no,
                    format!("unknown [global] key '{other}'"),
                ));
            }
        }
        Ok(())
    }

    fn interface(
        &mut self,
        line_no: usize,
        index: usize,
        key: &str,
        value: &str,
    ) -> Result<(), ConfigError> {
        let iface = self
            .config
            .interfaces
            .get_mut(index)
            .ok_or_else(|| ConfigError::new(line_no, "internal interface index error"))?;
        match key {
            "type" => {
                iface.kind =
                    InterfaceKind::parse(value).map_err(|e| ConfigError::new(line_no, e))?
            }
            "enabled" => iface.enabled = parse_bool(line_no, key, value)?,
            "required" => iface.required = parse_bool(line_no, key, value)?,
            "dhcp4" => iface.dhcp4 = parse_bool(line_no, key, value)?,
            "address" => {
                if iface.addresses.len() >= MAX_ADDRESSES {
                    return Err(ConfigError::new(
                        line_no,
                        format!("more than {MAX_ADDRESSES} addresses"),
                    ));
                }
                iface.addresses.push(parse_net(line_no, key, value)?);
            }
            "gateway" => iface.gateway = Some(parse_ipv4(line_no, key, value)?),
            "dns" => {
                for item in value.split_whitespace() {
                    iface.dns.push(parse_ipv4(line_no, "dns", item)?);
                }
                if iface.dns.len() > MAX_DNS {
                    return Err(ConfigError::new(
                        line_no,
                        format!("more than {MAX_DNS} DNS servers"),
                    ));
                }
            }
            "mtu" => {
                let mtu = parse_number(line_no, key, value)?;
                if !(68..=65535).contains(&mtu) {
                    return Err(ConfigError::new(line_no, "mtu must be 68..=65535"));
                }
                iface.mtu = Some(mtu);
            }
            "ports" => {
                if let Some(flag) = self.bridge_keys_seen.get_mut(index) {
                    *flag = true;
                }
                for port in value.split_whitespace() {
                    validate_interface_name(port).map_err(|e| ConfigError::new(line_no, e))?;
                    if iface.ports.len() >= MAX_PORTS {
                        return Err(ConfigError::new(
                            line_no,
                            format!("more than {MAX_PORTS} bridge ports"),
                        ));
                    }
                    if iface.ports.iter().any(|existing| existing == port) {
                        return Err(ConfigError::new(
                            line_no,
                            format!("duplicate bridge port '{port}'"),
                        ));
                    }
                    iface.ports.push(port.to_string());
                }
            }
            "stp" => {
                if let Some(flag) = self.bridge_keys_seen.get_mut(index) {
                    *flag = true;
                }
                iface.stp = parse_bool(line_no, key, value)?;
            }
            "forward-delay" => {
                if let Some(flag) = self.bridge_keys_seen.get_mut(index) {
                    *flag = true;
                }
                iface.forward_delay = parse_number(line_no, key, value)?;
            }
            "hello-time" => {
                if let Some(flag) = self.bridge_keys_seen.get_mut(index) {
                    *flag = true;
                }
                iface.hello_time = parse_number(line_no, key, value)?;
            }
            "max-age" => {
                if let Some(flag) = self.bridge_keys_seen.get_mut(index) {
                    *flag = true;
                }
                iface.max_age = parse_number(line_no, key, value)?;
            }
            "wait-online" => iface.wait_online = Some(parse_bool(line_no, key, value)?),
            other => {
                return Err(ConfigError::new(
                    line_no,
                    format!("unknown [interface] key '{other}'"),
                ));
            }
        }
        Ok(())
    }

    fn route(
        &mut self,
        line_no: usize,
        index: usize,
        key: &str,
        value: &str,
    ) -> Result<(), ConfigError> {
        let route = self
            .config
            .routes
            .get_mut(index)
            .ok_or_else(|| ConfigError::new(line_no, "internal route index error"))?;
        match key {
            "to" => {
                if let Some(flag) = self.route_has_to.get_mut(index) {
                    *flag = true;
                }
                route.to = if value.eq_ignore_ascii_case("default") {
                    None
                } else {
                    Some(parse_net(line_no, key, value)?)
                };
            }
            "via" => route.via = Some(parse_ipv4(line_no, key, value)?),
            "dev" => {
                validate_interface_name(value).map_err(|e| ConfigError::new(line_no, e))?;
                route.dev = Some(value.to_string());
            }
            "metric" => route.metric = parse_number(line_no, key, value)?,
            "table" => route.table = parse_table(line_no, value)?,
            "src" => route.src = Some(parse_ipv4(line_no, key, value)?),
            "onlink" => route.onlink = parse_bool(line_no, key, value)?,
            other => {
                return Err(ConfigError::new(
                    line_no,
                    format!("unknown [route] key '{other}'"),
                ));
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<Config, ConfigError> {
        let mut bridge_ports: HashMap<String, String> = HashMap::new();
        for (index, iface) in self.config.interfaces.iter().enumerate() {
            let line = self.interface_lines.get(index).copied().unwrap_or(0);
            if iface.dhcp4 && (!iface.addresses.is_empty() || iface.gateway.is_some()) {
                return Err(ConfigError::new(
                    line,
                    format!(
                        "{}: dhcp4 and static address/gateway are mutually exclusive",
                        iface.name
                    ),
                ));
            }
            if iface.kind != InterfaceKind::Bridge
                && self.bridge_keys_seen.get(index).copied().unwrap_or(false)
            {
                return Err(ConfigError::new(
                    line,
                    format!("{}: bridge keys are only valid on a bridge", iface.name),
                ));
            }
            for port in &iface.ports {
                if iface.kind != InterfaceKind::Bridge {
                    return Err(ConfigError::new(
                        line,
                        format!("{}: ports is only valid on a bridge", iface.name),
                    ));
                }
                if port == &iface.name {
                    return Err(ConfigError::new(
                        line,
                        format!("{}: a bridge cannot contain itself", iface.name),
                    ));
                }
                if let Some(existing) = bridge_ports.get(port)
                    && existing != &iface.name
                {
                    return Err(ConfigError::new(
                        line,
                        format!(
                            "{}: port {port} is already in bridge {existing}",
                            iface.name
                        ),
                    ));
                }
                bridge_ports.insert(port.clone(), iface.name.clone());
                if let Some(port_iface) = self.config.interface(port) {
                    if port_iface.kind == InterfaceKind::Bridge {
                        return Err(ConfigError::new(
                            line,
                            format!("{}: port {port} is itself a bridge", iface.name),
                        ));
                    }
                    if !port_iface.addresses.is_empty()
                        || port_iface.dhcp4
                        || port_iface.gateway.is_some()
                    {
                        return Err(ConfigError::new(
                            line,
                            format!(
                                "{}: port {port} must not have an address, DHCP or gateway",
                                iface.name
                            ),
                        ));
                    }
                }
            }
        }
        for (index, route) in self.config.routes.iter().enumerate() {
            let line = self.route_lines.get(index).copied().unwrap_or(0);
            if !self.route_has_to.get(index).copied().unwrap_or(false) {
                return Err(ConfigError::new(
                    line,
                    format!("route {}: 'to' is required", route.name),
                ));
            }
            if route.via.is_none() && route.dev.is_none() {
                return Err(ConfigError::new(
                    line,
                    format!("route {}: at least one of via/dev is required", route.name),
                ));
            }
        }
        Ok(self.config)
    }
}

fn validate_interface_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 15 {
        return Err(format!("interface name '{name}' must be 1..=15 bytes"));
    }
    if name == "." || name == ".." || name.contains('/') {
        return Err(format!("interface name '{name}' is not allowed"));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err(format!(
            "interface name '{name}' contains characters outside A-Za-z0-9_.-"
        ));
    }
    Ok(())
}

fn validate_domain(line: usize, domain: &str) -> Result<(), ConfigError> {
    if domain.is_empty() || domain.len() > 253 {
        return Err(ConfigError::new(
            line,
            "search domain must be 1..=253 bytes",
        ));
    }
    if domain.contains('/') || domain.contains(char::is_whitespace) {
        return Err(ConfigError::new(
            line,
            format!("invalid search domain '{domain}'"),
        ));
    }
    Ok(())
}

fn parse_bool(line: usize, key: &str, value: &str) -> Result<bool, ConfigError> {
    match value {
        "yes" => Ok(true),
        "no" => Ok(false),
        other => Err(ConfigError::new(
            line,
            format!("{key} expects 'yes' or 'no', got '{other}'"),
        )),
    }
}

fn parse_number(line: usize, key: &str, value: &str) -> Result<u32, ConfigError> {
    value
        .parse()
        .map_err(|_| ConfigError::new(line, format!("{key} expects a non-negative number")))
}

fn parse_u64(line: usize, key: &str, value: &str) -> Result<u64, ConfigError> {
    value
        .parse()
        .map_err(|_| ConfigError::new(line, format!("{key} expects a non-negative number")))
}

fn parse_ipv4(line: usize, key: &str, value: &str) -> Result<Ipv4Addr, ConfigError> {
    if value.contains(':') {
        return Err(ConfigError::new(
            line,
            format!("{key}: IPv6 is not supported in this version"),
        ));
    }
    Ipv4Addr::from_str(value)
        .map_err(|_| ConfigError::new(line, format!("{key}: '{value}' is not an IPv4 address")))
}

fn parse_net(line: usize, key: &str, value: &str) -> Result<Ipv4Net, ConfigError> {
    Ipv4Net::from_str(value).map_err(|e| ConfigError::new(line, format!("{key}: {e}")))
}

fn parse_table(line: usize, value: &str) -> Result<u32, ConfigError> {
    if value.eq_ignore_ascii_case("main") {
        return Ok(RT_TABLE_MAIN);
    }
    let table = value
        .parse::<u32>()
        .map_err(|_| ConfigError::new(line, "table expects 'main' or a number"))?;
    if table == 0 {
        return Err(ConfigError::new(
            line,
            "table 0 is not a valid routing table",
        ));
    }
    Ok(table)
}

/// The defaults used by tests and by an empty configuration.
pub fn defaults() -> Config {
    Config::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = "\
[global]
manage-resolv-conf = yes
manage-loopback = no
wait-online = any
wait-online-timeout = 30
search = lan

[interface eth0]
type = ethernet
enabled = yes
dhcp4 = yes

[interface eth1]
type = ethernet
enabled = yes

[interface eth2]
type = ethernet
enabled = yes

[interface br0]
type = bridge
enabled = yes
ports = eth1 eth2
stp = yes
forward-delay = 2
address = 192.168.10.2/24
gateway = 192.168.10.1
dns = 192.168.10.1
mtu = 1500

[route lab]
to = 10.20.0.0/16
via = 192.168.10.1
dev = br0
metric = 100
";

    #[test]
    fn example_parses() {
        let config = parse(EXAMPLE).expect("example");
        assert_eq!(config.interfaces.len(), 4);
        assert_eq!(config.routes.len(), 1);
        assert!(config.interface("br0").expect("bridge").stp);
        assert!(config.interface("eth0").expect("eth0").dhcp4);
        assert_eq!(
            config.routes[0].to.expect("route").to_string(),
            "10.20.0.0/16"
        );
    }

    #[test]
    fn ipv6_literals_are_rejected() {
        let text = EXAMPLE.replace("address = 192.168.10.2/24", "address = fd00::1/64");
        let error = parse(&text).expect_err("IPv6");
        assert!(error.message.contains("IPv6"), "{error}");
    }

    #[test]
    fn dhcp_and_static_addresses_conflict() {
        let text = EXAMPLE.replace("dhcp4 = yes", "dhcp4 = yes\naddress = 192.0.2.10/24");
        let error = parse(&text).expect_err("conflict");
        assert!(error.message.contains("mutually exclusive"), "{error}");
    }

    #[test]
    fn bridge_port_may_not_have_an_address() {
        let text = EXAMPLE.replace(
            "[interface eth1]\ntype = ethernet\nenabled = yes",
            "[interface eth1]\ntype = ethernet\nenabled = yes\naddress = 192.0.2.1/24",
        );
        let error = parse(&text).expect_err("port address");
        assert!(
            error.message.contains("must not have an address"),
            "{error}"
        );
    }

    #[test]
    fn unknown_keys_and_duplicates_are_rejected() {
        let unknown = EXAMPLE.replace("enabled = yes", "enabld = yes");
        assert!(parse(&unknown).is_err());
        let duplicate = EXAMPLE.replace("enabled = yes", "enabled = yes\nenabled = no");
        assert!(parse(&duplicate).is_err());
    }

    #[test]
    fn route_requires_to_and_a_next_hop() {
        let text = "[route bad]\nmetric = 1\n";
        assert!(parse(text).is_err());
        let text = "[route bad]\nto = 10.0.0.0/8\n";
        assert!(parse(text).is_err());
    }

    #[test]
    fn continuation_and_comments_work() {
        let text = "\
[global]
# a comment
wait-online = \\
any
";
        let config = parse(text).expect("continuation");
        assert_eq!(config.global.wait_online, WaitOnline::Any);
    }

    #[test]
    fn arbitrary_text_does_not_panic() {
        let mut state = 0xdead_beefu32;
        for length in 0..512 {
            let mut bytes = vec![0u8; length];
            for byte in &mut bytes {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *byte = (state >> 24) as u8;
            }
            let text = String::from_utf8_lossy(&bytes);
            let _ = parse(&text);
        }
    }

    #[test]
    fn invalid_interface_names_are_rejected() {
        for name in ["", "a/b", "..", "this-name-is-too-long"] {
            let text = format!("[interface {name}]\n");
            assert!(parse(&text).is_err(), "{name}");
        }
    }

    #[test]
    fn effective_config_applies_dropins() {
        let root = std::env::temp_dir().join(format!("rnet-config-dropin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config.d")).expect("temp");
        let main = root.join("config");
        std::fs::write(
            &main,
            "[global]\nwait-online = any\n\n[interface eth0]\ntype = ethernet\ndhcp4 = yes\n\n[route lab]\nto = 10.0.0.0/8\nvia = 192.0.2.1\n",
        )
        .expect("main");
        std::fs::write(
            root.join("config.d/90-rncli.conf"),
            "[interface eth0]\ntype = ethernet\nenabled = yes\naddress = 192.0.2.10/24\n\n[route lab]\nto = 10.1.0.0/16\nvia = 192.0.2.1\n",
        )
        .expect("dropin");
        let config = load_effective(&main).expect("effective");
        assert_eq!(
            config.interface("eth0").expect("eth0").addresses,
            vec!["192.0.2.10/24".parse().expect("net")]
        );
        assert_eq!(
            config
                .route("lab")
                .expect("route")
                .to
                .expect("to")
                .to_string(),
            "10.1.0.0/16"
        );
        let rendered = render_sections(&config);
        assert!(rendered.contains("[interface eth0]"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rendering_round_trips_through_the_parser() {
        let config = parse(EXAMPLE).expect("example");
        let rendered = render_sections(&config);
        let reparsed = parse(&rendered).expect("rendered");
        assert_eq!(reparsed.interfaces, config.interfaces);
        assert_eq!(reparsed.routes, config.routes);
    }
}
