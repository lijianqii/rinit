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
use std::path::PathBuf;

/// Environment variable that overrides the hardcoded test service.
pub const ENV_TEST_SERVICE: &str = "RINIT_TEST_SERVICE";

/// Parsed startup configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Print version and exit.
    pub show_version: bool,
    /// Print usage and exit.
    pub show_help: bool,
    /// Skip the first-wave mounts and never reboot (host/dev smoke testing).
    pub skip_mounts: bool,
    /// The hardcoded service as an argv vector.
    ///
    /// M1 replaced the single hardcoded service with the unit directory; this
    /// stays as an explicit override that bypasses unit loading entirely, for
    /// bring-up and the `--dev` smoke test.
    pub service_argv: Vec<String>,
    /// Where to look for unit files (M1).
    pub unit_dir: PathBuf,
    /// Which target to boot (M1; plan §7.2 default target).
    pub default_target: String,
    /// The mounted cgroup v2 hierarchy (plan §7.3), or `None` for the default.
    ///
    /// `--cgroup-root <DIR>` names where the hierarchy is, for a system that
    /// mounts it somewhere other than `/sys/fs/cgroup` - and for the host
    /// integration tests, which drive the whole limit path against a directory
    /// of ordinary files without needing root or cgroups.
    pub cgroup_root: Option<PathBuf>,
    /// The watchdog device (plan §7.5), or `None` for the default.
    ///
    /// `--watchdog <DEVICE>` names the device, which is how a board with the
    /// watchdog somewhere unusual is served - and how the host tests drive the
    /// feed/magic-close protocol against a file instead of against hardware.
    /// The default path is never opened in `--dev`.
    pub watchdog_device: Option<PathBuf>,
    /// The hardware timeout rinit asks for and feeds against (plan §7.5).
    ///
    /// `--watchdog-timeout <SECONDS>`; the feed interval is half of it.
    pub watchdog_timeout_sec: u64,
    /// Where to mirror captured service output (plan §1.3(7)).
    ///
    /// `--log-file <PATH>`; `None` keeps it in memory only, which an empty value
    /// or the word `none` selects. The default is the plan's own path - and it
    /// is a *mirror* of the ring, not a replacement, so `rctl log` does not
    /// depend on it.
    pub log_file: Option<PathBuf>,
}

/// Where captured service output is persisted by default (plan §1.3(7)).
pub const DEFAULT_LOG_FILE: &str = "/var/log/rinit/log";

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
/// `None`, or a value that is empty or only whitespace, yields an **empty**
/// argv - which from M1 onward means "start the unit directory". Injecting
/// [`DEFAULT_TEST_SERVICE`] here instead would make the unit path unreachable
/// in practice, since a default is always present.
///
/// A whitespace-only value is treated as unset rather than as an empty
/// command, so `RINIT_TEST_SERVICE="  "` cannot silently disable the unit
/// directory.
fn service_argv_from_env(value: Option<String>) -> Vec<String> {
    value
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_default()
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
        unit_dir: PathBuf::from(rinit_runtime::DEFAULT_UNIT_DIR),
        default_target: rinit_runtime::DEFAULT_TARGET.to_string(),
        cgroup_root: None,
        watchdog_device: None,
        watchdog_timeout_sec: rinit_runtime::syscall::watchdog::DEFAULT_TIMEOUT_SEC,
        log_file: Some(PathBuf::from(DEFAULT_LOG_FILE)),
    };

    let mut iter = args.into_iter();
    while let Some(arg_os) = iter.next() {
        let arg = arg_os.to_string_lossy();
        match arg.as_ref() {
            "-h" | "--help" => cfg.show_help = true,
            "-V" | "--version" => cfg.show_version = true,
            "--dev" => cfg.skip_mounts = true,
            "--units" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--units requires a directory".to_string())?;
                cfg.unit_dir = PathBuf::from(value.to_string_lossy().into_owned());
            }
            "--default" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--default requires a target unit".to_string())?;
                cfg.default_target = value.to_string_lossy().into_owned();
                validate_target(&cfg.default_target)?;
            }
            "--cgroup-root" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--cgroup-root requires a directory".to_string())?;
                cfg.cgroup_root = Some(PathBuf::from(value.to_string_lossy().into_owned()));
            }
            "--watchdog" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--watchdog requires a device path".to_string())?;
                cfg.watchdog_device = Some(PathBuf::from(value.to_string_lossy().into_owned()));
            }
            "--log-file" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--log-file requires a path".to_string())?;
                cfg.log_file = log_file_path(&value.to_string_lossy());
            }
            "--watchdog-timeout" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--watchdog-timeout requires a number of seconds".to_string())?;
                cfg.watchdog_timeout_sec = parse_seconds(&value.to_string_lossy())?;
            }
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
                } else if let Some(value) = other.strip_prefix("--units=") {
                    cfg.unit_dir = PathBuf::from(value);
                } else if let Some(value) = other.strip_prefix("--default=") {
                    cfg.default_target = value.to_string();
                    validate_target(&cfg.default_target)?;
                } else if let Some(value) = other.strip_prefix("--cgroup-root=") {
                    cfg.cgroup_root = Some(PathBuf::from(value));
                } else if let Some(value) = other.strip_prefix("--log-file=") {
                    cfg.log_file = log_file_path(value);
                } else if let Some(value) = other.strip_prefix("--watchdog=") {
                    cfg.watchdog_device = Some(PathBuf::from(value));
                } else if let Some(value) = other.strip_prefix("--watchdog-timeout=") {
                    cfg.watchdog_timeout_sec = parse_seconds(value)?;
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

    /// Reject a boot target that could never name a unit.
    ///
    /// Validating here turns a typo into a usage error, instead of a boot that
    /// fails minutes later with "target not found".
    fn validate_target(target: &str) -> Result<(), String> {
        rinit_core::UnitName::parse(target)
            .map(|_| ())
            .map_err(|e| format!("--default: {e}"))
    }

    /// The path `--log-file` names, or `None` for "do not persist".
    ///
    /// `none` is spelled out rather than only accepting an empty value because a
    /// kernel command line cannot easily carry an empty argument - and the two
    /// spellings meaning the same thing is one fewer difference between a boot
    /// and a `make run`.
    fn log_file_path(value: &str) -> Option<PathBuf> {
        let value = value.trim();
        if value.is_empty() || value.eq_ignore_ascii_case("none") {
            return None;
        }
        Some(PathBuf::from(value))
    }

    /// A whole number of seconds, for `--watchdog-timeout`.
    ///
    /// A watchdog timeout is the one interval where a typo is dangerous rather
    /// than merely wrong: `30s` or `-1` silently becoming a default would arm the
    /// hardware for a length of time nobody chose. So a value that is not a plain
    /// non-negative integer is a usage error, not a fallback - which is also why
    /// the leading `+` and the surrounding whitespace `str::parse` accepts are
    /// rejected here instead of being quietly accepted.
    ///
    /// Zero is rejected too, and it is the one value that is a whole number:
    /// there is no such thing as a watchdog that fires after no seconds, and
    /// accepting it would arm a device whose feed interval rounds to
    /// milliseconds - a supervisor loop spinning at a kilohertz to feed a timer
    /// that was never going to be satisfied.
    fn parse_seconds(value: &str) -> Result<u64, String> {
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!(
                "--watchdog-timeout: '{value}' is not a whole number of seconds"
            ));
        }
        match value.parse::<u64>() {
            Ok(0) => Err("--watchdog-timeout: 0 seconds is not a watchdog timeout".to_string()),
            Ok(seconds) => Ok(seconds),
            Err(_) => Err(format!("--watchdog-timeout: '{value}' is out of range")),
        }
    }

    // An empty argv is legitimate from M1 onwards: a normal boot starts the
    // unit directory, not a hardcoded command. It is only a *bad program
    // path* if one was explicitly given and it is empty, since that would
    // reach `posix_spawn` as an empty path and fail with a confusing ENOENT.
    if let Some((program, _)) = cfg.service_argv.split_first()
        && program.is_empty()
    {
        return Err("service command has an empty program path".to_string());
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
      --dev                  Skip mounts, do not reboot, and accept control
                             commands from the invoking user
      --units <DIR>          Unit directory (default: /etc/rinit/units)
      --default <TARGET>     Boot target (default: multi-user.target)
      --cgroup-root <DIR>    Mounted cgroup v2 hierarchy for unit limits
                             (default: /sys/fs/cgroup)
      --watchdog <DEVICE>    Hardware watchdog to arm and feed
                             (default: /dev/watchdog; not opened in --dev)
      --watchdog-timeout <S> Hardware timeout in seconds; fed every half
                             (default: 30)
      --log-file <PATH>      Mirror captured service output to this file
                             (default: /var/log/rinit/log; 'none' disables)
      --test-service <CMD>   Launch this instead of the unit directory
      -- <SERVICE> [ARGS]    Service as an argv (kernel-cmdline form)

Environment:
  RINIT_TEST_SERVICE         Service to launch instead of the unit directory
                            (default: none - boot the unit directory)
";

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse with an empty default service, matching what a real boot does when
    /// nothing overrides it: start the unit directory.
    fn parse_args(args: &[&str]) -> Result<Config, String> {
        parse_with_default(args.iter().map(OsString::from), Vec::new())
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
        assert!(
            service(&cfg).is_empty(),
            "no arguments means: start the unit directory"
        );
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

    /// M1 changed this contract deliberately: an empty service command is now
    /// legal, because a normal boot starts the unit directory instead of a
    /// hardcoded command. What must still be rejected is an explicitly given
    /// *empty program path*, which would reach `posix_spawn` as "" and surface
    /// as a confusing ENOENT.
    #[test]
    fn an_absent_service_command_falls_back_to_the_unit_directory() {
        for args in [
            vec!["--test-service", ""],
            vec!["--test-service", "   "],
            vec!["--test-service="],
            vec!["--test-service=   "],
            vec!["--"],
        ] {
            let cfg = parse_args(&args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            assert!(
                cfg.service_argv.is_empty(),
                "{args:?} should yield no command"
            );
        }
    }

    #[test]
    fn an_empty_program_path_is_still_rejected() {
        // `--` keeps empty tokens, so this reaches argv even though the
        // command is otherwise non-empty.
        let err = parse_args(&["--", "", "/bin/sh"]).unwrap_err();
        assert!(err.contains("empty program path"), "got: {err}");
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
    /// A bare empty positional argument is the same hazard via the
    /// positional form.
    #[test]
    fn empty_positional_program_is_rejected() {
        let err = parse_args(&[""]).unwrap_err();
        assert!(err.contains("empty program path"), "got: {err}");
    }

    // --- M1: unit directory and boot target ----------------------------

    #[test]
    fn unit_directory_and_target_default_to_the_plan_values() {
        let cfg = parse_args(&[]).expect("parse");
        assert_eq!(cfg.unit_dir, PathBuf::from("/etc/rinit/units"));
        assert_eq!(cfg.default_target, "multi-user.target");
    }

    #[test]
    fn unit_directory_can_be_overridden() {
        for args in [vec!["--units", "/tmp/units"], vec!["--units=/tmp/units"]] {
            let cfg = parse_args(&args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            assert_eq!(cfg.unit_dir, PathBuf::from("/tmp/units"), "{args:?}");
        }
    }

    /// Plan §7.2 names `multi-user.target` as the default target and allows
    /// `--default` to select another.
    #[test]
    fn boot_target_can_be_overridden() {
        for args in [
            vec!["--default", "rescue.target"],
            vec!["--default=rescue.target"],
        ] {
            let cfg = parse_args(&args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            assert_eq!(cfg.default_target, "rescue.target", "{args:?}");
        }
    }

    #[test]
    fn unit_flags_require_values() {
        assert!(parse_args(&["--units"]).unwrap_err().contains("--units"));
        assert!(
            parse_args(&["--default"])
                .unwrap_err()
                .contains("--default")
        );
    }

    /// A target is validated here rather than at boot, so a typo is a usage
    /// error instead of a failed startup.
    #[test]
    fn an_invalid_boot_target_is_rejected_at_parse_time() {
        let err = parse_args(&["--default", "not a unit"]).unwrap_err();
        assert!(err.contains("not a unit"), "got: {err}");
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

    /// A blank environment variable must behave as if it were unset - which
    /// from M1 means "start the unit directory", not "run an empty command".
    #[test]
    fn blank_env_value_behaves_as_unset() {
        for value in ["", "   ", "\t\n"] {
            let argv = service_argv_from_env(Some(value.to_string()));
            assert!(argv.is_empty(), "value: {value:?} must yield no command");
        }
    }

    /// With no override and no flag, a normal boot starts the unit directory.
    #[test]
    fn a_missing_env_value_leaves_the_unit_directory_in_charge() {
        assert!(
            service_argv_from_env(None).is_empty(),
            "an absent override must not inject a service"
        );
    }

    /// The end-to-end consequence: with no arguments at all, rinit boots the
    /// unit directory rather than a hardcoded service.
    #[test]
    fn no_arguments_means_boot_the_unit_directory() {
        let cfg =
            parse_with_default(Vec::<OsString>::new(), service_argv_from_env(None)).expect("parse");
        assert!(
            cfg.service_argv.is_empty(),
            "the unit directory path must be reachable by default"
        );
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
            "--units",
            "--default",
            "--cgroup-root",
            "--test-service",
            ENV_TEST_SERVICE,
        ] {
            assert!(USAGE.contains(flag), "usage is missing {flag}");
        }
    }

    /// `--cgroup-root` is an optional override in both the separated and the
    /// `=` form, and its absence leaves the plan's own default in place.
    #[test]
    fn cgroup_root_is_parsed_in_both_forms_and_defaults_to_none() {
        assert_eq!(parse_args(&[]).unwrap().cgroup_root, None);
        assert_eq!(
            parse_args(&["--cgroup-root", "/tmp/cg"])
                .unwrap()
                .cgroup_root,
            Some(PathBuf::from("/tmp/cg"))
        );
        assert_eq!(
            parse_args(&["--cgroup-root=/tmp/cg"]).unwrap().cgroup_root,
            Some(PathBuf::from("/tmp/cg"))
        );
    }

    /// A flag that promises a value and gets none must say so rather than
    /// silently booting with the default.
    #[test]
    fn cgroup_root_without_a_value_is_an_error() {
        let err = parse_args(&["--cgroup-root"]).expect_err("must fail");
        assert!(err.contains("--cgroup-root"), "got: {err}");
    }

    // --- the log file (plan §1.3(7)) ----------------------------------

    /// The file defaults to the plan's path, both flag forms name another, and
    /// the two spellings of "off" mean off.
    #[test]
    fn the_log_file_defaults_to_the_plans_path_and_can_be_turned_off() {
        assert_eq!(
            parse_args(&[]).unwrap().log_file,
            Some(PathBuf::from(DEFAULT_LOG_FILE))
        );
        assert_eq!(
            parse_args(&["--log-file", "/tmp/rinit.log"])
                .unwrap()
                .log_file,
            Some(PathBuf::from("/tmp/rinit.log"))
        );
        assert_eq!(
            parse_args(&["--log-file=/tmp/rinit.log"]).unwrap().log_file,
            Some(PathBuf::from("/tmp/rinit.log"))
        );
        for off in ["none", "NONE", ""] {
            assert_eq!(
                parse_args(&["--log-file", off]).unwrap().log_file,
                None,
                "'{off}' means do not persist"
            );
        }
        let missing = parse_args(&["--log-file"]).expect_err("must fail");
        assert!(missing.contains("--log-file"), "got: {missing}");
    }

    // --- the watchdog (plan §7.5) -------------------------------------

    /// The device defaults to `None` - which is what tells `Watchdog::prepare`
    /// to use `/dev/watchdog` except in `--dev` - and both flag forms name one.
    #[test]
    fn the_watchdog_device_is_parsed_in_both_forms_and_defaults_to_none() {
        assert_eq!(parse_args(&[]).unwrap().watchdog_device, None);
        assert_eq!(
            parse_args(&["--watchdog", "/dev/watchdog1"])
                .unwrap()
                .watchdog_device,
            Some(PathBuf::from("/dev/watchdog1"))
        );
        assert_eq!(
            parse_args(&["--watchdog=/dev/watchdog1"])
                .unwrap()
                .watchdog_device,
            Some(PathBuf::from("/dev/watchdog1"))
        );
    }

    /// The timeout defaults to the plan's 30 s, and is a plain second count.
    #[test]
    fn the_watchdog_timeout_defaults_to_the_plans_value() {
        let cfg = parse_args(&[]).unwrap();
        assert_eq!(
            cfg.watchdog_timeout_sec,
            rinit_runtime::syscall::watchdog::DEFAULT_TIMEOUT_SEC
        );
        assert_eq!(
            parse_args(&["--watchdog-timeout", "90"])
                .unwrap()
                .watchdog_timeout_sec,
            90
        );
        assert_eq!(
            parse_args(&["--watchdog-timeout=5"])
                .unwrap()
                .watchdog_timeout_sec,
            5
        );
    }

    /// A timeout that is not a usable second count is a usage error: this is the
    /// one value where falling back to a default would arm hardware for a length
    /// of time nobody chose. `0` is in the list on purpose - it is a whole
    /// number, and it is the one that would make the supervisor loop spin.
    #[test]
    fn a_watchdog_timeout_that_is_not_a_usable_second_count_is_an_error() {
        for bad in ["30s", "-1", "", "1.5", " 30", "+30", "thirty", "0", "0.0"] {
            let err = parse_args(&["--watchdog-timeout", bad])
                .expect_err("must fail")
                .to_string();
            assert!(err.contains("--watchdog-timeout"), "got: {err}");
        }
        let missing = parse_args(&["--watchdog-timeout"]).expect_err("must fail");
        assert!(missing.contains("--watchdog-timeout"), "got: {missing}");
    }
}
