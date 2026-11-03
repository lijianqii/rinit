//! `echo` - write arguments to standard output.

use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name};

fn expand(input: &str, out: &mut String) -> bool {
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let Some(escape) = chars.next() else {
            out.push('\\');
            break;
        };
        match escape {
            'a' => out.push('\x07'),
            'b' => out.push('\x08'),
            'c' => return true,
            'e' => out.push('\x1b'),
            'f' => out.push('\x0c'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'v' => out.push('\x0b'),
            '\\' => out.push('\\'),
            '0' => {
                let mut value = 0_u32;
                let mut digits = 0;
                while digits < 3 {
                    let Some(next) = chars.peek().copied() else {
                        break;
                    };
                    let Some(digit) = next.to_digit(8) else {
                        break;
                    };
                    chars.next();
                    value = value * 8 + digit;
                    digits += 1;
                }
                if let Some(character) = char::from_u32(value) {
                    out.push(character);
                }
            }
            'x' => {
                let mut value = 0_u32;
                let mut digits = 0;
                while digits < 2 {
                    let Some(next) = chars.peek().copied() else {
                        break;
                    };
                    let Some(digit) = next.to_digit(16) else {
                        break;
                    };
                    chars.next();
                    value = value * 16 + digit;
                    digits += 1;
                }
                if digits == 0 {
                    out.push('\\');
                    out.push('x');
                } else if let Some(character) = char::from_u32(value) {
                    out.push(character);
                }
            }
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    false
}

fn render(args: &[String], escapes: bool) -> (String, bool) {
    let mut out = String::new();
    let mut stop = false;
    for (index, arg) in args.iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        if escapes {
            if expand(arg, &mut out) {
                stop = true;
                break;
            }
        } else {
            out.push_str(arg);
        }
    }
    (out, stop)
}

fn run(args: &[String], _program: &str) -> ExitCode {
    let mut newline = true;
    let mut escapes = false;
    let mut values: Vec<String> = Vec::new();
    let mut parsing = true;
    for arg in args {
        if parsing {
            match arg.as_str() {
                "--help" => {
                    println!("Usage: echo [-neE] [STRING...]");
                    return ExitCode::from(EXIT_OK);
                }
                "-n" => {
                    newline = false;
                    continue;
                }
                "-e" => {
                    escapes = true;
                    continue;
                }
                "-E" => {
                    escapes = false;
                    continue;
                }
                value if value.starts_with('-') && value.len() > 1 => {
                    parsing = false;
                }
                _ => parsing = false,
            }
        }
        values.push(arg.clone());
    }
    let (text, stop) = render(&values, escapes);
    if !stop && newline {
        println!("{text}");
    } else {
        print!("{text}");
    }
    ExitCode::from(EXIT_OK)
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(std::env::args().next().as_deref(), "echo");
    let args: Vec<String> = std::env::args().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_rendering_joins_arguments() {
        let args = vec!["one".to_string(), "two".to_string()];
        assert_eq!(render(&args, false), ("one two".to_string(), false));
    }

    #[test]
    fn escapes_cover_common_sequences() {
        let args = vec!["a\\tb\\nc".to_string()];
        assert_eq!(render(&args, true), ("a\tb\nc".to_string(), false));
        let args = vec!["stop\\chere".to_string()];
        assert_eq!(render(&args, true), ("stop".to_string(), true));
        let args = vec!["\\x41\\0101".to_string()];
        assert_eq!(render(&args, true), ("AA".to_string(), false));
    }

    #[test]
    fn run_handles_help_and_options() {
        assert_eq!(
            run(&["--help".to_string()], "echo"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["-n".to_string(), "x".to_string()], "echo"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["-e".to_string(), "x".to_string()], "echo"),
            ExitCode::from(EXIT_OK)
        );
    }
}
