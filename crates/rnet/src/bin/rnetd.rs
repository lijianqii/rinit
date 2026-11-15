use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
Usage: rnetd [options]

Options:
  --config PATH          Configuration file (default /etc/rnet/config)
  --control PATH         Control socket (default /run/rnet/control.sock)
  --state PATH           Applied-state file (default /run/rnet/state)
  --leases PATH          Lease directory (default /run/rnet/leases)
  --resolv-conf PATH     Resolver file (default /etc/resolv.conf)
  --once                 Apply the configuration once and exit
  -h, --help             Show this help
  -V, --version          Show the version
";

fn main() -> ExitCode {
    let mut config = PathBuf::from(rnet::DEFAULT_CONFIG);
    let mut control = PathBuf::from(rnet::DEFAULT_CONTROL_SOCKET);
    let mut state = PathBuf::from("/run/rnet/state");
    let mut leases = PathBuf::from("/run/rnet/leases");
    let mut resolv = PathBuf::from("/etc/resolv.conf");
    let mut once = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                let Some(value) = args.next() else {
                    return usage("--config needs a path");
                };
                config = PathBuf::from(value);
            }
            "--control" => {
                let Some(value) = args.next() else {
                    return usage("--control needs a path");
                };
                control = PathBuf::from(value);
            }
            "--state" => {
                let Some(value) = args.next() else {
                    return usage("--state needs a path");
                };
                state = PathBuf::from(value);
            }
            "--leases" => {
                let Some(value) = args.next() else {
                    return usage("--leases needs a path");
                };
                leases = PathBuf::from(value);
            }
            "--resolv-conf" => {
                let Some(value) = args.next() else {
                    return usage("--resolv-conf needs a path");
                };
                resolv = PathBuf::from(value);
            }
            "--once" => once = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "-V" | "--version" => {
                println!("rnetd {}", rnet::VERSION);
                return ExitCode::SUCCESS;
            }
            other => return usage(&format!("unknown option '{other}'")),
        }
    }
    match rnet::runtime::run(&config, &control, &state, &leases, &resolv, once) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rnetd: {error}");
            ExitCode::from(1)
        }
    }
}

fn usage(message: &str) -> ExitCode {
    eprintln!("rnetd: {message}\n\n{USAGE}");
    ExitCode::from(2)
}
