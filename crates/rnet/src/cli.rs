//! `rncli` implementation.

use std::io::{Read, Write};
use std::net::Ipv4Addr;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::config;
use crate::control::{Request, Response, decode_response, encode_request, frame, frame_len};
use crate::model::{Config, Interface, InterfaceKind, Ipv4Net, RT_TABLE_MAIN, Route, WaitOnline};

const USAGE: &str = "\
Usage: rncli [options] COMMAND

Commands:
  status [IFACE]                  Show interfaces and addresses
  show IFACE                      Show one interface in detail
  config check [FILE]             Validate the configuration
  config show                     Validate and print the configuration
  reload                          Re-read the configuration and apply it
  wait-online [--timeout N]       Wait for the configured online policy
  iface up|down IFACE             Bring an interface up or down
  addr add|del IFACE CIDR         Add or remove an IPv4 address
  route add|del NAME DEST [via GW] [dev IFACE] [metric N] [table T]
  bridge create NAME [stp yes|no] [forward-delay N] [hello-time N] [max-age N]
  bridge delete NAME
  bridge add-port|del-port BRIDGE PORT
  dhcp renew|release IFACE        Renew or release a DHCP lease

Options:
  --config PATH                   Configuration file (default /etc/rnet/config)
  --control PATH                  Control socket (default /run/rnet/control.sock)
  --porcelain                     Print key=value output
  --persist                       Persist a mutation in config.d/90-rncli.conf
  -h, --help                      Show this help
  -V, --version                   Show the version
";

struct Options {
    config: PathBuf,
    control: PathBuf,
    porcelain: bool,
    persist: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            config: PathBuf::from(crate::DEFAULT_CONFIG),
            control: PathBuf::from(crate::DEFAULT_CONTROL_SOCKET),
            porcelain: false,
            persist: false,
        }
    }
}

/// Run `rncli`.
pub fn run(args: impl Iterator<Item = std::ffi::OsString>) -> ExitCode {
    crate::restore_sigpipe();
    let args: Vec<String> = match args
        .map(|arg| arg.into_string())
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(args) => args,
        Err(_) => {
            eprintln!("rncli: arguments must be valid UTF-8");
            return ExitCode::from(2);
        }
    };
    let mut options = Options::default();
    let mut positional: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" => {
                let Some(value) = iter.next() else {
                    return usage("--config needs a path");
                };
                options.config = PathBuf::from(value);
            }
            "--control" => {
                let Some(value) = iter.next() else {
                    return usage("--control needs a path");
                };
                options.control = PathBuf::from(value);
            }
            "--porcelain" => options.porcelain = true,
            "--persist" => options.persist = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "-V" | "--version" => {
                println!("rncli {}", crate::VERSION);
                return ExitCode::SUCCESS;
            }
            other => positional.push(other.to_string()),
        }
    }
    let Some(command) = positional.first() else {
        return usage("a command is required");
    };
    let rest = positional.get(1..).unwrap_or(&[]);
    match command.as_str() {
        "status" => status(&options, rest.first().map(String::as_str)),
        "show" => {
            let Some(iface) = rest.first() else {
                return usage("show needs an interface");
            };
            show(&options, iface)
        }
        "config" => config_command(&options, rest),
        "reload" => simple(&options, Request::Reload),
        "wait-online" => wait_online(&options, rest),
        "iface" => iface_command(&options, rest),
        "addr" => addr_command(&options, rest),
        "route" => route_command(&options, rest),
        "bridge" => bridge_command(&options, rest),
        "dhcp" => dhcp_command(&options, rest),
        other => usage(&format!("unknown command '{other}'")),
    }
}

fn status(options: &Options, iface: Option<&str>) -> ExitCode {
    let request = Request::Status {
        iface: iface.map(str::to_string),
    };
    let response = match send(&options.control, &request) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("rncli: {error}");
            return ExitCode::from(1);
        }
    };
    if !response.ok {
        eprintln!("rncli: {}", response.message);
        return ExitCode::from(response.code.clamp(1, 3) as u8);
    }
    if options.porcelain {
        for (key, value) in &response.fields {
            println!("{key}={value}");
        }
        return ExitCode::SUCCESS;
    }
    print_status(&response.fields)
}

fn show(options: &Options, iface: &str) -> ExitCode {
    let response = match send(
        &options.control,
        &Request::Show {
            iface: iface.to_string(),
        },
    ) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("rncli: {error}");
            return ExitCode::from(1);
        }
    };
    if !response.ok {
        eprintln!("rncli: {}", response.message);
        return ExitCode::from(response.code.clamp(1, 3) as u8);
    }
    if options.porcelain {
        for (key, value) in &response.fields {
            println!("{key}={value}");
        }
    } else {
        for (key, value) in &response.fields {
            if key != "iface" {
                println!("{key}: {value}");
            }
        }
    }
    ExitCode::SUCCESS
}

fn config_command(options: &Options, args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("check") => {
            let explicit = args.get(1).map(PathBuf::from);
            let result = match &explicit {
                Some(path) => config::load(path),
                None => config::load_effective(&options.config),
            };
            match result {
                Ok(_) => {
                    println!("OK");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    let path = explicit.as_ref().unwrap_or(&options.config);
                    eprintln!("rncli: {}: {error}", path.display());
                    ExitCode::from(2)
                }
            }
        }
        Some("show") => match config::load_effective(&options.config) {
            Ok(config) => {
                print!("{}", config::render(&config));
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("rncli: {}: {error}", options.config.display());
                ExitCode::from(2)
            }
        },
        _ => usage("config expects check or show"),
    }
}

fn send_ok(options: &Options, request: Request) -> Result<(), ExitCode> {
    match send(&options.control, &request) {
        Ok(response) if response.ok => Ok(()),
        Ok(response) => {
            eprintln!("rncli: {}", response.message);
            Err(ExitCode::from(response.code.clamp(1, 3) as u8))
        }
        Err(error) => {
            eprintln!("rncli: {error}");
            Err(ExitCode::from(1))
        }
    }
}

fn simple(options: &Options, request: Request) -> ExitCode {
    match send_ok(options, request) {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => code,
    }
}

/// A mutation, optionally persisted in the machine-owned drop-in.
fn mutate(options: &Options, request: Request, change: Option<PersistChange>) -> ExitCode {
    if options.persist && change.is_none() {
        return usage("--persist is not supported for this command");
    }
    if let Err(code) = send_ok(options, request) {
        return code;
    }
    if !options.persist {
        return ExitCode::SUCCESS;
    }
    let Some(change) = change else {
        return ExitCode::SUCCESS;
    };
    match persist(options, change) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rncli: {error}");
            ExitCode::from(1)
        }
    }
}

#[derive(Debug, Clone)]
enum PersistChange {
    Iface {
        name: String,
        up: bool,
    },
    Address {
        iface: String,
        address: Ipv4Net,
        add: bool,
    },
    Route {
        route: Route,
        add: bool,
    },
    BridgeCreate {
        bridge: Interface,
    },
    BridgeDelete {
        name: String,
    },
    BridgePort {
        bridge: String,
        port: String,
        add: bool,
    },
}

fn persist(options: &Options, change: PersistChange) -> Result<(), String> {
    let effective = config::load_effective(&options.config)
        .map_err(|e| format!("{}: {e}", options.config.display()))?;
    let path = dropin_path(&options.config);
    let mut persisted = match std::fs::metadata(&path) {
        Ok(_) => config::load(&path).map_err(|e| format!("{}: {e}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    match change {
        PersistChange::Iface { name, up } => {
            let mut iface = persisted
                .interface(&name)
                .cloned()
                .or_else(|| effective.interface(&name).cloned())
                .unwrap_or_else(|| Interface::new(name.clone(), InterfaceKind::Ethernet));
            iface.enabled = up;
            upsert_interface(&mut persisted, iface);
        }
        PersistChange::Address {
            iface,
            address,
            add,
        } => {
            let mut target = persisted
                .interface(&iface)
                .cloned()
                .or_else(|| effective.interface(&iface).cloned())
                .unwrap_or_else(|| Interface::new(iface.clone(), InterfaceKind::Ethernet));
            if add {
                if !target.addresses.contains(&address) {
                    target.addresses.push(address);
                }
            } else {
                target.addresses.retain(|candidate| *candidate != address);
            }
            upsert_interface(&mut persisted, target);
        }
        PersistChange::Route { route, add } => {
            if add {
                upsert_route(&mut persisted, route);
            } else {
                persisted
                    .routes
                    .retain(|candidate| candidate.name != route.name);
            }
        }
        PersistChange::BridgeCreate { bridge } => {
            upsert_interface(&mut persisted, bridge);
        }
        PersistChange::BridgeDelete { name } => {
            if let Some(mut iface) = effective.interface(&name).cloned() {
                iface.enabled = false;
                upsert_interface(&mut persisted, iface);
            } else {
                persisted.interfaces.retain(|iface| iface.name != name);
            }
        }
        PersistChange::BridgePort { bridge, port, add } => {
            let mut target = persisted
                .interface(&bridge)
                .cloned()
                .or_else(|| effective.interface(&bridge).cloned())
                .unwrap_or_else(|| Interface::new(bridge.clone(), InterfaceKind::Bridge));
            if add {
                if !target.ports.contains(&port) {
                    target.ports.push(port);
                }
            } else {
                target.ports.retain(|candidate| *candidate != port);
            }
            upsert_interface(&mut persisted, target);
        }
    }

    let previous = std::fs::read_to_string(&path).ok();
    let text = config::render_sections(&persisted);
    if text.trim().is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{}: {error}", path.display())),
        }
    } else {
        write_atomic(&path, &text)?;
    }
    match send(&options.control, &Request::Reload) {
        Ok(response) if response.ok => Ok(()),
        Ok(response) => {
            restore(&path, previous.as_deref());
            let _ = send(&options.control, &Request::Reload);
            Err(format!("reload rejected: {}", response.message))
        }
        Err(error) => {
            restore(&path, previous.as_deref());
            let _ = send(&options.control, &Request::Reload);
            Err(format!("reload failed: {error}"))
        }
    }
}

fn dropin_path(config: &Path) -> PathBuf {
    config
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("config.d")
        .join("90-rncli.conf")
}

fn upsert_interface(config: &mut Config, iface: Interface) {
    if let Some(existing) = config.interface_mut(&iface.name) {
        *existing = iface;
    } else {
        config.interfaces.push(iface);
    }
}

fn upsert_route(config: &mut Config, route: Route) {
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

fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, text).map_err(|e| format!("{}: {e}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|e| format!("{}: {e}", path.display()))
}

fn restore(path: &Path, previous: Option<&str>) {
    match previous {
        Some(text) => {
            let _ = write_atomic(path, text);
        }
        None => {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn iface_command(options: &Options, args: &[String]) -> ExitCode {
    let (Some(action), Some(iface)) = (args.first().map(String::as_str), args.get(1)) else {
        return usage("iface needs up|down and an interface");
    };
    let up = match action {
        "up" => true,
        "down" => false,
        other => return usage(&format!("unknown iface action '{other}'")),
    };
    mutate(
        options,
        Request::Iface {
            iface: iface.clone(),
            up,
        },
        Some(PersistChange::Iface {
            name: iface.clone(),
            up,
        }),
    )
}

fn addr_command(options: &Options, args: &[String]) -> ExitCode {
    let (Some(action), Some(iface), Some(address)) =
        (args.first().map(String::as_str), args.get(1), args.get(2))
    else {
        return usage("addr needs add|del, an interface and a CIDR");
    };
    let add = match action {
        "add" => true,
        "del" | "delete" => false,
        other => return usage(&format!("unknown addr action '{other}'")),
    };
    let address = match address.parse::<Ipv4Net>() {
        Ok(address) => address,
        Err(error) => return usage(&error),
    };
    mutate(
        options,
        Request::Address {
            iface: iface.clone(),
            address,
            add,
        },
        Some(PersistChange::Address {
            iface: iface.clone(),
            address,
            add,
        }),
    )
}

fn route_command(options: &Options, args: &[String]) -> ExitCode {
    let (Some(action), Some(name), Some(destination)) =
        (args.first().map(String::as_str), args.get(1), args.get(2))
    else {
        return usage("route needs add|del, a name and a destination");
    };
    let add = match action {
        "add" => true,
        "del" | "delete" => false,
        other => return usage(&format!("unknown route action '{other}'")),
    };
    let to = if destination.eq_ignore_ascii_case("default") {
        None
    } else {
        match destination.parse::<Ipv4Net>() {
            Ok(to) => Some(to),
            Err(error) => return usage(&error),
        }
    };
    let mut route = Route::new(name.clone());
    route.to = to;
    let mut index = 3usize;
    while let Some(key) = args.get(index) {
        let Some(value) = args.get(index + 1) else {
            return usage(&format!("route option '{key}' needs a value"));
        };
        match key.as_str() {
            "via" => match value.parse::<Ipv4Addr>() {
                Ok(via) => route.via = Some(via),
                Err(_) => return usage("via must be an IPv4 address"),
            },
            "dev" => route.dev = Some(value.clone()),
            "metric" => match value.parse::<u32>() {
                Ok(metric) => route.metric = metric,
                Err(_) => return usage("metric must be a number"),
            },
            "table" => {
                route.table = if value == "main" {
                    RT_TABLE_MAIN
                } else {
                    match value.parse::<u32>() {
                        Ok(table) => table,
                        Err(_) => return usage("table must be 'main' or a number"),
                    }
                };
            }
            "src" => match value.parse::<Ipv4Addr>() {
                Ok(src) => route.src = Some(src),
                Err(_) => return usage("src must be an IPv4 address"),
            },
            "onlink" => match value.as_str() {
                "yes" => route.onlink = true,
                "no" => route.onlink = false,
                _ => return usage("onlink expects yes or no"),
            },
            other => return usage(&format!("unknown route option '{other}'")),
        }
        index += 2;
    }
    mutate(
        options,
        Request::Route {
            route: route.clone(),
            add,
        },
        Some(PersistChange::Route { route, add }),
    )
}

fn bridge_command(options: &Options, args: &[String]) -> ExitCode {
    let Some(action) = args.first().map(String::as_str) else {
        return usage("bridge needs create, delete, add-port or del-port");
    };
    match action {
        "create" => {
            let Some(name) = args.get(1) else {
                return usage("bridge create needs a name");
            };
            let mut bridge = Interface::new(name.clone(), InterfaceKind::Bridge);
            let mut index = 2usize;
            while let Some(key) = args.get(index) {
                let Some(value) = args.get(index + 1) else {
                    return usage(&format!("bridge option '{key}' needs a value"));
                };
                match key.as_str() {
                    "stp" => match value.as_str() {
                        "yes" => bridge.stp = true,
                        "no" => bridge.stp = false,
                        _ => return usage("stp expects yes or no"),
                    },
                    "forward-delay" => match value.parse::<u32>() {
                        Ok(value) => bridge.forward_delay = value,
                        Err(_) => return usage("forward-delay must be a number"),
                    },
                    "hello-time" => match value.parse::<u32>() {
                        Ok(value) => bridge.hello_time = value,
                        Err(_) => return usage("hello-time must be a number"),
                    },
                    "max-age" => match value.parse::<u32>() {
                        Ok(value) => bridge.max_age = value,
                        Err(_) => return usage("max-age must be a number"),
                    },
                    "mtu" => match value.parse::<u32>() {
                        Ok(value) => bridge.mtu = Some(value),
                        Err(_) => return usage("mtu must be a number"),
                    },
                    other => return usage(&format!("unknown bridge option '{other}'")),
                }
                index += 2;
            }
            mutate(
                options,
                Request::BridgeCreate {
                    bridge: bridge.clone(),
                },
                Some(PersistChange::BridgeCreate { bridge }),
            )
        }
        "delete" => {
            let Some(name) = args.get(1) else {
                return usage("bridge delete needs a name");
            };
            mutate(
                options,
                Request::BridgeDelete { name: name.clone() },
                Some(PersistChange::BridgeDelete { name: name.clone() }),
            )
        }
        "add-port" | "del-port" => {
            let (Some(bridge), Some(port)) = (args.get(1), args.get(2)) else {
                return usage("bridge add-port|del-port needs a bridge and a port");
            };
            let add = action == "add-port";
            mutate(
                options,
                Request::BridgePort {
                    bridge: bridge.clone(),
                    port: port.clone(),
                    add,
                },
                Some(PersistChange::BridgePort {
                    bridge: bridge.clone(),
                    port: port.clone(),
                    add,
                }),
            )
        }
        other => usage(&format!("unknown bridge action '{other}'")),
    }
}

fn dhcp_command(options: &Options, args: &[String]) -> ExitCode {
    let (Some(action), Some(iface)) = (args.first().map(String::as_str), args.get(1)) else {
        return usage("dhcp needs renew|release and an interface");
    };
    let renew = match action {
        "renew" => true,
        "release" => false,
        other => return usage(&format!("unknown dhcp action '{other}'")),
    };
    mutate(
        options,
        Request::Dhcp {
            iface: iface.clone(),
            renew,
        },
        None,
    )
}

fn wait_online(options: &Options, args: &[String]) -> ExitCode {
    let config = match config::load_effective(&options.config) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("rncli: {}: {error}", options.config.display());
            return ExitCode::from(2);
        }
    };
    if config.global.wait_online == WaitOnline::None {
        return ExitCode::SUCCESS;
    }
    let mut timeout = config.global.wait_online_timeout;
    let mut index = 0usize;
    while let Some(arg) = args.get(index) {
        if arg == "--timeout" {
            let Some(value) = args.get(index + 1) else {
                return usage("--timeout needs a number");
            };
            timeout = match value.parse::<u64>() {
                Ok(value) => value,
                Err(_) => return usage("--timeout must be a number"),
            };
            index += 2;
        } else {
            return usage(&format!("unknown wait-online option '{arg}'"));
        }
    }
    let deadline = Instant::now() + Duration::from_secs(timeout);
    loop {
        let response = match send(&options.control, &Request::Status { iface: None }) {
            Ok(response) => response,
            Err(error) => {
                if Instant::now() >= deadline {
                    eprintln!("rncli: wait-online timed out after {timeout}s: {error}");
                    return ExitCode::from(1);
                }
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        if !response.ok {
            if Instant::now() >= deadline {
                eprintln!("rncli: {}: wait-online timed out", response.message);
                return ExitCode::from(response.code.clamp(1, 3) as u8);
            }
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        let groups = group_fields(&response.fields);
        let eligible: Vec<&Interface> = config
            .interfaces
            .iter()
            .filter(|iface| iface.wait_online_enabled(&config.global))
            .collect();
        if eligible.is_empty() {
            return ExitCode::SUCCESS;
        }
        let online = eligible
            .iter()
            .filter(|iface| group_value(&groups, &iface.name, "online") == Some("yes"))
            .count();
        let missing_required = eligible.iter().any(|iface| {
            iface.required && group_value(&groups, &iface.name, "state") == Some("missing")
        });
        let ready = match config.global.wait_online {
            WaitOnline::Any => online > 0,
            WaitOnline::All => online == eligible.len(),
            WaitOnline::None => true,
        };
        if ready && !missing_required {
            return ExitCode::SUCCESS;
        }
        if Instant::now() >= deadline {
            eprintln!("rncli: wait-online timed out after {timeout}s");
            return ExitCode::from(1);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn send(path: &Path, request: &Request) -> Result<Response, String> {
    let mut stream = UnixStream::connect(path)
        .map_err(|e| format!("cannot connect to {}: {e}", path.display()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| format!("cannot set read timeout: {e}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| format!("cannot set write timeout: {e}"))?;
    stream
        .write_all(&frame(&encode_request(request)))
        .map_err(|e| format!("cannot send request: {e}"))?;
    let mut prefix = [0u8; 4];
    stream
        .read_exact(&mut prefix)
        .map_err(|e| format!("rnetd did not answer: {e}"))?;
    let length = frame_len(prefix).map_err(|e| format!("bad response frame: {e}"))?;
    let mut body = vec![0u8; length];
    stream
        .read_exact(&mut body)
        .map_err(|e| format!("truncated response: {e}"))?;
    decode_response(&body).map_err(|e| format!("malformed response: {e}"))
}

fn group_fields(fields: &[(String, String)]) -> Vec<Vec<(String, String)>> {
    let mut groups: Vec<Vec<(String, String)>> = Vec::new();
    for (key, value) in fields {
        if key == "iface" {
            groups.push(Vec::new());
        }
        if let Some(group) = groups.last_mut() {
            group.push((key.clone(), value.clone()));
        }
    }
    groups
}

fn group_value<'a>(groups: &'a [Vec<(String, String)>], iface: &str, key: &str) -> Option<&'a str> {
    groups
        .iter()
        .find(|group| {
            group
                .iter()
                .any(|(candidate, value)| candidate == "iface" && value == iface)
        })
        .and_then(|group| {
            group
                .iter()
                .find(|(candidate, _)| candidate == key)
                .map(|(_, value)| value.as_str())
        })
}

fn print_status(fields: &[(String, String)]) -> ExitCode {
    let groups = group_fields(fields);
    if groups.is_empty() {
        println!("no interfaces");
        return ExitCode::SUCCESS;
    }
    println!(
        "{:<10} {:<7} {:<10} {:<28} ONLINE",
        "IFACE", "STATE", "KIND", "ADDRESSES"
    );
    for group in &groups {
        let name = field(group, "iface").unwrap_or("-");
        let state = field(group, "state").unwrap_or("-");
        let kind = field(group, "kind").unwrap_or("-");
        let online = field(group, "online").unwrap_or("-");
        let addresses: Vec<&str> = group
            .iter()
            .filter(|(key, _)| key == "address")
            .map(|(_, value)| value.as_str())
            .collect();
        let addresses = if addresses.is_empty() {
            "-".to_string()
        } else {
            addresses.join(",")
        };
        println!("{name:<10} {state:<7} {kind:<10} {addresses:<28} {online}");
    }
    ExitCode::SUCCESS
}

fn field<'a>(group: &'a [(String, String)], key: &str) -> Option<&'a str> {
    group
        .iter()
        .find(|(candidate, _)| candidate == key)
        .map(|(_, value)| value.as_str())
}

fn usage(message: &str) -> ExitCode {
    eprintln!("rncli: {message}\n\n{USAGE}");
    ExitCode::from(2)
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

    use super::*;
    use crate::control::{decode_request, encode_response};

    #[test]
    fn dropin_path_is_next_to_the_main_file() {
        assert_eq!(
            dropin_path(Path::new("/etc/rnet/config")),
            PathBuf::from("/etc/rnet/config.d/90-rncli.conf")
        );
    }

    #[test]
    fn upsert_replaces_sections_by_name() {
        let mut config = Config::default();
        let mut iface = Interface::new("eth0".to_string(), InterfaceKind::Ethernet);
        iface.enabled = true;
        upsert_interface(&mut config, iface.clone());
        iface.enabled = false;
        upsert_interface(&mut config, iface);
        assert_eq!(config.interfaces.len(), 1);
        assert!(!config.interfaces[0].enabled);

        let mut route = Route::new("lab".to_string());
        route.to = Some("10.0.0.0/8".parse().expect("net"));
        route.dev = Some("eth0".to_string());
        upsert_route(&mut config, route.clone());
        route.metric = 100;
        upsert_route(&mut config, route);
        assert_eq!(config.routes.len(), 1);
        assert_eq!(config.routes[0].metric, 100);
    }

    #[test]
    fn atomic_write_replaces_the_file() {
        let root = std::env::temp_dir().join(format!("rnet-cli-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("config.d/90-rncli.conf");
        write_atomic(&path, "one\n").expect("write");
        write_atomic(&path, "two\n").expect("replace");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "two\n");
        let _ = std::fs::remove_dir_all(root);
    }

    fn fake_server(path: &Path, responses: usize) -> std::thread::JoinHandle<()> {
        let listener = UnixListener::bind(path).expect("fake control listener");
        std::thread::spawn(move || {
            for _ in 0..responses {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut prefix = [0u8; 4];
                stream.read_exact(&mut prefix).expect("prefix");
                let length = frame_len(prefix).expect("frame length");
                let mut body = vec![0u8; length];
                stream.read_exact(&mut body).expect("body");
                let _ = decode_request(&body).expect("request");
                let response = Response::ok();
                stream
                    .write_all(&frame(&encode_response(&response)))
                    .expect("response");
            }
        })
    }

    #[test]
    fn send_round_trips_through_a_control_socket() {
        let root = std::env::temp_dir().join(format!("rnet-cli-send-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp");
        let socket = root.join("control.sock");
        let server = fake_server(&socket, 1);
        let options = Options {
            config: root.join("config"),
            control: socket,
            porcelain: true,
            persist: false,
        };
        // The fake server replies ok to this request.
        let response = send(&options.control, &Request::Status { iface: None }).expect("send");
        assert!(response.ok);
        server.join().expect("server");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn persist_writes_and_removes_a_dropin() {
        let root = std::env::temp_dir().join(format!("rnet-cli-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp");
        let config = root.join("config");
        std::fs::write(
            &config,
            "[global]\nmanage-loopback = no\n\n[interface eth0]\ntype = ethernet\nenabled = yes\n",
        )
        .expect("config");
        let socket = root.join("control.sock");
        let server = fake_server(&socket, 2);
        let options = Options {
            config: config.clone(),
            control: socket,
            porcelain: false,
            persist: true,
        };
        let route = Route {
            name: "lab".to_string(),
            to: Some("10.0.0.0/8".parse().expect("net")),
            via: None,
            dev: Some("eth0".to_string()),
            metric: 0,
            table: RT_TABLE_MAIN,
            src: None,
            onlink: false,
        };
        persist(
            &options,
            PersistChange::Route {
                route: route.clone(),
                add: true,
            },
        )
        .expect("persist add");
        let dropin = dropin_path(&config);
        let text = std::fs::read_to_string(&dropin).expect("dropin");
        assert!(text.contains("[route lab]"));
        persist(&options, PersistChange::Route { route, add: false }).expect("persist delete");
        assert!(!dropin.exists());
        server.join().expect("server");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn groups_split_on_iface() {
        let fields = vec![
            ("iface".to_string(), "eth0".to_string()),
            ("state".to_string(), "up".to_string()),
            ("iface".to_string(), "br0".to_string()),
            ("state".to_string(), "down".to_string()),
        ];
        let groups = group_fields(&fields);
        assert_eq!(groups.len(), 2);
        assert_eq!(group_value(&groups, "br0", "state"), Some("down"));
    }
}
