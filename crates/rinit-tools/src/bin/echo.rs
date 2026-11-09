//! `echo` - write arguments to standard output.

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name};

fn expand(input: &[u8], out: &mut Vec<u8>) -> bool {
    let mut index = 0;
    while index < input.len() {
        let byte = input[index];
        index += 1;
        if byte != b'\\' {
            out.push(byte);
            continue;
        }
        let Some(&escape) = input.get(index) else {
            out.push(b'\\');
            break;
        };
        index += 1;
        match escape {
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'c' => return true,
            b'e' => out.push(0x1b),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0b),
            b'\\' => out.push(b'\\'),
            b'0' => {
                let mut value = 0_u16;
                let mut digits = 0;
                while digits < 3 {
                    let Some(&next) = input.get(index) else {
                        break;
                    };
                    if !(b'0'..=b'7').contains(&next) {
                        break;
                    }
                    index += 1;
                    value = value * 8 + u16::from(next - b'0');
                    digits += 1;
                }
                out.push(value as u8);
            }
            b'x' => {
                let mut value = 0_u16;
                let mut digits = 0;
                while digits < 2 {
                    let Some(&next) = input.get(index) else {
                        break;
                    };
                    let Some(digit) = (next as char).to_digit(16) else {
                        break;
                    };
                    index += 1;
                    value = value * 16 + digit as u16;
                    digits += 1;
                }
                if digits == 0 {
                    out.push(b'\\');
                    out.push(b'x');
                } else {
                    out.push(value as u8);
                }
            }
            other => {
                out.push(b'\\');
                out.push(other);
            }
        }
    }
    false
}

fn render(args: &[OsString], escapes: bool) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut stop = false;
    for (index, arg) in args.iter().enumerate() {
        if index > 0 {
            out.push(b' ');
        }
        let bytes = arg.as_bytes();
        if escapes {
            if expand(bytes, &mut out) {
                stop = true;
                break;
            }
        } else {
            out.extend_from_slice(bytes);
        }
    }
    (out, stop)
}

fn run(args: &[OsString], program: &str) -> ExitCode {
    let mut newline = true;
    let mut escapes = false;
    let mut values: Vec<OsString> = Vec::new();
    let mut parsing = true;
    for arg in args {
        if parsing {
            if let Some(text) = arg.to_str() {
                if text == "--help" {
                    println!("Usage: echo [-neE] [STRING...]");
                    return ExitCode::from(EXIT_OK);
                }
                if text.len() > 1 && text.starts_with('-') {
                    let flags = &text[1..];
                    if flags.chars().all(|ch| matches!(ch, 'n' | 'e' | 'E')) {
                        for ch in flags.chars() {
                            match ch {
                                'n' => newline = false,
                                'e' => escapes = true,
                                'E' => escapes = false,
                                _ => {}
                            }
                        }
                        continue;
                    }
                }
            }
            parsing = false;
        }
        values.push(arg.clone());
    }
    let (text, stop) = render(&values, escapes);
    let mut stdout = std::io::stdout().lock();
    let result = if !stop && newline {
        stdout
            .write_all(&text)
            .and_then(|()| stdout.write_all(b"\n"))
    } else {
        stdout.write_all(&text)
    };
    match result {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(error) => {
            eprintln!("{program}: {error}");
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
        "echo",
    );
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn plain_rendering_joins_arguments() {
        assert_eq!(
            render(&os(&["one", "two"]), false),
            (b"one two".to_vec(), false)
        );
    }

    #[test]
    fn escapes_cover_common_sequences() {
        assert_eq!(
            render(&os(&["a\\tb\\nc"]), true),
            (b"a\tb\nc".to_vec(), false)
        );
        assert_eq!(
            render(&os(&["stop\\chere"]), true),
            (b"stop".to_vec(), true)
        );
        assert_eq!(render(&os(&["\\x41\\0101"]), true), (b"AA".to_vec(), false));
    }

    #[test]
    fn hex_escapes_are_raw_bytes_not_utf8() {
        assert_eq!(render(&os(&["\\xFF"]), true), (vec![0xff], false));
        assert_eq!(render(&os(&["\\0377"]), true), (vec![0xff], false));
    }

    #[test]
    fn run_handles_help_and_clustered_options() {
        assert_eq!(run(&os(&["--help"]), "echo"), ExitCode::from(EXIT_OK));
        assert_eq!(run(&os(&["-n", "x"]), "echo"), ExitCode::from(EXIT_OK));
        assert_eq!(run(&os(&["-e", "x"]), "echo"), ExitCode::from(EXIT_OK));
        assert_eq!(run(&os(&["-ne", "x"]), "echo"), ExitCode::from(EXIT_OK));
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;

    fn os(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn run_parses_flags_and_escapes() {
        assert_eq!(run(&os(&["--help"]), "echo"), ExitCode::from(EXIT_OK));
        assert_eq!(run(&os(&["-n", "x"]), "echo"), ExitCode::from(EXIT_OK));
        assert_eq!(run(&os(&["-e", "a\nb"]), "echo"), ExitCode::from(EXIT_OK));
        assert_eq!(run(&os(&["-E", "a\nb"]), "echo"), ExitCode::from(EXIT_OK));
        assert_eq!(run(&os(&["-ne", "x"]), "echo"), ExitCode::from(EXIT_OK));
    }
}
