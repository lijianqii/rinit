//! Command-line parsing for `rctl`.
//!
//! Pure and separate from `main` so every subcommand, every missing argument
//! and every `--tail` spelling is unit-tested without a socket.

use std::path::PathBuf;

use rinit_core::{DEFAULT_LOG_FILE, ListFormat, Request, StatusFormat, UnitName};

/// How many recent log lines the default systemd status view includes.
const DEFAULT_STATUS_TAIL: u32 = 10;

/// Read the persisted log instead of asking rinit for the ring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileLog {
    /// Current log path; rotated files are derived as `path.1` ... `path.N`.
    pub path: PathBuf,
    /// Restrict output to one unit, or `None` for every unit.
    pub unit: Option<UnitName>,
    /// Newest line count; zero means everything.
    pub tail: u32,
}

/// What a command line asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// `--help`.
    Help,
    /// `--version`.
    Version,
    /// A request to send.
    Request(Request),
    /// Read the persisted log locally, without the control socket.
    FileLog(FileLog),
}

/// Parse `rctl`'s arguments (excluding `argv[0]`).
///
/// # Errors
/// A human-readable message for an unknown command, a missing unit, or a bad
/// `--tail`. The caller turns that into exit code 2.
pub fn command_line(args: &[String]) -> Result<Parsed, String> {
    let Some((command, rest)) = args.split_first() else {
        return Err("no command given".to_string());
    };

    match command.as_str() {
        "-h" | "--help" => Ok(Parsed::Help),
        "-V" | "--version" => Ok(Parsed::Version),
        "list" => {
            let format = parse_list(rest)?;
            Ok(Parsed::Request(Request::ListFormatted { format }))
        }
        "status" => {
            let (unit, tail, format) = parse_status(rest)?;
            Ok(Parsed::Request(Request::StatusFormatted {
                unit,
                tail,
                format,
            }))
        }
        "log" => parse_log(rest),
        "start" => Ok(Parsed::Request(Request::Start(required_unit(rest)?))),
        "stop" => Ok(Parsed::Request(Request::Stop(required_unit(rest)?))),
        "restart" => Ok(Parsed::Request(Request::Restart(required_unit(rest)?))),
        "reset-failed" => Ok(Parsed::Request(Request::ResetFailed(required_unit(rest)?))),
        "reload" => {
            ensure_no_arguments(rest)?;
            Ok(Parsed::Request(Request::Reload))
        }
        "reboot" => {
            ensure_no_arguments(rest)?;
            Ok(Parsed::Request(Request::Reboot))
        }
        "poweroff" => {
            ensure_no_arguments(rest)?;
            Ok(Parsed::Request(Request::Poweroff))
        }
        other => Err(format!("unknown command '{other}'")),
    }
}

/// A subcommand that takes exactly one unit.
fn required_unit(rest: &[String]) -> Result<UnitName, String> {
    match rest {
        [] => Err("this command needs a unit name".to_string()),
        [unit] => parse_unit(unit),
        // The `[]` and `[unit]` arms above are matched first, so `extra` below
        // is the second argument: indexing `rest[1]` is safe but unnecessary.
        [_, extra, ..] => Err(format!("unexpected extra argument '{extra}'")),
    }
}

/// `log` reads the ring by default, or a persisted file with `--file`.
fn parse_log(rest: &[String]) -> Result<Parsed, String> {
    let mut unit: Option<UnitName> = None;
    let mut tail: u32 = 0;
    let mut file: Option<PathBuf> = None;
    let mut index = 0;
    while let Some(arg) = rest.get(index) {
        if let Some(value) = arg.strip_prefix("--tail=") {
            tail = parse_tail(value)?;
        } else if arg == "--tail" {
            let value = rest
                .get(index + 1)
                .ok_or_else(|| "--tail needs a line count".to_string())?;
            tail = parse_tail(value)?;
            index += 1;
        } else if let Some(value) = arg.strip_prefix("--file=") {
            if value.is_empty() {
                return Err("--file needs a path".to_string());
            }
            file = Some(PathBuf::from(value));
        } else if arg == "--file" {
            file = Some(PathBuf::from(DEFAULT_LOG_FILE));
        } else if arg.starts_with('-') {
            return Err(format!("unknown option '{arg}'"));
        } else if unit.is_none() {
            unit = Some(parse_unit(arg)?);
        } else {
            return Err(format!("unexpected extra argument '{arg}'"));
        }
        index += 1;
    }
    match file {
        Some(path) => Ok(Parsed::FileLog(FileLog { path, unit, tail })),
        None => Ok(Parsed::Request(Request::Log { unit, tail })),
    }
}

/// `list` takes an optional presentation format and no unit name.
fn parse_list(rest: &[String]) -> Result<ListFormat, String> {
    let mut format = ListFormat::Table;
    let mut index = 0;
    while let Some(arg) = rest.get(index) {
        if let Some(value) = arg.strip_prefix("--format=") {
            format = parse_list_format(value)?;
        } else if arg == "--format" {
            let value = rest
                .get(index + 1)
                .ok_or_else(|| "--format needs a value".to_string())?;
            format = parse_list_format(value)?;
            index += 1;
        } else if arg == "--table" {
            format = ListFormat::Table;
        } else if arg == "--plain" {
            format = ListFormat::Plain;
        } else if arg == "--short" {
            format = ListFormat::Short;
        } else if arg == "--json" {
            format = ListFormat::Json;
        } else {
            return Err(format!("unexpected argument '{arg}'"));
        }
        index += 1;
    }
    Ok(format)
}

fn parse_list_format(value: &str) -> Result<ListFormat, String> {
    ListFormat::parse(value)
        .ok_or_else(|| format!("unknown list format '{value}' (try table, plain, short, json)"))
}

/// `status` takes the same optional unit and `--tail`, plus a presentation
/// format. The aliases are intentional: `--json` is easier to type than
/// `--format=json`, and `--plain` is the escape hatch for existing scripts.
fn parse_status(rest: &[String]) -> Result<(Option<UnitName>, u32, StatusFormat), String> {
    let mut unit: Option<UnitName> = None;
    let mut tail: Option<u32> = None;
    let mut format = StatusFormat::Systemd;
    let mut index = 0;
    while let Some(arg) = rest.get(index) {
        if let Some(value) = arg.strip_prefix("--tail=") {
            tail = Some(parse_tail(value)?);
        } else if arg == "--tail" {
            let value = rest
                .get(index + 1)
                .ok_or_else(|| "--tail needs a line count".to_string())?;
            tail = Some(parse_tail(value)?);
            index += 1;
        } else if let Some(value) = arg.strip_prefix("--format=") {
            format = parse_status_format(value)?;
        } else if arg == "--format" {
            let value = rest
                .get(index + 1)
                .ok_or_else(|| "--format needs a value".to_string())?;
            format = parse_status_format(value)?;
            index += 1;
        } else if arg == "--json" {
            format = StatusFormat::Json;
        } else if arg == "--plain" {
            format = StatusFormat::Plain;
        } else if arg == "--short" {
            format = StatusFormat::Short;
        } else if arg == "--card" {
            format = StatusFormat::Card;
        } else if arg.starts_with('-') {
            return Err(format!("unknown option '{arg}'"));
        } else if unit.is_none() {
            unit = Some(parse_unit(arg)?);
        } else {
            return Err(format!("unexpected extra argument '{arg}'"));
        }
        index += 1;
    }
    // The default human view carries a small tail; machine-oriented formats
    // stay stable unless the caller asks for logs explicitly. `--tail 0` is
    // the escape hatch for the systemd view itself.
    let tail = tail.unwrap_or_else(|| {
        if unit.is_some() && format == StatusFormat::Systemd {
            DEFAULT_STATUS_TAIL
        } else {
            0
        }
    });
    Ok((unit, tail, format))
}

fn parse_status_format(value: &str) -> Result<StatusFormat, String> {
    StatusFormat::parse(value).ok_or_else(|| {
        format!("unknown status format '{value}' (try systemd, plain, short, json, card)")
    })
}

fn parse_tail(value: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .map_err(|_| format!("--tail expects a line count, got '{value}'"))
}

fn parse_unit(value: &str) -> Result<UnitName, String> {
    UnitName::parse(value).map_err(|e| e.to_string())
}

fn ensure_no_arguments(rest: &[String]) -> Result<(), String> {
    match rest {
        [] => Ok(()),
        [extra, ..] => Err(format!("unexpected argument '{extra}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Parsed, String> {
        let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        command_line(&args)
    }

    fn unit(name: &str) -> UnitName {
        UnitName::parse(name).unwrap()
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert_eq!(parse(&["--help"]), Ok(Parsed::Help));
        assert_eq!(parse(&["-h"]), Ok(Parsed::Help));
        assert_eq!(parse(&["--version"]), Ok(Parsed::Version));
        assert_eq!(parse(&["-V"]), Ok(Parsed::Version));
    }

    #[test]
    fn no_arguments_is_an_error() {
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn list_defaults_to_the_table_and_takes_formats() {
        assert_eq!(
            parse(&["list"]),
            Ok(Parsed::Request(Request::ListFormatted {
                format: ListFormat::Table
            }))
        );
        for args in [
            ["list", "--format=plain"].as_slice(),
            ["list", "--format", "plain"].as_slice(),
            ["list", "--plain"].as_slice(),
        ] {
            assert_eq!(
                parse(args),
                Ok(Parsed::Request(Request::ListFormatted {
                    format: ListFormat::Plain
                })),
                "{args:?}"
            );
        }
        assert_eq!(
            parse(&["list", "--json"]),
            Ok(Parsed::Request(Request::ListFormatted {
                format: ListFormat::Json
            }))
        );
        assert!(parse(&["list", "--format=xml"]).is_err());
        assert!(parse(&["list", "extra"]).is_err());
    }

    /// `reload` names no unit: it is about the directory, not about one unit
    /// (plan §7.6).
    #[test]
    fn reload_takes_nothing() {
        assert_eq!(parse(&["reload"]), Ok(Parsed::Request(Request::Reload)));
        assert!(parse(&["reload", "app.service"]).is_err());
        assert!(parse(&["reload", "--tail", "5"]).is_err());
    }

    #[test]
    fn reboot_and_poweroff_take_nothing() {
        assert_eq!(parse(&["reboot"]), Ok(Parsed::Request(Request::Reboot)));
        assert_eq!(parse(&["poweroff"]), Ok(Parsed::Request(Request::Poweroff)));
        assert!(parse(&["reboot", "app.service"]).is_err());
        assert!(parse(&["poweroff", "--tail", "5"]).is_err());
    }

    /// A bare unit name defaults to `.service`, exactly as in a unit file.
    #[test]
    fn unit_commands_parse_a_bare_name() {
        assert_eq!(
            parse(&["start", "app"]),
            Ok(Parsed::Request(Request::Start(unit("app.service"))))
        );
        assert_eq!(
            parse(&["stop", "app.service"]),
            Ok(Parsed::Request(Request::Stop(unit("app.service"))))
        );
        assert_eq!(
            parse(&["restart", "app.target"]),
            Ok(Parsed::Request(Request::Restart(unit("app.target"))))
        );
        assert_eq!(
            parse(&["reset-failed", "app"]),
            Ok(Parsed::Request(Request::ResetFailed(unit("app.service"))))
        );
    }

    #[test]
    fn a_missing_unit_is_an_error() {
        for command in ["start", "stop", "restart", "reset-failed"] {
            assert!(parse(&[command]).is_err(), "{command} needs a unit");
        }
        assert!(parse(&["start", "a", "b"]).is_err(), "one unit only");
    }

    #[test]
    fn status_and_log_take_an_optional_unit() {
        assert_eq!(
            parse(&["status"]),
            Ok(Parsed::Request(Request::StatusFormatted {
                unit: None,
                tail: 0,
                format: StatusFormat::Systemd
            }))
        );
        assert_eq!(
            parse(&["status", "app"]),
            Ok(Parsed::Request(Request::StatusFormatted {
                unit: Some(unit("app.service")),
                tail: DEFAULT_STATUS_TAIL,
                format: StatusFormat::Systemd
            }))
        );
        assert_eq!(
            parse(&["log", "app"]),
            Ok(Parsed::Request(Request::Log {
                unit: Some(unit("app.service")),
                tail: 0
            }))
        );
    }

    #[test]
    fn log_file_mode_uses_the_default_or_an_explicit_path() {
        assert_eq!(
            parse(&["log", "--file"]),
            Ok(Parsed::FileLog(FileLog {
                path: PathBuf::from(DEFAULT_LOG_FILE),
                unit: None,
                tail: 0
            }))
        );
        assert_eq!(
            parse(&["log", "app", "--file=/tmp/rinit.log", "--tail", "5"]),
            Ok(Parsed::FileLog(FileLog {
                path: PathBuf::from("/tmp/rinit.log"),
                unit: Some(unit("app.service")),
                tail: 5
            }))
        );
        assert_eq!(
            parse(&["log", "--file=/tmp/rinit.log", "app", "--tail=3"]),
            Ok(Parsed::FileLog(FileLog {
                path: PathBuf::from("/tmp/rinit.log"),
                unit: Some(unit("app.service")),
                tail: 3
            }))
        );
        assert!(parse(&["log", "--file="]).is_err());
    }

    /// The default systemd status view includes a small tail; explicit
    /// formats and `--tail 0` do not.
    #[test]
    fn status_defaults_to_a_small_log_tail_only_in_the_systemd_view() {
        for args in [
            ["status", "app", "--plain"].as_slice(),
            ["status", "app", "--short"].as_slice(),
            ["status", "app", "--json"].as_slice(),
            ["status", "app", "--card"].as_slice(),
        ] {
            let Ok(Parsed::Request(Request::StatusFormatted {
                unit: parsed_unit,
                tail,
                ..
            })) = parse(args)
            else {
                panic!("{args:?} did not parse as a formatted status request");
            };
            assert_eq!(parsed_unit, Some(unit("app.service")), "{args:?}");
            assert_eq!(tail, 0, "{args:?}");
        }
        assert_eq!(
            parse(&["status", "app", "--tail", "0"]),
            Ok(Parsed::Request(Request::StatusFormatted {
                unit: Some(unit("app.service")),
                tail: 0,
                format: StatusFormat::Systemd
            }))
        );
        assert_eq!(
            parse(&["status", "app", "--format=systemd"]),
            Ok(Parsed::Request(Request::StatusFormatted {
                unit: Some(unit("app.service")),
                tail: DEFAULT_STATUS_TAIL,
                format: StatusFormat::Systemd
            }))
        );
    }

    /// Both `--tail N` and `--tail=N` are accepted, in either position.
    #[test]
    fn tail_is_accepted_in_both_spellings() {
        for args in [
            ["status", "app", "--tail", "5"].as_slice(),
            ["status", "app", "--tail=5"].as_slice(),
            ["status", "--tail", "5", "app"].as_slice(),
            ["status", "--tail=5", "app"].as_slice(),
        ] {
            assert_eq!(
                parse(args),
                Ok(Parsed::Request(Request::StatusFormatted {
                    unit: Some(unit("app.service")),
                    tail: 5,
                    format: StatusFormat::Systemd
                })),
                "{args:?}"
            );
        }
        assert_eq!(
            parse(&["log", "--tail=100"]),
            Ok(Parsed::Request(Request::Log {
                unit: None,
                tail: 100
            }))
        );
    }

    #[test]
    fn status_formats_are_aliases_or_values() {
        for args in [
            ["status", "--format=plain"].as_slice(),
            ["status", "--format", "plain"].as_slice(),
            ["status", "--plain"].as_slice(),
        ] {
            assert_eq!(
                parse(args),
                Ok(Parsed::Request(Request::StatusFormatted {
                    unit: None,
                    tail: 0,
                    format: StatusFormat::Plain
                })),
                "{args:?}"
            );
        }
        for (args, format) in [
            (["status", "--json"].as_slice(), StatusFormat::Json),
            (["status", "--short"].as_slice(), StatusFormat::Short),
            (["status", "--card"].as_slice(), StatusFormat::Card),
            (
                ["status", "--format=systemd"].as_slice(),
                StatusFormat::Systemd,
            ),
        ] {
            assert_eq!(
                parse(args),
                Ok(Parsed::Request(Request::StatusFormatted {
                    unit: None,
                    tail: 0,
                    format
                })),
                "{args:?}"
            );
        }
        assert!(parse(&["status", "--format=json", "app"]).is_ok());
        assert!(parse(&["status", "--format=xml"]).is_err());
        assert!(parse(&["status", "--format"]).is_err());
    }

    #[test]
    fn a_bad_tail_is_an_error() {
        assert!(parse(&["status", "--tail"]).is_err());
        assert!(parse(&["status", "--tail", "soon"]).is_err());
        assert!(parse(&["status", "--tail=-1"]).is_err());
    }

    #[test]
    fn an_unknown_option_or_command_is_an_error() {
        assert!(parse(&["status", "--lines", "5"]).is_err());
        assert!(parse(&["frobnicate"]).is_err());
    }

    #[test]
    fn an_invalid_unit_name_is_an_error() {
        assert!(parse(&["start", "bad/name"]).is_err());
    }
}
