//! `rinit-check` - validate a unit directory without booting rinit.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use rinit_runtime::plan::plan_boot;
use rinit_runtime::{DEFAULT_TARGET, DEFAULT_UNIT_DIR};
use rinit_tools::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, program_name};

fn usage() -> String {
    format!(
        "\
Usage: rinit-check [OPTIONS]

Validate a unit directory and its boot plan without starting a service.

Options:
  --units <DIR>       Unit directory (default: {DEFAULT_UNIT_DIR})
  --default <TARGET>  Boot target (default: {DEFAULT_TARGET})
  -h, --help          Show this help and exit

Warnings do not fail the check; a missing default target, an unparsable unit,
a dependency cycle or a missing hard dependency does. Exit codes: 0 valid,
1 invalid, 2 usage.
"
    )
}

#[derive(Debug, PartialEq, Eq)]
struct Options {
    unit_dir: PathBuf,
    default_target: String,
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Help,
    Check(Options),
}

fn value(args: &mut impl Iterator<Item = OsString>, option: &str) -> Result<OsString, String> {
    args.next().ok_or_else(|| format!("{option} needs a value"))
}

fn parse<I: IntoIterator<Item = OsString>>(args: I) -> Result<Command, String> {
    let mut args = args.into_iter();
    let mut options = Options {
        unit_dir: PathBuf::from(DEFAULT_UNIT_DIR),
        default_target: DEFAULT_TARGET.to_string(),
    };
    while let Some(arg) = args.next() {
        let Some(text) = arg.to_str() else {
            return Err("an argument is not valid UTF-8".to_string());
        };
        match text {
            "-h" | "--help" => return Ok(Command::Help),
            "--units" => options.unit_dir = PathBuf::from(value(&mut args, "--units")?),
            "--default" => {
                let value = value(&mut args, "--default")?;
                options.default_target = value
                    .into_string()
                    .map_err(|_| "--default needs a UTF-8 target".to_string())?;
            }
            "--" => {
                if args.next().is_some() {
                    return Err("unexpected operand after '--'".to_string());
                }
            }
            _ if text.starts_with("--units=") => {
                let value = text.strip_prefix("--units=").unwrap_or_default();
                options.unit_dir = PathBuf::from(value);
            }
            _ if text.starts_with("--default=") => {
                let value = text.strip_prefix("--default=").unwrap_or_default();
                options.default_target = value.to_string();
            }
            _ if text.starts_with('-') => {
                return Err(format!("unknown option '{text}'"));
            }
            _ => return Err(format!("unexpected operand '{text}'")),
        }
    }
    Ok(Command::Check(options))
}

fn run<A: IntoIterator<Item = OsString>>(args: A, program: &str) -> ExitCode {
    let options = match parse(args) {
        Ok(Command::Help) => {
            print!("{}", usage());
            return ExitCode::from(EXIT_OK);
        }
        Ok(Command::Check(options)) => options,
        Err(message) => {
            eprintln!("{program}: {message}\n\n{}", usage());
            return ExitCode::from(EXIT_USAGE);
        }
    };

    match plan_boot(&options.unit_dir, &options.default_target) {
        Ok(plan) => {
            for warning in &plan.warnings {
                eprintln!("{program}: warning: {warning}");
            }
            println!(
                "{program}: OK: {} unit(s), {} level(s), target {}",
                plan.configs.len(),
                plan.levels.len(),
                plan.default_target
            );
            ExitCode::from(EXIT_OK)
        }
        Err(error) => {
            eprintln!("{program}: {error}");
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

fn main() -> ExitCode {
    let program = program_name(
        std::env::args_os()
            .next()
            .as_deref()
            .and_then(|arg| arg.to_str()),
        "rinit-check",
    );
    run(std::env::args_os().skip(1), &program)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-check-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("temp root");
        root
    }

    fn standard_tree(root: &Path) {
        fs::write(
            root.join("multi-user.target"),
            "[Unit]\nType=target\nRequires=app.service\n",
        )
        .expect("target");
        fs::write(root.join("app.service"), "[Service]\nExecStart=/bin/true\n").expect("service");
    }

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn defaults_and_both_option_forms_are_accepted() {
        let parsed = parse(args(&[])).expect("defaults");
        assert_eq!(
            parsed,
            Command::Check(Options {
                unit_dir: PathBuf::from(DEFAULT_UNIT_DIR),
                default_target: DEFAULT_TARGET.to_string(),
            })
        );

        let parsed = parse(args(&[
            "--units",
            "/tmp/units",
            "--default",
            "device.target",
        ]))
        .expect("separated forms");
        assert_eq!(
            parsed,
            Command::Check(Options {
                unit_dir: PathBuf::from("/tmp/units"),
                default_target: "device.target".to_string(),
            })
        );

        let parsed =
            parse(args(&["--units=/tmp/units", "--default=device.target"])).expect("equal forms");
        assert_eq!(
            parsed,
            Command::Check(Options {
                unit_dir: PathBuf::from("/tmp/units"),
                default_target: "device.target".to_string(),
            })
        );
    }

    #[test]
    fn help_and_malformed_arguments_are_distinguished() {
        assert_eq!(parse(args(&["--help"])).expect("help"), Command::Help);
        for bad in [
            vec!["--units"],
            vec!["--default"],
            vec!["--unknown"],
            vec!["operand"],
            vec!["--", "operand"],
        ] {
            assert!(parse(args(&bad)).is_err(), "{bad:?} must be rejected");
        }
        assert_eq!(
            run(args(&["--help"]), "rinit-check"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(args(&["--unknown"]), "rinit-check"),
            ExitCode::from(EXIT_USAGE)
        );
    }

    #[test]
    fn a_valid_tree_passes() {
        let root = temp_root("valid");
        standard_tree(&root);
        assert_eq!(
            run(
                args(&["--units", root.to_str().expect("UTF-8 temp path")]),
                "rinit-check"
            ),
            ExitCode::from(EXIT_OK)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_missing_target_and_a_parse_error_fail() {
        let root = temp_root("invalid");
        standard_tree(&root);
        assert_eq!(
            run(
                args(&[
                    "--units",
                    root.to_str().expect("UTF-8 temp path"),
                    "--default",
                    "missing.target",
                ]),
                "rinit-check"
            ),
            ExitCode::from(EXIT_FAILURE)
        );
        fs::write(
            root.join("broken.service"),
            "[Service]\nExecStrt=/bin/true\n",
        )
        .expect("broken unit");
        assert_eq!(
            run(
                args(&["--units", root.to_str().expect("UTF-8 temp path")]),
                "rinit-check"
            ),
            ExitCode::from(EXIT_FAILURE)
        );
        let missing = root.join("does-not-exist");
        assert_eq!(
            run(
                args(&["--units", missing.to_str().expect("UTF-8 temp path")]),
                "rinit-check"
            ),
            ExitCode::from(EXIT_FAILURE)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn warnings_do_not_fail_the_check() {
        let root = temp_root("warnings");
        fs::write(
            root.join("multi-user.target"),
            "[Unit]\nType=target\nRequires=app.service\n",
        )
        .expect("target");
        fs::write(
            root.join("app.service"),
            "[Unit]\nWants=ghost.service\n[Service]\nExecStart=/bin/true\n",
        )
        .expect("service");
        assert_eq!(
            run(
                args(&["--units", root.to_str().expect("UTF-8 temp path")]),
                "rinit-check"
            ),
            ExitCode::from(EXIT_OK)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn the_shipped_unit_directory_validates() {
        let units = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../units");
        assert_eq!(
            run(
                args(&["--units", units.to_str().expect("UTF-8 source path")]),
                "rinit-check"
            ),
            ExitCode::from(EXIT_OK)
        );
    }
}
