//! Command-line parsing for the PID 1 binary.
//!
//! Hand-rolled (no clap) to keep the M0 binary small (plan §2 "依赖清单").

use std::ffi::OsString;

/// Environment variable that overrides the hardcoded test service.
pub const ENV_TEST_SERVICE: &str = "RINIT_TEST_SERVICE";

/// Default hardcoded test service (plan §3 M0).
pub const DEFAULT_TEST_SERVICE: &str = "/sbin/rinit-testservice";

/// Parsed startup configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Print version and exit.
    pub show_version: bool,
    /// Print usage and exit.
    pub show_help: bool,
    /// Skip the first-wave mounts and never reboot (host/dev smoke testing).
    pub skip_mounts: bool,
    /// The hardcoded service as an argv vector (M0; no config parsing yet).
    pub service_argv: Vec<String>,
}

/// Parse arguments (excluding `argv[0]`).
///
/// The service command can be given either as `--test-service "<cmd>"` or, for
/// the kernel cmdline (which splits on spaces), as everything after a literal
/// `--`: `... rdinit=/sbin/rinit -- /sbin/rinit-testservice --ticks 8`.
///
/// # Errors
/// Returns a human-readable message on an unknown flag or a missing value.
pub fn parse<I: IntoIterator<Item = OsString>>(args: I) -> Result<Config, String> {
    let default_service: Vec<String> = std::env::var(ENV_TEST_SERVICE)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_else(|| vec![DEFAULT_TEST_SERVICE.to_string()]);

    let mut cfg = Config {
        show_version: false,
        show_help: false,
        skip_mounts: false,
        service_argv: default_service,
    };

    let mut iter = args.into_iter();
    while let Some(arg_os) = iter.next() {
        let arg = arg_os.to_string_lossy();
        match arg.as_ref() {
            "-h" | "--help" => cfg.show_help = true,
            "-V" | "--version" => cfg.show_version = true,
            "--dev" => cfg.skip_mounts = true,
            "--" => {
                // Everything after `--` is the service command and its args.
                cfg.service_argv = iter
                    .by_ref()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect();
                break;
            }
            "--test-service" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--test-service requires a value".to_string())?;
                cfg.service_argv = value
                    .to_string_lossy()
                    .split_whitespace()
                    .map(String::from)
                    .collect();
            }
            other => {
                if let Some(value) = other.strip_prefix("--test-service=") {
                    cfg.service_argv = value.split_whitespace().map(String::from).collect();
                } else if other.starts_with('-') {
                    return Err(format!("unknown argument: {other}"));
                } else {
                    // First non-flag argument starts the service command; the
                    // rest (including flags like `--ticks`) belong to it. This
                    // is how the kernel passes init args after `--` on its
                    // cmdline, e.g. `rdinit=/sbin/rinit /sbin/svc --ticks 6`.
                    cfg.service_argv = std::iter::once(other.to_string())
                        .chain(iter.by_ref().map(|a| a.to_string_lossy().into_owned()))
                        .collect();
                    break;
                }
            }
        }
    }

    Ok(cfg)
}

/// Usage text.
pub const USAGE: &str = "\
Usage: rinit [OPTIONS]

A lightweight PID 1 init and service manager.

Options:
  -h, --help                 Show this help and exit
  -V, --version              Show version and exit
      --dev                  Skip mounts and do not reboot (dev/smoke testing)
      --test-service <CMD>   Hardcoded service to launch (M0); whitespace-split
      -- <SERVICE> [ARGS]    Hardcoded service as an argv (kernel-cmdline form)

Environment:
  RINIT_TEST_SERVICE         Default for --test-service
";
