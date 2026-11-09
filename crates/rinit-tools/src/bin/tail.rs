//! `tail` - print the last lines or bytes of each file.
//!
//! Follow mode is deliberately absent: it has different failure, rotation and
//! signal semantics from a finite read, and the service manager already owns
//! the live log path.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[cfg(test)]
use rinit_tools::EXIT_USAGE;
use rinit_tools::{EXIT_OK, program_name, usage_error};

/// Largest byte count a non-seekable input will buffer.
///
/// Regular files are read by seeking to the requested tail and streaming it, so
/// they do not allocate this buffer. A pipe or a non-regular path has no way to
/// seek; a user asking for an arbitrarily large tail there would otherwise make
/// the tool allocate that much memory, or abort on the allocation itself.
const MAX_TAIL_BYTES: u64 = 16 * 1024 * 1024;

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

fn capacity(count: u64) -> io::Result<usize> {
    usize::try_from(count)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "count is too large"))
}

fn byte_capacity(count: u64) -> io::Result<usize> {
    if count > MAX_TAIL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("byte count exceeds the {MAX_TAIL_BYTES}-byte buffer limit"),
        ));
    }
    capacity(count)
}

fn tail_lines<R: BufRead + ?Sized, W: Write>(
    reader: &mut R,
    writer: &mut W,
    count: u64,
) -> io::Result<()> {
    if count == 0 {
        return Ok(());
    }
    let limit = capacity(count)?;
    let mut ring: VecDeque<Vec<u8>> = VecDeque::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if ring.len() == limit {
            ring.pop_front();
        }
        ring.push_back(std::mem::take(&mut line));
    }
    for line in ring {
        writer.write_all(&line)?;
    }
    Ok(())
}

fn tail_bytes<R: Read + ?Sized, W: Write>(
    reader: &mut R,
    writer: &mut W,
    count: u64,
) -> io::Result<()> {
    if count == 0 {
        return Ok(());
    }
    let limit = byte_capacity(count)?;
    let mut ring = vec![0_u8; limit];
    let mut position = 0;
    let mut filled = 0;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        for byte in &buffer[..read] {
            ring[position] = *byte;
            position += 1;
            if position == limit {
                position = 0;
            }
            if filled < limit {
                filled += 1;
            }
        }
    }
    if filled < limit {
        writer.write_all(&ring[..filled])?;
    } else {
        writer.write_all(&ring[position..])?;
        writer.write_all(&ring[..position])?;
    }
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

fn tail_path<W: Write>(
    path: &Path,
    options: &Options,
    writer: &mut W,
    show_headers: bool,
    header_count: &mut usize,
) -> io::Result<()> {
    if path == Path::new("-") {
        let stdin = io::stdin();
        let mut reader = stdin.lock();
        write_header(path, writer, show_headers, header_count)?;
        return match options.unit {
            Unit::Lines => tail_lines(&mut reader, writer, options.count),
            Unit::Bytes => tail_bytes(&mut reader, writer, options.count),
        };
    }

    let mut file = File::open(path)?;
    if options.unit == Unit::Bytes
        && let Ok(metadata) = file.metadata()
        && metadata.file_type().is_file()
    {
        let start = metadata.len().saturating_sub(options.count);
        if file.seek(SeekFrom::Start(start)).is_ok() {
            write_header(path, writer, show_headers, header_count)?;
            io::copy(&mut file.take(options.count), writer)?;
            return Ok(());
        }
    }

    let mut reader = BufReader::new(file);
    write_header(path, writer, show_headers, header_count)?;
    match options.unit {
        Unit::Lines => tail_lines(&mut reader, writer, options.count),
        Unit::Bytes => tail_bytes(&mut reader, writer, options.count),
    }
}

fn byte_count_needs_a_buffer(count: u64) -> bool {
    count > MAX_TAIL_BYTES || usize::try_from(count).is_err()
}

fn path_needs_buffering(path: &Path) -> bool {
    if path == Path::new("-") {
        return true;
    }
    std::fs::metadata(path)
        .map(|metadata| !metadata.file_type().is_file())
        .unwrap_or(false)
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
            files.push(PathBuf::from(arg.as_ref()));
            index += 1;
            continue;
        };
        match text {
            "-h" | "--help" => {
                println!("Usage: tail [-n N|-c N] [-q|-v] [FILE...]");
                return ExitCode::from(EXIT_OK);
            }
            "-f" | "-F" | "--follow" | "-s" | "--sleep-interval" => {
                return ExitCode::from(usage_error(program, "follow mode is not supported"));
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
    if options.unit == Unit::Bytes && byte_count_needs_a_buffer(options.count) {
        for path in &files {
            if path_needs_buffering(path) {
                return ExitCode::from(usage_error(
                    program,
                    "byte count is too large for non-seekable input",
                ));
            }
        }
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
        if let Err(error) = tail_path(
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
        "tail",
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
        tail_lines(&mut reader, &mut writer, count).expect("tail");
        writer
    }

    fn bytes(input: &[u8], count: u64) -> Vec<u8> {
        let mut reader = Cursor::new(input);
        let mut writer = Vec::new();
        tail_bytes(&mut reader, &mut writer, count).expect("tail");
        writer
    }

    #[test]
    fn lines_and_bytes_are_exact() {
        assert_eq!(lines(b"a\nb\nc\n", 2), b"b\nc\n");
        assert_eq!(lines(b"a\nb\n", 9), b"a\nb\n");
        assert_eq!(lines(b"a\nb\n", 0), b"");
        assert_eq!(lines(b"a\nb", 1), b"b");
        assert_eq!(bytes(b"abcdef", 3), b"def");
        assert_eq!(bytes(b"abc", 9), b"abc");
        assert_eq!(bytes(b"abc", 0), b"");
        assert_eq!(bytes(b"0123456789", 3), b"789");
    }

    #[test]
    fn counts_are_non_negative_decimals() {
        assert_eq!(parse_count("10"), Ok(10));
        assert_eq!(parse_count("0"), Ok(0));
        assert!(parse_count("+1").is_err());
        assert!(parse_count("-1").is_err());
        assert!(parse_count("1k").is_err());
    }

    #[test]
    fn headers_separate_multiple_files() {
        let root = std::env::temp_dir().join(format!("rinit-tail-multi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp");
        let first = root.join("first");
        let second = root.join("second");
        std::fs::write(&first, b"one\n").expect("first");
        std::fs::write(&second, b"two\n").expect("second");
        let mut out = Vec::new();
        let mut headers = 0;
        tail_path(&first, &Options::default(), &mut out, true, &mut headers).expect("first");
        tail_path(&second, &Options::default(), &mut out, true, &mut headers).expect("second");
        let text = String::from_utf8(out).expect("UTF-8 fixture");
        assert_eq!(text.matches("==>").count(), 2, "{text}");
        assert!(text.contains("one\n\n==>"), "{text}");
        assert!(text.ends_with("two\n"), "{text}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn regular_files_do_not_allocate_the_byte_count() {
        let root = std::env::temp_dir().join(format!("rinit-tail-large-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp");
        let path = root.join("file");
        std::fs::write(&path, b"abc").expect("file");
        let options = Options {
            count: MAX_TAIL_BYTES + 1,
            unit: Unit::Bytes,
            header: Header::Never,
        };
        let mut out = Vec::new();
        let mut headers = 0;
        tail_path(&path, &options, &mut out, false, &mut headers).expect("seek and stream");
        assert_eq!(out, b"abc");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn non_seekable_byte_counts_are_bounded_before_allocation() {
        let mut reader = Cursor::new(b"abc");
        let mut writer = Vec::new();
        let error = tail_bytes(&mut reader, &mut writer, MAX_TAIL_BYTES + 1)
            .expect_err("an enormous pipe tail must be refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(writer.is_empty());
    }

    #[test]
    fn stdin_has_a_readable_header() {
        assert_eq!(header_name(Path::new("-")), b"standard input");
        assert_eq!(header_name(Path::new("/tmp/file")), b"/tmp/file");
    }

    #[test]
    fn run_handles_options_and_rejects_follow_mode() {
        let root = std::env::temp_dir().join(format!("rinit-tail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp");
        let file = root.join("file");
        std::fs::write(&file, b"one\ntwo\n").expect("file");
        let file = file.as_os_str().to_os_string();
        assert_eq!(
            run(&[OsString::from("-n1"), file] as &[OsString], "tail"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["-c3".to_string()] as &[String], "tail"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["-f".to_string()] as &[String], "tail"),
            ExitCode::from(EXIT_USAGE)
        );
        assert_eq!(
            run(&["--bad".to_string()] as &[String], "tail"),
            ExitCode::from(EXIT_USAGE)
        );
        assert_eq!(
            run(&["-n".to_string()] as &[String], "tail"),
            ExitCode::from(EXIT_USAGE)
        );
        assert_eq!(
            run(&["-n".to_string(), "x".to_string()] as &[String], "tail"),
            ExitCode::from(EXIT_USAGE)
        );
        assert_eq!(
            run(&["--help".to_string()] as &[String], "tail"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(
                &[
                    "-c".to_string(),
                    (MAX_TAIL_BYTES + 1).to_string(),
                    "-".to_string()
                ] as &[String],
                "tail"
            ),
            ExitCode::from(EXIT_USAGE)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_trailing_newline_free_line_is_preserved() {
        assert_eq!(lines(b"only", 1), b"only");
        assert_eq!(bytes(b"only", 4), b"only");
    }
}
