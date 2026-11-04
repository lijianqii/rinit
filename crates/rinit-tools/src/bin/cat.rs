//! `cat` - concatenate files and standard input.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, usage_error};

#[derive(Clone, Copy, Default)]
struct Options {
    number: bool,
    number_nonblank: bool,
    squeeze: bool,
}

#[derive(Default)]
struct State {
    line: u64,
}

fn is_blank(line: &[u8]) -> bool {
    line.iter().all(|byte| *byte == b'\n' || *byte == b'\r')
}

fn copy_stream<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    options: Options,
    state: &mut State,
) -> io::Result<()> {
    let mut line = Vec::new();
    let mut blank_run = 0_u32;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let blank = is_blank(&line);
        if options.squeeze {
            if blank {
                blank_run += 1;
                if blank_run > 1 {
                    continue;
                }
            } else {
                blank_run = 0;
            }
        }
        if options.number_nonblank {
            if !blank {
                state.line += 1;
                write!(writer, "{:>6}\t", state.line)?;
            }
        } else if options.number {
            state.line += 1;
            write!(writer, "{:>6}\t", state.line)?;
        }
        writer.write_all(&line)?;
    }
    Ok(())
}

fn cat_path(path: &Path, options: Options, state: &mut State) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    if path == Path::new("-") {
        let stdin = io::stdin();
        let mut reader = stdin.lock();
        return copy_stream(&mut reader, &mut stdout, options, state);
    }
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    copy_stream(&mut reader, &mut stdout, options, state)
}

fn run<A: AsRef<OsStr>>(args: &[A], program: &str) -> ExitCode {
    let mut options = Options::default();
    let mut files: Vec<PathBuf> = Vec::new();
    let marker = args.iter().position(|arg| arg.as_ref() == OsStr::new("--"));
    let (options_args, trailing): (&[A], &[A]) = match marker {
        Some(index) => (
            args.get(..index).unwrap_or(&[]),
            args.get(index + 1..).unwrap_or(&[]),
        ),
        None => (args, &[] as &[A]),
    };
    for arg in options_args {
        match arg.as_ref().to_str() {
            Some("--help") => {
                println!("Usage: cat [-bnsu] [FILE...]");
                return ExitCode::from(EXIT_OK);
            }
            Some("-n" | "--number") => options.number = true,
            Some("-b" | "--number-nonblank") => options.number_nonblank = true,
            Some("-s" | "--squeeze-blank") => options.squeeze = true,
            Some("-u") => {}
            Some(value) if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            _ => files.push(PathBuf::from(arg.as_ref())),
        }
    }
    files.extend(trailing.iter().map(|arg| PathBuf::from(arg.as_ref())));
    if files.is_empty() {
        files.push(PathBuf::from("-"));
    }
    let mut state = State::default();
    let mut failed = false;
    for path in files {
        if let Err(error) = cat_path(&path, options, &mut state) {
            eprintln!("{program}: {}: {error}", path.display());
            failed = true;
        }
    }
    ExitCode::from(if failed { 1 } else { EXIT_OK })
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(
        std::env::args_os()
            .next()
            .as_deref()
            .and_then(|arg| arg.to_str()),
        "cat",
    );
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn output(input: &[u8], options: Options) -> Vec<u8> {
        let mut reader = Cursor::new(input);
        let mut writer = Vec::new();
        let mut state = State::default();
        copy_stream(&mut reader, &mut writer, options, &mut state).expect("copy");
        writer
    }

    #[test]
    fn plain_copy_is_byte_exact() {
        assert_eq!(output(b"a\0b\nc", Options::default()), b"a\0b\nc");
    }

    #[test]
    fn numbers_all_lines_or_only_nonblank() {
        let input = b"one\n\nthree\n";
        let numbered = output(
            input,
            Options {
                number: true,
                ..Options::default()
            },
        );
        assert_eq!(
            String::from_utf8(numbered).expect("utf8"),
            "     1\tone\n     2\t\n     3\tthree\n"
        );
        let nonblank = output(
            input,
            Options {
                number_nonblank: true,
                ..Options::default()
            },
        );
        assert_eq!(
            String::from_utf8(nonblank).expect("utf8"),
            "     1\tone\n\n     2\tthree\n"
        );
    }

    #[test]
    fn squeeze_collapses_repeated_blank_lines() {
        let input = b"one\n\n\n\ntwo\n";
        let squeezed = output(
            input,
            Options {
                squeeze: true,
                ..Options::default()
            },
        );
        assert_eq!(squeezed, b"one\n\ntwo\n");
    }

    #[test]
    fn run_reports_missing_files_and_bad_options() {
        let missing =
            std::env::temp_dir().join(format!("rinit-cat-missing-{}", std::process::id()));
        let missing = missing.display().to_string();
        assert_eq!(run(&[missing], "cat"), ExitCode::from(1));
        assert_eq!(run(&["--help".to_string()], "cat"), ExitCode::from(EXIT_OK));
        assert_eq!(
            run(&["--bad".to_string()], "cat"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
    }
}
