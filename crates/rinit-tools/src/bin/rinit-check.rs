//! `rinit-check` - validate a unit directory without booting rinit.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use rinit_runtime::plan::{BootPlan, describe_plan, plan_boot};
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
  --strict            Treat warnings as failures
  --json              Emit a machine-readable report
  --plan              Print the resolved startup levels
  -h, --help          Show this help and exit

Warnings do not fail the check unless --strict is given. A missing default
target, an unparsable unit, a dependency cycle or a missing hard dependency
always fails. Exit codes: 0 valid, 1 invalid, 2 usage.
"
    )
}

#[derive(Debug, PartialEq, Eq)]
struct Options {
    unit_dir: PathBuf,
    default_target: String,
    strict: bool,
    json: bool,
    plan: bool,
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
        strict: false,
        json: false,
        plan: false,
    };
    while let Some(arg) = args.next() {
        let Some(text) = arg.to_str() else {
            return Err("an argument is not valid UTF-8".to_string());
        };
        match text {
            "-h" | "--help" => return Ok(Command::Help),
            "--strict" => options.strict = true,
            "--json" => options.json = true,
            "--plan" => options.plan = true,
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
    if options.json && options.plan {
        return Err("--json and --plan cannot be combined".to_string());
    }
    Ok(Command::Check(options))
}

fn json_string(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn json_report(plan: &BootPlan, ok: bool) -> String {
    let levels = plan
        .levels
        .iter()
        .map(|level| {
            let units = level
                .iter()
                .map(|unit| json_string(unit.canonical()))
                .collect::<Vec<_>>()
                .join(",");
            format!("[{units}]")
        })
        .collect::<Vec<_>>()
        .join(",");
    let warnings = plan
        .warnings
        .iter()
        .map(|warning| json_string(warning))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"ok\":{ok},\"target\":{},\"units\":{},\"templates\":{},\"levels\":[{}],\"warnings\":[{}]}}",
        json_string(plan.default_target.canonical()),
        plan.configs.len(),
        plan.templates.len(),
        levels,
        warnings
    )
}

fn json_error(error: &str) -> String {
    format!("{{\"ok\":false,\"error\":{}}}", json_string(error))
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
            let ok = !(options.strict && !plan.warnings.is_empty());
            if options.json {
                println!("{}", json_report(&plan, ok));
            } else if options.plan {
                for line in describe_plan(&plan) {
                    println!("{line}");
                }
            } else {
                println!(
                    "{program}: OK: {} unit(s), {} level(s), target {}",
                    plan.configs.len(),
                    plan.levels.len(),
                    plan.default_target
                );
            }
            ExitCode::from(if ok { EXIT_OK } else { EXIT_FAILURE })
        }
        Err(error) => {
            if options.json {
                println!("{}", json_error(&error.to_string()));
            }
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
                strict: false,
                json: false,
                plan: false,
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
                strict: false,
                json: false,
                plan: false,
            })
        );

        let parsed =
            parse(args(&["--units=/tmp/units", "--default=device.target"])).expect("equal forms");
        assert_eq!(
            parsed,
            Command::Check(Options {
                unit_dir: PathBuf::from("/tmp/units"),
                default_target: "device.target".to_string(),
                strict: false,
                json: false,
                plan: false,
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
        for flag in ["--json", "--plan"] {
            assert_eq!(
                run(
                    args(&["--units", root.to_str().expect("UTF-8 temp path"), flag,]),
                    "rinit-check"
                ),
                ExitCode::from(EXIT_OK)
            );
        }
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
    fn report_flags_are_parsed_and_json_and_plan_are_exclusive() {
        let parsed = parse(args(&["--strict", "--json"])).expect("flags");
        assert!(matches!(
            parsed,
            Command::Check(Options {
                strict: true,
                json: true,
                plan: false,
                ..
            })
        ));
        assert!(parse(args(&["--json", "--plan"])).is_err());
    }

    #[test]
    fn json_output_is_structured_and_escaped() {
        assert_eq!(json_string("a\"b\n"), "\"a\\\"b\\n\"");
        let root = temp_root("json");
        standard_tree(&root);
        let plan = plan_boot(&root, "multi-user.target").expect("plan");
        let report = json_report(&plan, true);
        assert!(report.contains("\"ok\":true"), "{report}");
        assert!(report.contains("\"levels\""), "{report}");
        assert!(report.contains("\"app.service\""), "{report}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn strict_turns_a_warning_into_a_failure() {
        let root = temp_root("strict");
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
                args(&[
                    "--units",
                    root.to_str().expect("UTF-8 temp path"),
                    "--strict",
                ]),
                "rinit-check"
            ),
            ExitCode::from(EXIT_FAILURE)
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
