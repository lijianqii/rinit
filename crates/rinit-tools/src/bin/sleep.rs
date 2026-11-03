//! `sleep` - delay for a specified amount of time.

use std::process::ExitCode;
use std::time::Duration;

use rinit_tools::{EXIT_FAILURE, EXIT_OK, program_name, usage_error};

fn parse_duration(value: &str) -> Result<f64, String> {
    let (number, multiplier) = match value.chars().last() {
        Some('s') => (&value[..value.len() - 1], 1.0),
        Some('m') => (&value[..value.len() - 1], 60.0),
        Some('h') => (&value[..value.len() - 1], 3600.0),
        Some('d') => (&value[..value.len() - 1], 86400.0),
        _ => (value, 1.0),
    };
    if number.is_empty() {
        return Err(format!("invalid time interval '{value}'"));
    }
    let seconds = number
        .parse::<f64>()
        .map_err(|_| format!("invalid time interval '{value}'"))?;
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(format!("invalid time interval '{value}'"));
    }
    Ok(seconds * multiplier)
}

fn run(args: &[String], program: &str) -> ExitCode {
    if args.is_empty() {
        return ExitCode::from(usage_error(program, "an operand is required"));
    }
    let mut total = 0.0;
    for arg in args {
        if arg == "--help" {
            println!("Usage: sleep NUMBER[smhd]...");
            return ExitCode::from(EXIT_OK);
        }
        match parse_duration(arg) {
            Ok(seconds) => total += seconds,
            Err(error) => {
                eprintln!("{program}: {error}");
                return ExitCode::from(EXIT_FAILURE);
            }
        }
    }
    let Ok(duration) = Duration::try_from_secs_f64(total) else {
        eprintln!("{program}: time interval is too large");
        return ExitCode::from(EXIT_FAILURE);
    };
    std::thread::sleep(duration);
    ExitCode::from(EXIT_OK)
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(std::env::args().next().as_deref(), "sleep");
    let args: Vec<String> = std::env::args().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_accept_suffixes_and_decimals() {
        assert_eq!(parse_duration("0"), Ok(0.0));
        assert_eq!(parse_duration("1.5"), Ok(1.5));
        assert_eq!(parse_duration("2m"), Ok(120.0));
        assert_eq!(parse_duration("1h"), Ok(3600.0));
        assert_eq!(parse_duration("1d"), Ok(86400.0));
    }

    #[test]
    fn durations_reject_bad_values() {
        for value in ["", "m", "-1", "nan", "inf", "2x"] {
            assert!(parse_duration(value).is_err(), "{value}");
        }
    }

    #[test]
    fn run_handles_zero_help_and_bad_values() {
        assert_eq!(run(&["0".to_string()], "sleep"), ExitCode::from(EXIT_OK));
        assert_eq!(
            run(&["--help".to_string()], "sleep"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["bad".to_string()], "sleep"),
            ExitCode::from(EXIT_FAILURE)
        );
        assert_eq!(run(&[], "sleep"), ExitCode::from(rinit_tools::EXIT_USAGE));
    }
}
