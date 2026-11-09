//! `cut` - select bytes or delimiter-separated fields from each line.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[cfg(test)]
use rinit_tools::EXIT_USAGE;
use rinit_tools::{EXIT_OK, program_name, usage_error};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Bytes,
    Fields,
}

struct Options {
    mode: Mode,
    ranges: Vec<(u64, u64)>,
    delimiter: u8,
    suppress: bool,
}

fn parse_number(value: &str) -> Result<u64, String> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("invalid position '{value}'"));
    }
    let number = value
        .parse::<u64>()
        .map_err(|_| format!("position '{value}' is too large"))?;
    if number == 0 {
        return Err("positions start at 1".to_string());
    }
    Ok(number)
}

fn parse_ranges(spec: &str) -> Result<Vec<(u64, u64)>, String> {
    if spec.is_empty() {
        return Err("a list is required".to_string());
    }
    let mut ranges = Vec::new();
    for part in spec.split(',') {
        if part.is_empty() {
            return Err(format!("empty item in list '{spec}'"));
        }
        let (start, end) = match part.split_once('-') {
            None => {
                let number = parse_number(part)?;
                (number, number)
            }
            Some(("", "")) => return Err("a range needs at least one bound".to_string()),
            Some((left, right)) => {
                let start = if left.is_empty() {
                    1
                } else {
                    parse_number(left)?
                };
                let end = if right.is_empty() {
                    u64::MAX
                } else {
                    parse_number(right)?
                };
                if start > end {
                    return Err(format!("range '{part}' runs backwards"));
                }
                (start, end)
            }
        };
        ranges.push((start, end));
    }
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (start, end) in ranges {
        if let Some(last) = merged.last_mut()
            && start <= last.1.saturating_add(1)
        {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    Ok(merged)
}

fn selected(ranges: &[(u64, u64)], position: u64) -> bool {
    ranges
        .iter()
        .any(|(start, end)| *start <= position && position <= *end)
}

fn parse_delimiter(value: &str) -> Result<u8, String> {
    match value.as_bytes() {
        [byte] => Ok(*byte),
        _ => Err("delimiter must be exactly one byte".to_string()),
    }
}

fn set_list(
    mode: &mut Option<Mode>,
    ranges: &mut Vec<(u64, u64)>,
    seen: &mut bool,
    next: Mode,
    spec: &str,
    program: &str,
) -> Result<(), u8> {
    if *seen {
        return Err(usage_error(program, "only one list may be specified"));
    }
    match parse_ranges(spec) {
        Ok(parsed) => {
            *mode = Some(next);
            *ranges = parsed;
            *seen = true;
            Ok(())
        }
        Err(error) => Err(usage_error(program, &error)),
    }
}

fn cut_stream<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    options: &Options,
) -> io::Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let newline = line.last() == Some(&b'\n');
        let body = if newline {
            &line[..line.len() - 1]
        } else {
            &line[..]
        };
        let has_delimiter = body.contains(&options.delimiter);
        if options.mode == Mode::Fields && options.suppress && !has_delimiter {
            continue;
        }

        let mut out = Vec::new();
        match options.mode {
            Mode::Bytes => {
                for (index, byte) in body.iter().enumerate() {
                    if selected(&options.ranges, index as u64 + 1) {
                        out.push(*byte);
                    }
                }
            }
            Mode::Fields if !has_delimiter => out.extend_from_slice(body),
            Mode::Fields => {
                let mut first = true;
                for (index, field) in body.split(|byte| *byte == options.delimiter).enumerate() {
                    if selected(&options.ranges, index as u64 + 1) {
                        if !first {
                            out.push(options.delimiter);
                        }
                        out.extend_from_slice(field);
                        first = false;
                    }
                }
            }
        }
        writer.write_all(&out)?;
        if newline {
            writer.write_all(b"\n")?;
        }
    }
    Ok(())
}

fn cut_path<W: Write>(path: &Path, options: &Options, writer: &mut W) -> io::Result<()> {
    if path == Path::new("-") {
        let stdin = io::stdin();
        let mut reader = stdin.lock();
        return cut_stream(&mut reader, writer, options);
    }
    let mut reader = BufReader::new(File::open(path)?);
    cut_stream(&mut reader, writer, options)
}

fn run<A: AsRef<OsStr>>(args: &[A], program: &str) -> ExitCode {
    let mut mode: Option<Mode> = None;
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    let mut delimiter = b'\t';
    let mut delimiter_set = false;
    let mut list_seen = false;
    let mut suppress = false;
    let mut files: Vec<PathBuf> = Vec::new();
    let marker = args.iter().position(|arg| arg.as_ref() == OsStr::new("--"));
    let (option_args, trailing): (&[A], &[A]) = match marker {
        Some(index) => (
            args.get(..index).unwrap_or(&[]),
            args.get(index + 1..).unwrap_or(&[]),
        ),
        None => (args, &[] as &[A]),
    };

    let mut index = 0;
    while let Some(arg) = option_args.get(index) {
        let Some(text) = arg.as_ref().to_str() else {
            files.push(PathBuf::from(arg.as_ref()));
            index += 1;
            continue;
        };
        match text {
            "-h" | "--help" => {
                println!("Usage: cut (-b LIST|-c LIST|-f LIST) [-d DELIM] [-s] [FILE...]");
                return ExitCode::from(EXIT_OK);
            }
            "-s" | "--only-delimited" => suppress = true,
            "-b" | "--bytes" | "-c" | "--characters" | "-f" | "--fields" => {
                index += 1;
                let Some(value) = option_args.get(index) else {
                    return ExitCode::from(usage_error(program, &format!("{text} needs a list")));
                };
                let Some(value) = value.as_ref().to_str() else {
                    return ExitCode::from(usage_error(
                        program,
                        &format!("{text} needs a UTF-8 list"),
                    ));
                };
                let next = if text == "-f" || text == "--fields" {
                    Mode::Fields
                } else {
                    Mode::Bytes
                };
                if let Err(code) =
                    set_list(&mut mode, &mut ranges, &mut list_seen, next, value, program)
                {
                    return ExitCode::from(code);
                }
            }
            "-d" | "--delimiter" => {
                index += 1;
                let Some(value) = option_args.get(index) else {
                    return ExitCode::from(usage_error(program, "-d needs a delimiter"));
                };
                let Some(value) = value.as_ref().to_str() else {
                    return ExitCode::from(usage_error(program, "-d needs a UTF-8 delimiter"));
                };
                match parse_delimiter(value) {
                    Ok(byte) => {
                        delimiter = byte;
                        delimiter_set = true;
                    }
                    Err(error) => return ExitCode::from(usage_error(program, &error)),
                }
            }
            _ if text.starts_with("-b") && text.len() > 2 => {
                if let Err(code) = set_list(
                    &mut mode,
                    &mut ranges,
                    &mut list_seen,
                    Mode::Bytes,
                    text.strip_prefix("-b").unwrap_or_default(),
                    program,
                ) {
                    return ExitCode::from(code);
                }
            }
            _ if text.starts_with("-c") && text.len() > 2 => {
                if let Err(code) = set_list(
                    &mut mode,
                    &mut ranges,
                    &mut list_seen,
                    Mode::Bytes,
                    text.strip_prefix("-c").unwrap_or_default(),
                    program,
                ) {
                    return ExitCode::from(code);
                }
            }
            _ if text.starts_with("-f") && text.len() > 2 => {
                if let Err(code) = set_list(
                    &mut mode,
                    &mut ranges,
                    &mut list_seen,
                    Mode::Fields,
                    text.strip_prefix("-f").unwrap_or_default(),
                    program,
                ) {
                    return ExitCode::from(code);
                }
            }
            _ if text.starts_with("-d") && text.len() > 2 => {
                match parse_delimiter(text.strip_prefix("-d").unwrap_or_default()) {
                    Ok(byte) => {
                        delimiter = byte;
                        delimiter_set = true;
                    }
                    Err(error) => return ExitCode::from(usage_error(program, &error)),
                }
            }
            _ if text.starts_with("--bytes=") => {
                if let Err(code) = set_list(
                    &mut mode,
                    &mut ranges,
                    &mut list_seen,
                    Mode::Bytes,
                    text.strip_prefix("--bytes=").unwrap_or_default(),
                    program,
                ) {
                    return ExitCode::from(code);
                }
            }
            _ if text.starts_with("--characters=") => {
                if let Err(code) = set_list(
                    &mut mode,
                    &mut ranges,
                    &mut list_seen,
                    Mode::Bytes,
                    text.strip_prefix("--characters=").unwrap_or_default(),
                    program,
                ) {
                    return ExitCode::from(code);
                }
            }
            _ if text.starts_with("--fields=") => {
                if let Err(code) = set_list(
                    &mut mode,
                    &mut ranges,
                    &mut list_seen,
                    Mode::Fields,
                    text.strip_prefix("--fields=").unwrap_or_default(),
                    program,
                ) {
                    return ExitCode::from(code);
                }
            }
            _ if text.starts_with("--delimiter=") => {
                match parse_delimiter(text.strip_prefix("--delimiter=").unwrap_or_default()) {
                    Ok(byte) => {
                        delimiter = byte;
                        delimiter_set = true;
                    }
                    Err(error) => return ExitCode::from(usage_error(program, &error)),
                }
            }
            value if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            _ => files.push(PathBuf::from(arg.as_ref())),
        }
        index += 1;
    }
    files.extend(trailing.iter().map(|arg| PathBuf::from(arg.as_ref())));
    if files.is_empty() {
        files.push(PathBuf::from("-"));
    }

    let Some(mode) = mode else {
        return ExitCode::from(usage_error(program, "one of -b, -c or -f is required"));
    };
    if suppress && mode != Mode::Fields {
        return ExitCode::from(usage_error(program, "-s is only valid with -f"));
    }
    if delimiter_set && mode != Mode::Fields {
        return ExitCode::from(usage_error(program, "-d is only valid with -f"));
    }

    let options = Options {
        mode,
        ranges,
        delimiter,
        suppress,
    };
    let mut writer = io::stdout().lock();
    let mut failed = false;
    for path in files {
        if let Err(error) = cut_path(&path, &options, &mut writer) {
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
        "cut",
    );
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn fields(input: &[u8], spec: &str, delimiter: u8, suppress: bool) -> Vec<u8> {
        let options = Options {
            mode: Mode::Fields,
            ranges: parse_ranges(spec).expect("ranges"),
            delimiter,
            suppress,
        };
        let mut reader = Cursor::new(input);
        let mut writer = Vec::new();
        cut_stream(&mut reader, &mut writer, &options).expect("cut");
        writer
    }

    fn bytes(input: &[u8], spec: &str) -> Vec<u8> {
        let options = Options {
            mode: Mode::Bytes,
            ranges: parse_ranges(spec).expect("ranges"),
            delimiter: b'\t',
            suppress: false,
        };
        let mut reader = Cursor::new(input);
        let mut writer = Vec::new();
        cut_stream(&mut reader, &mut writer, &options).expect("cut");
        writer
    }

    #[test]
    fn ranges_parse_merge_and_reject_bad_bounds() {
        assert_eq!(
            parse_ranges("1,3-5,7-").expect("ranges"),
            vec![(1, 1), (3, 5), (7, u64::MAX)]
        );
        assert_eq!(parse_ranges("1-3,2-4").expect("ranges"), vec![(1, 4)]);
        assert_eq!(parse_ranges("-3").expect("ranges"), vec![(1, 3)]);
        for bad in ["", "0", "3-1", "1,,2", "-", "a", "1-a"] {
            assert!(parse_ranges(bad).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn fields_select_in_input_order_and_keep_empty_fields() {
        assert_eq!(fields(b"a:b:c\nx:y\n", "3,1", b':', false), b"a:c\nx\n");
        assert_eq!(fields(b"a::c\n", "2", b':', false), b"\n");
        assert_eq!(fields(b"plain\n", "2", b':', false), b"plain\n");
        assert_eq!(fields(b"plain\n", "2", b':', true), b"");
    }

    #[test]
    fn bytes_select_ranges_and_preserve_line_endings() {
        assert_eq!(bytes(b"abcd\n", "1-2,4"), b"abd\n");
        assert_eq!(bytes(b"abcd", "2-"), b"bcd");
        assert_eq!(bytes(b"\n", "1"), b"\n");
    }

    #[test]
    fn delimiters_must_be_one_byte() {
        assert_eq!(parse_delimiter(":"), Ok(b':'));
        assert!(parse_delimiter("").is_err());
        assert!(parse_delimiter("ab").is_err());
        assert!(parse_delimiter("é").is_err());
    }

    #[test]
    fn run_handles_files_and_rejects_incomplete_modes() {
        let root = std::env::temp_dir().join(format!("rinit-cut-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp");
        let file = root.join("file");
        std::fs::write(&file, b"a:b\n").expect("file");
        let file = file.as_os_str().to_os_string();
        assert_eq!(
            run(
                &[OsString::from("-d:"), OsString::from("-f2"), file] as &[OsString],
                "cut"
            ),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["-s".to_string()] as &[String], "cut"),
            ExitCode::from(EXIT_USAGE)
        );
        assert_eq!(
            run(&["-f2".to_string(), "-d".to_string()] as &[String], "cut"),
            ExitCode::from(EXIT_USAGE)
        );
        assert_eq!(
            run(&["-f2".to_string(), "-s".to_string()] as &[String], "cut"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["-b1".to_string(), "-f2".to_string()] as &[String], "cut"),
            ExitCode::from(EXIT_USAGE)
        );
        assert_eq!(
            run(&["-b1".to_string(), "-c2".to_string()] as &[String], "cut"),
            ExitCode::from(EXIT_USAGE)
        );
        assert_eq!(
            run(&["-b1".to_string(), "-b2".to_string()] as &[String], "cut"),
            ExitCode::from(EXIT_USAGE)
        );
        assert_eq!(
            run(&["-f1".to_string(), "-f2".to_string()] as &[String], "cut"),
            ExitCode::from(EXIT_USAGE)
        );
        assert_eq!(
            run(&["-f2".to_string(), "-d::".to_string()] as &[String], "cut"),
            ExitCode::from(EXIT_USAGE)
        );
        assert_eq!(
            run(&["--help".to_string()] as &[String], "cut"),
            ExitCode::from(EXIT_OK)
        );
        let missing = root.join("missing");
        assert_eq!(
            run(
                &[OsString::from("-f1"), missing.as_os_str().to_os_string()] as &[OsString],
                "cut"
            ),
            ExitCode::from(1)
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
