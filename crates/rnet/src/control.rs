//! Control protocol between `rncli` and `rnetd`.
//!
//! The framing matches rinit's control plane: a little-endian `u32` body
//! length followed by a UTF-8 text body. The body is line-oriented
//! `key=value`, which keeps the protocol debuggable with `socat` and avoids
//! pulling serde into the daemon. It is intentionally separate from rinit's
//! protocol: rnet has its own version and operations.

use std::fmt;
use std::net::Ipv4Addr;
use std::str::FromStr;

use crate::model::{Interface, InterfaceKind, Ipv4Net, RT_TABLE_MAIN, Route};

/// Protocol version carried in the first body line.
pub const PROTOCOL_VERSION: u16 = 1;
/// Bytes in the little-endian length prefix.
pub const LENGTH_PREFIX_BYTES: usize = 4;
/// Largest control body accepted.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// A request from `rncli`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Show the summary, optionally for one interface.
    Status { iface: Option<String> },
    /// Show one interface in detail.
    Show { iface: String },
    /// Re-read the configuration and apply the diff.
    Reload,
    /// Bring an interface up or down.
    Iface { iface: String, up: bool },
    /// Add or remove an IPv4 address at runtime.
    Address {
        iface: String,
        address: Ipv4Net,
        add: bool,
    },
    /// Add or remove a route at runtime.
    Route { route: Route, add: bool },
    /// Create a bridge at runtime.
    BridgeCreate { bridge: Interface },
    /// Delete a bridge at runtime.
    BridgeDelete { name: String },
    /// Add or remove a port from a bridge.
    BridgePort {
        bridge: String,
        port: String,
        add: bool,
    },
    /// Renew or release a DHCP lease.
    Dhcp { iface: String, renew: bool },
}

impl Request {
    /// The operation name used on the wire.
    pub const fn op(&self) -> &'static str {
        match self {
            Request::Status { .. } => "status",
            Request::Show { .. } => "show",
            Request::Reload => "reload",
            Request::Iface { .. } => "iface",
            Request::Address { .. } => "address",
            Request::Route { .. } => "route",
            Request::BridgeCreate { .. } => "bridge-create",
            Request::BridgeDelete { .. } => "bridge-delete",
            Request::BridgePort { .. } => "bridge-port",
            Request::Dhcp { .. } => "dhcp",
        }
    }
}

/// A response from `rnetd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// Whether the operation succeeded.
    pub ok: bool,
    /// Stable numeric status: 0 ok, 1 error, 2 missing, 3 conflict.
    pub code: u16,
    /// Human-readable message; empty for success.
    pub message: String,
    /// Repeated `key=value` output fields.
    pub fields: Vec<(String, String)>,
}

impl Response {
    /// A successful response with no fields.
    pub fn ok() -> Self {
        Response {
            ok: true,
            code: 0,
            message: String::new(),
            fields: Vec::new(),
        }
    }

    /// A successful response with output fields.
    pub fn ok_with(fields: Vec<(String, String)>) -> Self {
        Response {
            ok: true,
            code: 0,
            message: String::new(),
            fields,
        }
    }

    /// An error response.
    pub fn err(code: u16, message: impl Into<String>) -> Self {
        Response {
            ok: false,
            code,
            message: message.into(),
            fields: Vec::new(),
        }
    }

    /// Add one output field.
    pub fn field(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.fields.push((key.into(), value.into()));
        self
    }
}

impl fmt::Display for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ok {
            write!(f, "ok")?;
        } else {
            write!(f, "err {} {}", self.code, self.message)?;
        }
        for (key, value) in &self.fields {
            write!(f, "\n{key}={value}")?;
        }
        Ok(())
    }
}

/// Wrap a body in its length prefix.
pub fn frame(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(LENGTH_PREFIX_BYTES + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// Decode a length prefix, rejecting an oversized frame.
pub fn frame_len(prefix: [u8; LENGTH_PREFIX_BYTES]) -> Result<usize, String> {
    let declared = u32::from_le_bytes(prefix) as usize;
    if declared > MAX_FRAME_BYTES {
        return Err(format!(
            "declared frame length {declared} exceeds {MAX_FRAME_BYTES}"
        ));
    }
    Ok(declared)
}

/// Encode a request body (without the length prefix).
pub fn encode_request(request: &Request) -> Vec<u8> {
    let mut out = format!("rnet {PROTOCOL_VERSION}\nop={}\n", request.op());
    match request {
        Request::Status { iface } => {
            if let Some(iface) = iface {
                out.push_str(&format!("iface={}\n", escape(iface)));
            }
        }
        Request::Show { iface } => out.push_str(&format!("iface={}\n", escape(iface))),
        Request::Reload => {}
        Request::Iface { iface, up } => {
            out.push_str(&format!("iface={}\nup={}\n", escape(iface), yes_no(*up)));
        }
        Request::Address {
            iface,
            address,
            add,
        } => {
            out.push_str(&format!(
                "iface={}\naddress={}\nadd={}\n",
                escape(iface),
                address,
                yes_no(*add)
            ));
        }
        Request::Route { route, add } => {
            out.push_str(&format!("add={}\n", yes_no(*add)));
            out.push_str(&format!("name={}\n", escape(&route.name)));
            out.push_str(&format!(
                "to={}\n",
                route
                    .to
                    .map(|net| net.to_string())
                    .unwrap_or_else(|| "default".to_string())
            ));
            if let Some(via) = route.via {
                out.push_str(&format!("via={via}\n"));
            }
            if let Some(dev) = &route.dev {
                out.push_str(&format!("dev={}\n", escape(dev)));
            }
            out.push_str(&format!("metric={}\n", route.metric));
            out.push_str(&format!("table={}\n", route.table));
            if let Some(src) = route.src {
                out.push_str(&format!("src={src}\n"));
            }
            out.push_str(&format!("onlink={}\n", yes_no(route.onlink)));
        }
        Request::BridgeCreate { bridge } => {
            out.push_str(&format!("name={}\n", escape(&bridge.name)));
            out.push_str(&format!("stp={}\n", yes_no(bridge.stp)));
            out.push_str(&format!("forward-delay={}\n", bridge.forward_delay));
            out.push_str(&format!("hello-time={}\n", bridge.hello_time));
            out.push_str(&format!("max-age={}\n", bridge.max_age));
            out.push_str(&format!("mtu={}\n", bridge.mtu.unwrap_or(0)));
        }
        Request::BridgeDelete { name } => out.push_str(&format!("name={}\n", escape(name))),
        Request::BridgePort { bridge, port, add } => {
            out.push_str(&format!(
                "bridge={}\nport={}\nadd={}\n",
                escape(bridge),
                escape(port),
                yes_no(*add)
            ));
        }
        Request::Dhcp { iface, renew } => {
            out.push_str(&format!(
                "iface={}\nrenew={}\n",
                escape(iface),
                yes_no(*renew)
            ));
        }
    }
    out.into_bytes()
}

/// Decode a request body.
pub fn decode_request(body: &[u8]) -> Result<Request, String> {
    let fields = Fields::parse(body)?;
    if fields.version()? != PROTOCOL_VERSION {
        return Err(format!(
            "protocol version {} is not {PROTOCOL_VERSION}",
            fields.version()?
        ));
    }
    let op = fields.one("op")?;
    match op.as_str() {
        "status" => Ok(Request::Status {
            iface: fields.opt("iface"),
        }),
        "show" => Ok(Request::Show {
            iface: fields.one("iface")?,
        }),
        "reload" => Ok(Request::Reload),
        "iface" => Ok(Request::Iface {
            iface: fields.one("iface")?,
            up: fields.bool("up")?,
        }),
        "address" => Ok(Request::Address {
            iface: fields.one("iface")?,
            address: Ipv4Net::from_str(&fields.one("address")?).map_err(|e| e.to_string())?,
            add: fields.bool("add")?,
        }),
        "route" => {
            let mut route = Route::new(fields.one("name")?);
            route.to = match fields.one("to")?.as_str() {
                "default" => None,
                value => Some(Ipv4Net::from_str(value).map_err(|e| e.to_string())?),
            };
            route.via = fields.opt("via").map(|v| parse_ipv4(&v)).transpose()?;
            route.dev = fields.opt("dev");
            route.metric = fields
                .opt("metric")
                .map(|v| v.parse::<u32>())
                .transpose()
                .map_err(|_| "metric is not a number".to_string())?
                .unwrap_or(0);
            route.table = fields
                .opt("table")
                .map(|v| v.parse::<u32>())
                .transpose()
                .map_err(|_| "table is not a number".to_string())?
                .unwrap_or(RT_TABLE_MAIN);
            route.src = fields.opt("src").map(|v| parse_ipv4(&v)).transpose()?;
            route.onlink = fields.opt_bool("onlink")?.unwrap_or(false);
            Ok(Request::Route {
                route,
                add: fields.bool("add")?,
            })
        }
        "bridge-create" => {
            let name = fields.one("name")?;
            let mut bridge = Interface::new(name, InterfaceKind::Bridge);
            bridge.stp = fields.opt_bool("stp")?.unwrap_or(false);
            bridge.forward_delay = fields.opt_u32("forward-delay")?.unwrap_or(15);
            bridge.hello_time = fields.opt_u32("hello-time")?.unwrap_or(2);
            bridge.max_age = fields.opt_u32("max-age")?.unwrap_or(20);
            bridge.mtu = fields.opt_u32("mtu")?.filter(|mtu| *mtu != 0);
            Ok(Request::BridgeCreate { bridge })
        }
        "bridge-delete" => Ok(Request::BridgeDelete {
            name: fields.one("name")?,
        }),
        "bridge-port" => Ok(Request::BridgePort {
            bridge: fields.one("bridge")?,
            port: fields.one("port")?,
            add: fields.bool("add")?,
        }),
        "dhcp" => Ok(Request::Dhcp {
            iface: fields.one("iface")?,
            renew: fields.bool("renew")?,
        }),
        other => Err(format!("unknown operation '{other}'")),
    }
}

/// Encode a response body (without the length prefix).
pub fn encode_response(response: &Response) -> Vec<u8> {
    let mut out = if response.ok {
        "ok\n".to_string()
    } else {
        format!("err {} {}\n", response.code, escape(&response.message))
    };
    for (key, value) in &response.fields {
        out.push_str(&format!("{key}={}\n", escape(value)));
    }
    out.into_bytes()
}

/// Decode a response body.
pub fn decode_response(body: &[u8]) -> Result<Response, String> {
    let text = std::str::from_utf8(body).map_err(|_| "response is not UTF-8".to_string())?;
    let mut lines = text.lines();
    let first = lines.next().ok_or_else(|| "empty response".to_string())?;
    let (ok, code, message) = if first == "ok" {
        (true, 0, String::new())
    } else if let Some(rest) = first.strip_prefix("err ") {
        let (code, message) = rest
            .split_once(' ')
            .ok_or_else(|| "malformed error line".to_string())?;
        let code = code
            .parse::<u16>()
            .map_err(|_| "error code is not a number".to_string())?;
        (false, code, unescape(message))
    } else {
        return Err("response must start with 'ok' or 'err'".to_string());
    };
    let mut fields = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("malformed response line '{line}'"))?;
        fields.push((key.to_string(), unescape(value)));
    }
    Ok(Response {
        ok,
        code,
        message,
        fields,
    })
}

/// Parsed request fields.
struct Fields {
    version: u16,
    values: Vec<(String, String)>,
}

impl Fields {
    fn parse(body: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(body).map_err(|_| "request is not UTF-8".to_string())?;
        let mut lines = text.lines();
        let version_line = lines.next().ok_or_else(|| "empty request".to_string())?;
        let version = version_line
            .strip_prefix("rnet ")
            .ok_or_else(|| "request must start with 'rnet <version>'".to_string())?
            .parse::<u16>()
            .map_err(|_| "protocol version is not a number".to_string())?;
        let mut values = Vec::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| format!("malformed request line '{line}'"))?;
            values.push((key.to_string(), unescape(value)));
        }
        Ok(Fields { version, values })
    }

    fn version(&self) -> Result<u16, String> {
        Ok(self.version)
    }

    fn one(&self, key: &str) -> Result<String, String> {
        self.opt(key)
            .ok_or_else(|| format!("missing field '{key}'"))
    }

    fn opt(&self, key: &str) -> Option<String> {
        self.values
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.clone())
    }

    fn bool(&self, key: &str) -> Result<bool, String> {
        match self.one(key)?.as_str() {
            "yes" => Ok(true),
            "no" => Ok(false),
            other => Err(format!("{key} expects 'yes' or 'no', got '{other}'")),
        }
    }

    fn opt_bool(&self, key: &str) -> Result<Option<bool>, String> {
        match self.opt(key).as_deref() {
            Some("yes") => Ok(Some(true)),
            Some("no") => Ok(Some(false)),
            Some(other) => Err(format!("{key} expects 'yes' or 'no', got '{other}'")),
            None => Ok(None),
        }
    }

    fn opt_u32(&self, key: &str) -> Result<Option<u32>, String> {
        self.opt(key)
            .map(|value| {
                value
                    .parse::<u32>()
                    .map_err(|_| format!("{key} is not a number"))
            })
            .transpose()
    }
}

fn parse_ipv4(value: &str) -> Result<Ipv4Addr, String> {
    Ipv4Addr::from_str(value).map_err(|_| format!("'{value}' is not an IPv4 address"))
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(request: Request) {
        let body = encode_request(&request);
        let decoded = decode_request(&body).expect("decode request");
        assert_eq!(decoded, request);
    }

    #[test]
    fn requests_round_trip() {
        roundtrip(Request::Status { iface: None });
        roundtrip(Request::Status {
            iface: Some("eth0".to_string()),
        });
        roundtrip(Request::Show {
            iface: "eth0".to_string(),
        });
        roundtrip(Request::Reload);
        roundtrip(Request::Iface {
            iface: "eth0".to_string(),
            up: true,
        });
        roundtrip(Request::Address {
            iface: "eth0".to_string(),
            address: "192.0.2.10/24".parse().expect("net"),
            add: true,
        });
        roundtrip(Request::Route {
            route: Route {
                name: "lab".to_string(),
                to: Some("10.0.0.0/8".parse().expect("net")),
                via: Some("192.0.2.1".parse().expect("ip")),
                dev: Some("eth0".to_string()),
                metric: 100,
                table: RT_TABLE_MAIN,
                src: None,
                onlink: false,
            },
            add: true,
        });
        roundtrip(Request::BridgeCreate {
            bridge: Interface {
                name: "br0".to_string(),
                kind: InterfaceKind::Bridge,
                stp: true,
                forward_delay: 2,
                hello_time: 2,
                max_age: 20,
                mtu: Some(1500),
                ..Interface::new("br0".to_string(), InterfaceKind::Bridge)
            },
        });
        roundtrip(Request::BridgeDelete {
            name: "br0".to_string(),
        });
        roundtrip(Request::BridgePort {
            bridge: "br0".to_string(),
            port: "eth1".to_string(),
            add: true,
        });
        roundtrip(Request::Dhcp {
            iface: "eth0".to_string(),
            renew: true,
        });
    }

    #[test]
    fn responses_round_trip() {
        let response = Response::ok_with(vec![
            ("iface".to_string(), "eth0".to_string()),
            ("state".to_string(), "up".to_string()),
        ]);
        let body = encode_response(&response);
        assert_eq!(decode_response(&body).expect("decode"), response);

        let error = Response::err(3, "no such unit");
        let body = encode_response(&error);
        assert_eq!(decode_response(&body).expect("decode"), error);
    }

    #[test]
    fn frame_rejects_oversize() {
        let mut prefix = [0u8; LENGTH_PREFIX_BYTES];
        prefix.copy_from_slice(&((MAX_FRAME_BYTES + 1) as u32).to_le_bytes());
        assert!(frame_len(prefix).is_err());
    }

    #[test]
    fn arbitrary_bytes_do_not_panic() {
        let mut state = 0x1234_5678u32;
        for length in 0..512 {
            let mut buffer = vec![0u8; length];
            for byte in &mut buffer {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *byte = (state >> 24) as u8;
            }
            let _ = decode_request(&buffer);
            let _ = decode_response(&buffer);
        }
    }

    #[test]
    fn malformed_requests_are_rejected() {
        assert!(decode_request(b"").is_err());
        assert!(decode_request(b"rnet 2\nop=status\n").is_err());
        assert!(decode_request(b"rnet 1\nop=status\nbad-line\n").is_err());
        assert!(decode_request(b"rnet 1\nop=unknown\n").is_err());
        assert!(decode_request(b"rnet 1\nop=address\niface=eth0\n").is_err());
    }
}
