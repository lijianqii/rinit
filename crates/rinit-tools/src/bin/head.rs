//! `head` - print the first lines or bytes of each file.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[cfg(test)]
use rinit_tools::EXIT_USAGE;
use rinit_tools::{EXIT_OK, program_name, usage_error};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Unit {
    Lines,
    Bytes,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Header {
    Auto,
    Always,
    Never,
}

struct Options {
    count: u64,
    unit: Unit,
    header: Header,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            count: 10,
            unit: Unit::Lines,
            header: Header::Auto,
        }
    }
}

fn parse_count(value: &str) -> Result<u64, String> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("invalid count '{value}'"));
    }
    value
        .parse::<u64>()
        .map_err(|_| format!("count '{value}' is too large"))
}

fn head_lines<R: BufRead + ?Sized, W: Write>(
    reader: &mut R,
    writer: &mut W,
    count: u64,
) -> io::Result<()> {
    let mut line = Vec::new();
    let mut written = 0_u64;
    while written < count {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        writer.write_all(&line)?;
        written += 1;
    }
    Ok(())
}

fn head_bytes<R: Read + ?Sized, W: Write>(
    reader: &mut R,
    writer: &mut W,
    count: u64,
) -> io::Result<()> {
    io::copy(&mut reader.take(count), writer)?;
    Ok(())
}

fn header_name(path: &Path) -> &[u8] {
    if path == Path::new("-") {
        b"standard input"
    } else {
        path.as_os_str().as_bytes()
    }
}

fn write_header<W: Write>(
    path: &Path,
    writer: &mut W,
    show_headers: bool,
    header_count: &mut usize,
) -> io::Result<()> {
    if show_headers {
        if *header_count > 0 {
            writer.write_all(b"\n")?;
        }
        writer.write_all(b"==> ")?;
        writer.write_all(header_name(path))?;
        writer.write_all(b" <==\n")?;
        *header_count += 1;
    }
    Ok(())
}

fn head_path<W: Write>(
    path: &Path,
    options: &Options,
    writer: &mut W,
    show_headers: bool,
    header_count: &mut usize,
) -> io::Result<()> {
    let mut stdin;
    let mut file;
    let reader: &mut dyn BufRead = if path == Path::new("-") {
        stdin = io::stdin().lock();
        &mut stdin
    } else {
        file = BufReader::new(File::open(path)?);
        &mut file
    };

    write_header(path, writer, show_headers, header_count)?;

    match options.unit {
        Unit::Lines => head_lines(reader, writer, options.count),
        Unit::Bytes => head_bytes(reader, writer, options.count),
    }
}

fn run<A: AsRef<OsStr>>(args: &[A], program: &str) -> ExitCode {
    let mut options = Options::default();
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
            return ExitCode::from(usage_error(program, "an argument is not valid UTF-8"));
        };
        match text {
            "-h" | "--help" => {
                println!("Usage: head [-n N|-c N] [-q|-v] [FILE...]");
                return ExitCode::from(EXIT_OK);
            }
            "-q" | "--quiet" | "--silent" => options.header = Header::Never,
            "-v" | "--verbose" => options.header = Header::Always,
            "-n" | "--lines" => {
                index += 1;
                let Some(value) = option_args.get(index) else {
                    return ExitCode::from(usage_error(program, "-n needs a count"));
                };
                let Some(value) = value.as_ref().to_str() else {
                    return ExitCode::from(usage_error(program, "-n needs a UTF-8 count"));
                };
                match parse_count(value) {
                    Ok(count) => {
                        options.count = count;
                        options.unit = Unit::Lines;
                    }
                    Err(error) => return ExitCode::from(usage_error(program, &error)),
                }
            }
            "-c" | "--bytes" => {
                index += 1;
                let Some(value) = option_args.get(index) else {
                    return ExitCode::from(usage_error(program, "-c needs a count"));
                };
                let Some(value) = value.as_ref().to_str() else {
                    return ExitCode::from(usage_error(program, "-c needs a UTF-8 count"));
                };
                match parse_count(value) {
                    Ok(count) => {
                        options.count = count;
                        options.unit = Unit::Bytes;
                    }
                    Err(error) => return ExitCode::from(usage_error(program, &error)),
                }
            }
            _ if text.starts_with("-n") && text.len() > 2 => {
                let value = text.strip_prefix("-n").unwrap_or_default();
                match parse_count(value) {
                    Ok(count) => {
                        options.count = count;
                        options.unit = Unit::Lines;
                    }
                    Err(error) => return ExitCode::from(usage_error(program, &error)),
                }
            }
            _ if text.starts_with("-c") && text.len() > 2 => {
                let value = text.strip_prefix("-c").unwrap_or_default();
                match parse_count(value) {
                    Ok(count) => {
                        options.count = count;
                        options.unit = Unit::Bytes;
                    }
                    Err(error) => return ExitCode::from(usage_error(program, &error)),
                }
            }
            _ if text.starts_with("--lines=") => {
                let value = text.strip_prefix("--lines=").unwrap_or_default();
                match parse_count(value) {
                    Ok(count) => {
                        options.count = count;
                        options.unit = Unit::Lines;
                    }
                    Err(error) => return ExitCode::from(usage_error(program, &error)),
                }
            }
            _ if text.starts_with("--bytes=") => {
                let value = text.strip_prefix("--bytes=").unwrap_or_default();
                match parse_count(value) {
                    Ok(count) => {
                        options.count = count;
                        options.unit = Unit::Bytes;
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

    let show_headers = match options.header {
        Header::Always => true,
        Header::Never => false,
        Header::Auto => files.len() > 1,
    };
    let mut writer = io::stdout().lock();
    let mut header_count = 0;
    let mut failed = false;
    for path in files {
        if let Err(error) = head_path(
            &path,
            &options,
            &mut writer,
            show_headers,
            &mut header_count,
        ) {
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
        "head",
    );
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn lines(input: &[u8], count: u64) -> Vec<u8> {
        let mut reader = Cursor::new(input);
        let mut writer = Vec::new();
        head_lines(&mut reader, &mut writer, count).expect("head");
        writer
    }

    fn bytes(input: &[u8], count: u64) -> Vec<u8> {
        let mut reader = Cursor::new(input);
        let mut writer = Vec::new();
        head_bytes(&mut reader, &mut writer, count).expect("head");
        writer
    }

    #[test]
    fn lines_and_bytes_are_exact() {
        assert_eq!(lines(b"a\nb\nc\n", 2), b"a\nb\n");
        assert_eq!(lines(b"a\nb\nc", 9), b"a\nb\nc");
        assert_eq!(lines(b"a\nb\n", 0), b"");
        assert_eq!(bytes(b"abcdef", 3), b"abc");
        assert_eq!(bytes(b"abc", 9), b"abc");
        assert_eq!(bytes(b"abc", 0), b"");
    }

    #[test]
    fn counts_are_non_negative_decimals() {
        assert_eq!(parse_count("10"), Ok(10));
        assert_eq!(parse_count("0"), Ok(0));
        assert!(parse_count("-1").is_err());
        assert!(parse_count("1k").is_err());
        assert!(parse_count("").is_err());
    }

    #[test]
    fn a_trailing_newline_free_line_is_preserved() {
        assert_eq!(lines(b"only", 1), b"only");
        assert_eq!(bytes(b"only", 4), b"only");
    }

    #[test]
    fn stdin_has_a_readable_header() {
        assert_eq!(header_name(Path::new("-")), b"standard input");
        assert_eq!(header_name(Path::new("/tmp/file")), b"/tmp/file");
    }

    #[test]
    fn headers_separate_multiple_files() {
        let root = std::env::temp_dir().join(format!("rinit-head-multi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp");
        let first = root.join("first");
        let second = root.join("second");
        std::fs::write(&first, b"one\n").expect("first");
        std::fs::write(&second, b"two\n").expect("second");
        let mut out = Vec::new();
        let mut headers = 0;
        head_path(&first, &Options::default(), &mut out, true, &mut headers).expect("first");
        head_path(&second, &Options::default(), &mut out, true, &mut headers).expect("second");
        let text = String::from_utf8(out).expect("UTF-8 fixture");
        assert_eq!(text.matches("==>").count(), 2, "{text}");
        assert!(text.contains("one\n\n==>"), "{text}");
        assert!(text.ends_with("two\n"), "{text}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_non_utf8_file_name_is_emitted_as_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let root = std::env::temp_dir().join(format!("rinit-head-bytes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp");
        let name = OsString::from_vec(vec![b'f', 0xff, b'x']);
        let path = root.join(&name);
        std::fs::write(&path, b"one\n").expect("file");
        let mut out = Vec::new();
        let mut headers = 0;
        head_path(&path, &Options::default(), &mut out, true, &mut headers).expect("head");
        assert!(
            out.windows(name.len())
                .any(|window| window == name.as_bytes()),
            "the raw name must survive: {out:?}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn run_handles_options_and_files() {
        let root = std::env::temp_dir().join(format!("rinit-head-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp");
        let file = root.join("file");
        std::fs::write(&file, b"one\ntwo\n").expect("file");
        let file = file.as_os_str().to_os_string();
        assert_eq!(
            run(&["-n".to_string(), "1".to_string()] as &[String], "head"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&[OsString::from("-c3"), file] as &[OsString], "head"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["--bad".to_string()] as &[String], "head"),
            ExitCode::from(EXIT_USAGE)
        );
        assert_eq!(
            run(&["-n".to_string()] as &[String], "head"),
            ExitCode::from(EXIT_USAGE)
        );
        assert_eq!(
            run(&["-n".to_string(), "x".to_string()] as &[String], "head"),
            ExitCode::from(EXIT_USAGE)
        );
        assert_eq!(
            run(&["--help".to_string()] as &[String], "head"),
            ExitCode::from(EXIT_OK)
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
