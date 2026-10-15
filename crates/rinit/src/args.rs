//! Command-line parsing for the PID 1 binary.
//!
//! Hand-rolled (no clap) to keep the M0 binary small (plan §2 "依赖清单").
//!
//! The parser is split into two layers so the interesting half is testable
//! without touching the process environment:
//!
//! - [`parse`] is the entry point: it reads [`ENV_TEST_SERVICE`] and delegates.
//! - [`parse_with_default`] is pure - given argv and an explicit default
//!   service, it returns a [`Config`] or a human-readable error.
//!
//! `libtest` runs tests in parallel inside one process, and `set_var` is
//! `unsafe` in edition 2024, so mutating the environment from a test would be
//! both racy and unsound. Tests therefore drive [`parse_with_default`] and
//! [`service_argv_from_env`] directly.

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

/// Parse arguments (excluding `argv[0]`), honouring [`ENV_TEST_SERVICE`].
///
/// # Errors
/// See [`parse_with_default`]; the only additional failure mode is an empty
/// resolved service command, which is reported identically.
pub fn parse<I: IntoIterator<Item = OsString>>(args: I) -> Result<Config, String> {
    let default = service_argv_from_env(std::env::var(ENV_TEST_SERVICE).ok());
    parse_with_default(args, default)
}

/// Resolve the default service argv from an environment value.
///
/// `None`, or a value that is empty or only whitespace, falls back to
/// [`DEFAULT_TEST_SERVICE`]. Otherwise the value is whitespace-split, matching
/// the kernel cmdline form. Splitting here (rather than at spawn time) means
/// `RINIT_TEST_SERVICE="  "` cannot produce an empty argv.
fn service_argv_from_env(value: Option<String>) -> Vec<String> {
    value
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_else(|| vec![DEFAULT_TEST_SERVICE.to_string()])
}

/// Parse arguments against an explicit default service.
///
/// The service command can be given as `--test-service "<cmd>"`,
/// `--test-service=<cmd>`, everything after a literal `--`, or - the form the
/// kernel actually uses when it splits `init=` arguments on spaces - as the
/// first non-flag argument with everything after it.
///
/// # Errors
/// Returns a human-readable message for an unknown flag, a `--test-service`
/// with no value, or a service command that ends up empty.
fn parse_with_default<I: IntoIterator<Item = OsString>>(
    args: I,
    default_service_argv: Vec<String>,
) -> Result<Config, String> {
    let mut cfg = Config {
        show_version: false,
        show_help: false,
        skip_mounts: false,
        service_argv: default_service_argv,
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

    // Every path above can leave an empty argv (`--test-service ""`, a bare
    // `--`, ...). Reject it here rather than letting `posix_spawn` fail later
    // with a confusing ENOENT on an empty path.
    match cfg.service_argv.split_first() {
        None => return Err("no service command given".to_string()),
        Some((program, _)) if program.is_empty() => {
            return Err("service command has an empty program path".to_string());
        }
        Some(_) => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse with the built-in default service, so tests never depend on the
    /// ambient `RINIT_TEST_SERVICE` of the developer's shell.
    fn parse_args(args: &[&str]) -> Result<Config, String> {
        parse_with_default(
            args.iter().map(OsString::from),
            vec![DEFAULT_TEST_SERVICE.to_string()],
        )
    }

    fn service(cfg: &Config) -> &[String] {
        &cfg.service_argv
    }

    // --- defaults and simple flags ------------------------------------

    #[test]
    fn no_arguments_yields_defaults() {
        let cfg = parse_args(&[]).expect("parse");
        assert!(!cfg.show_help);
        assert!(!cfg.show_version);
        assert!(!cfg.skip_mounts);
        assert_eq!(service(&cfg), ["/sbin/rinit-testservice"]);
    }

    #[test]
    fn parses_help_and_version_flags() {
        for flag in ["-h", "--help"] {
            assert!(parse_args(&[flag]).expect("parse").show_help, "{flag}");
        }
        for flag in ["-V", "--version"] {
            assert!(parse_args(&[flag]).expect("parse").show_version, "{flag}");
        }
    }

    #[test]
    fn parses_dev_flag() {
        assert!(parse_args(&["--dev"]).expect("parse").skip_mounts);
    }

    /// `-h` and `-V` may both appear; `main` gives help precedence, so both
    /// being set is valid rather than an error.
    #[test]
    fn help_and_version_may_combine() {
        let cfg = parse_args(&["--version", "--help"]).expect("parse");
        assert!(cfg.show_help);
        assert!(cfg.show_version);
    }

    // --- service command forms ----------------------------------------

    #[test]
    fn test_service_value_is_whitespace_split() {
        let cfg = parse_args(&["--test-service", "/bin/sh -c true"]).expect("parse");
        assert_eq!(service(&cfg), ["/bin/sh", "-c", "true"]);
    }

    #[test]
    fn test_service_equals_form_is_split() {
        let cfg = parse_args(&["--test-service=/bin/sh -c true"]).expect("parse");
        assert_eq!(service(&cfg), ["/bin/sh", "-c", "true"]);
    }

    #[test]
    fn double_dash_takes_the_remainder_verbatim() {
        // Everything after `--` belongs to the service, including things that
        // would otherwise parse as rinit's own flags.
        let cfg = parse_args(&["--", "/bin/sh", "--dev", "-V", "--"]).expect("parse");
        assert_eq!(service(&cfg), ["/bin/sh", "--dev", "-V", "--"]);
        assert!(!cfg.show_version, "service args must not set rinit flags");
        assert!(!cfg.skip_mounts, "service args must not set rinit flags");
    }

    /// The form the kernel actually produces: `init=/sbin/rinit` arguments are
    /// split on spaces, so the first non-flag token starts the service and the
    /// rest are its own flags.
    #[test]
    fn bare_positional_forms_the_service_command() {
        let cfg = parse_args(&["/sbin/rinit-testservice", "--ticks", "6"]).expect("parse");
        assert_eq!(service(&cfg), ["/sbin/rinit-testservice", "--ticks", "6"]);
    }

    #[test]
    fn rinit_flags_before_the_service_are_still_parsed() {
        let cfg = parse_args(&["--dev", "/bin/sh", "--ticks", "1"]).expect("parse");
        assert!(cfg.skip_mounts);
        assert_eq!(service(&cfg), ["/bin/sh", "--ticks", "1"]);
    }

    /// A later `--test-service` overrides an earlier one.
    #[test]
    fn last_service_flag_wins() {
        let cfg = parse_args(&["--test-service", "/bin/first", "--test-service=/bin/second"])
            .expect("parse");
        assert_eq!(service(&cfg), ["/bin/second"]);
    }

    // --- errors --------------------------------------------------------

    #[test]
    fn test_service_without_a_value_is_an_error() {
        let err = parse_args(&["--test-service"]).unwrap_err();
        assert!(err.contains("--test-service"), "got: {err}");
    }

    #[test]
    fn unknown_flags_are_rejected() {
        for flag in ["-x", "--nope", "-", "--"] {
            // `--` alone is handled separately (it is not an unknown flag),
            // so only assert on genuinely unknown flags.
            if flag == "--" {
                continue;
            }
            let err = parse_args(&[flag]).unwrap_err();
            assert!(err.contains("unknown argument"), "{flag}: got: {err}");
        }
    }

    /// An unknown flag after the service command belongs to the service, so it
    /// is not an error.
    #[test]
    fn unknown_flags_after_the_service_are_the_services_own() {
        let cfg = parse_args(&["/bin/sh", "--totally-unknown"]).expect("parse");
        assert_eq!(service(&cfg), ["/bin/sh", "--totally-unknown"]);
    }

    /// Regression: every form can yield an empty argv. Left unchecked, this
    /// reached `posix_spawn` as an empty path and surfaced as ENOENT.
    #[test]
    fn empty_service_commands_are_rejected() {
        for args in [
            vec!["--test-service", ""],
            vec!["--test-service", "   "],
            vec!["--test-service="],
            vec!["--test-service=   "],
            vec!["--"],
        ] {
            let err = parse_args(&args).unwrap_err();
            assert!(
                err.contains("service command"),
                "{args:?} should be rejected, got: {err}"
            );
        }
    }

    /// `--` keeps empty tokens, so an empty program path can reach argv even
    /// when the command is otherwise non-empty.
    #[test]
    fn empty_program_path_is_rejected() {
        let err = parse_args(&["--", "", "/bin/sh"]).unwrap_err();
        assert!(err.contains("empty program path"), "got: {err}");
    }

    /// A bare empty positional argument is the same hazard via the
    /// positional form.
    #[test]
    fn empty_positional_program_is_rejected() {
        let err = parse_args(&[""]).unwrap_err();
        assert!(err.contains("empty program path"), "got: {err}");
    }

    /// The default comes from the caller, not from the environment, when
    /// nothing on the command line overrides it.
    #[test]
    fn explicit_default_is_used_when_no_flag_given() {
        let cfg = parse_with_default(
            ["--dev"].iter().map(OsString::from),
            vec!["/custom/default".to_string()],
        )
        .expect("parse");
        assert_eq!(service(&cfg), ["/custom/default"]);
    }

    // --- environment resolution ---------------------------------------

    #[test]
    fn env_value_becomes_the_default_service() {
        let argv = service_argv_from_env(Some("/bin/sh -c true".to_string()));
        assert_eq!(argv, ["/bin/sh", "-c", "true"]);
    }

    /// A blank environment variable must fall back to the built-in default
    /// rather than producing an empty argv that later fails at spawn time.
    #[test]
    fn blank_env_value_falls_back_to_the_builtin_default() {
        for value in ["", "   ", "\t\n"] {
            let argv = service_argv_from_env(Some(value.to_string()));
            assert_eq!(argv, [DEFAULT_TEST_SERVICE], "value: {value:?}");
        }
    }

    #[test]
    fn missing_env_value_falls_back_to_the_builtin_default() {
        assert_eq!(service_argv_from_env(None), [DEFAULT_TEST_SERVICE]);
    }

    /// An explicit flag must beat whatever the environment says.
    #[test]
    fn flag_overrides_the_environment_default() {
        let cfg = parse_with_default(
            ["--test-service", "/bin/from-flag"]
                .iter()
                .map(OsString::from),
            vec!["/bin/from-env".to_string()],
        )
        .expect("parse");
        assert_eq!(service(&cfg), ["/bin/from-flag"]);
    }

    // --- usage text ---------------------------------------------------

    /// The usage string is the only documentation a stuck operator sees; every
    /// flag the parser accepts must appear in it.
    #[test]
    fn usage_documents_every_flag() {
        for flag in [
            "-h",
            "--help",
            "-V",
            "--version",
            "--dev",
            "--test-service",
            ENV_TEST_SERVICE,
        ] {
            assert!(USAGE.contains(flag), "usage is missing {flag}");
        }
    }
}
