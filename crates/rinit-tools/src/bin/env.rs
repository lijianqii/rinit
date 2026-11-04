//! `env` - run a program in a modified environment.

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::process::{Command, ExitCode};

use rinit_tools::{EXIT_OK, program_name, usage_error};

#[derive(Default)]
struct Plan {
    clear: bool,
    unset: Vec<String>,
    assignments: Vec<(String, String)>,
    command: Vec<String>,
}

enum Parsed {
    Help,
    Plan(Plan),
}

fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut plan = Plan::default();
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if !plan.command.is_empty() {
            plan.command.push(arg.clone());
            index += 1;
            continue;
        }
        match arg.as_str() {
            "--help" => return Ok(Parsed::Help),
            "-i" | "--ignore-environment" => plan.clear = true,
            "-u" | "--unset" => {
                index += 1;
                let Some(name) = args.get(index) else {
                    return Err("-u needs a variable name".to_string());
                };
                plan.unset.push(name.clone());
            }
            "--" => {
                index += 1;
                if let Some(command) = args.get(index) {
                    plan.command.push(command.clone());
                }
            }
            value if value.starts_with('-') && value != "-" => {
                return Err(format!("unknown option '{value}'"));
            }
            value => {
                if let Some((name, value)) = value.split_once('=') {
                    if name.is_empty() {
                        return Err("empty variable name".to_string());
                    }
                    plan.assignments.push((name.to_string(), value.to_string()));
                } else {
                    plan.command.push(value.to_string());
                }
            }
        }
        index += 1;
    }
    Ok(Parsed::Plan(plan))
}

fn environment(plan: &Plan) -> Vec<(OsString, OsString)> {
    let mut values: Vec<(OsString, OsString)> = if plan.clear {
        Vec::new()
    } else {
        std::env::vars_os().collect()
    };
    values.retain(|(name, _)| {
        !plan
            .unset
            .iter()
            .any(|unset| name.as_os_str() == std::ffi::OsStr::new(unset))
    });
    for (name, value) in &plan.assignments {
        let name = OsString::from(name);
        let value = OsString::from(value);
        if let Some(existing) = values.iter_mut().find(|(key, _)| *key == name) {
            existing.1 = value;
        } else {
            values.push((name, value));
        }
    }
    values
}

fn print_environment(plan: &Plan) -> std::io::Result<()> {
    let mut stdout = std::io::stdout().lock();
    for (name, value) in environment(plan) {
        stdout.write_all(name.as_os_str().as_bytes())?;
        stdout.write_all(b"=")?;
        stdout.write_all(value.as_os_str().as_bytes())?;
        stdout.write_all(b"\n")?;
    }
    Ok(())
}

fn run(args: &[String], program: &str) -> ExitCode {
    let parsed = match parse(args) {
        Ok(parsed) => parsed,
        Err(error) => return ExitCode::from(usage_error(program, &error)),
    };
    let plan = match parsed {
        Parsed::Help => {
            println!("Usage: env [-i] [-u NAME] [NAME=VALUE...] [COMMAND [ARG...]]");
            return ExitCode::from(EXIT_OK);
        }
        Parsed::Plan(plan) => plan,
    };
    if plan.command.is_empty() {
        return match print_environment(&plan) {
            Ok(()) => ExitCode::from(EXIT_OK),
            Err(error) => {
                eprintln!("{program}: {error}");
                ExitCode::from(1)
            }
        };
    }
    let mut command = Command::new(&plan.command[0]);
    command.args(&plan.command[1..]);
    if plan.clear {
        command.env_clear();
    }
    for name in &plan.unset {
        command.env_remove(name);
    }
    for (name, value) in &plan.assignments {
        command.env(name, value);
    }
    match command.status() {
        Ok(status) => ExitCode::from(
            status
                .code()
                .and_then(|code| u8::try_from(code).ok())
                .unwrap_or(1),
        ),
        Err(error) => {
            eprintln!("{program}: {}: {error}", plan.command[0]);
            ExitCode::from(1)
        }
    }
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(
        std::env::args_os()
            .next()
            .as_deref()
            .and_then(|arg| arg.to_str()),
        "env",
    );
    let Some(args) = rinit_tools::utf8_args(&program) else {
        return ExitCode::from(rinit_tools::EXIT_USAGE);
    };
    let args: Vec<String> = args.into_iter().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_collects_assignments_and_commands() {
        let parsed = parse(&[
            "-i".to_string(),
            "FOO=bar".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            "true".to_string(),
        ])
        .expect("parse");
        let Parsed::Plan(plan) = parsed else {
            panic!("expected a plan");
        };
        assert!(plan.clear);
        assert_eq!(
            plan.assignments,
            vec![("FOO".to_string(), "bar".to_string())]
        );
        assert_eq!(plan.command, ["sh", "-c", "true"]);
    }

    #[test]
    fn parser_handles_unset_and_errors() {
        let parsed = parse(&["-u".to_string(), "HOME".to_string()]).expect("parse");
        let Parsed::Plan(plan) = parsed else {
            panic!("expected a plan");
        };
        assert_eq!(plan.unset, ["HOME"]);
        assert!(parse(&["-u".to_string()]).is_err());
        assert!(parse(&["--bad".to_string()]).is_err());
    }

    #[test]
    fn run_executes_a_command_with_an_assignment() {
        let status = run(
            &[
                "-i".to_string(),
                "FOO=bar".to_string(),
                "/bin/sh".to_string(),
                "-c".to_string(),
                "test \"$FOO\" = bar".to_string(),
            ],
            "env",
        );
        assert_eq!(status, ExitCode::from(EXIT_OK));
    }

    #[test]
    fn run_prints_environment_and_help() {
        assert_eq!(run(&[], "env"), ExitCode::from(EXIT_OK));
        assert_eq!(run(&["--help".to_string()], "env"), ExitCode::from(EXIT_OK));
        assert_eq!(
            run(&["--bad".to_string()], "env"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;

    #[test]
    fn run_builds_environments_and_reports_usage_errors() {
        assert_eq!(run(&["--help".to_string()], "env"), ExitCode::from(EXIT_OK));
        assert_eq!(run(&["-i".to_string()], "env"), ExitCode::from(EXIT_OK));
        assert_eq!(
            run(
                &[
                    "-i".to_string(),
                    "FOO=bar".to_string(),
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "test \"$FOO\" = bar".to_string()
                ],
                "env"
            ),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(
                &[
                    "-u".to_string(),
                    "PATH".to_string(),
                    "/bin/true".to_string()
                ],
                "env"
            ),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["--bad".to_string()], "env"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        assert_eq!(
            run(&["-u".to_string()], "env"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        assert_eq!(
            run(&["/definitely/missing".to_string()], "env"),
            ExitCode::from(1)
        );
    }
}
