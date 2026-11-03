//! `mktemp` - create a temporary file or directory.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, usage_error};

const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

#[derive(Default)]
struct Options {
    directory: bool,
    dry_run: bool,
    quiet: bool,
    prefix: Option<String>,
}

fn random_bytes(length: usize) -> Vec<u8> {
    let mut buffer = vec![0_u8; length];
    if let Ok(mut file) = File::open("/dev/urandom")
        && file.read_exact(&mut buffer).is_ok()
    {
        return buffer;
    }
    let mut state = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(1)
        ^ (std::process::id() as u64);
    for byte in &mut buffer {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = state as u8;
    }
    buffer
}

fn split_template(template: &str) -> Result<(&str, usize, &str), String> {
    let bytes = template.as_bytes();
    let Some(last) = bytes.iter().rposition(|byte| *byte == b'X') else {
        return Err("template must contain at least 3 consecutive X's".to_string());
    };
    let mut start = last;
    while start > 0 && bytes.get(start - 1) == Some(&b'X') {
        start -= 1;
    }
    let count = last - start + 1;
    if count < 3 {
        return Err("template must contain at least 3 consecutive X's".to_string());
    }
    Ok((
        &template[..start],
        count,
        template.get(last + 1..).unwrap_or_default(),
    ))
}

fn make_name(template: &str, options: &Options) -> Result<String, String> {
    let (prefix, count, suffix) = split_template(template)?;
    for _ in 0..100 {
        let random = random_bytes(count);
        let replacement: String = random
            .iter()
            .map(|byte| {
                ALPHABET
                    .get(*byte as usize % ALPHABET.len())
                    .copied()
                    .unwrap_or(b'a') as char
            })
            .collect();
        let path = format!("{prefix}{replacement}{suffix}");
        if options.dry_run {
            return Ok(path);
        }
        let result = if options.directory {
            fs::DirBuilder::new().mode(0o700).create(&path)
        } else {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map(drop)
        };
        match result {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("{path}: {error}")),
        }
    }
    Err("could not create a unique temporary name".to_string())
}

fn apply_prefix(template: String, options: &Options) -> String {
    let Some(prefix) = &options.prefix else {
        return template;
    };
    if template.contains('/') {
        return template;
    }
    Path::new(prefix).join(template).display().to_string()
}

fn run(args: &[String], program: &str) -> ExitCode {
    let mut options = Options::default();
    let mut template: Option<String> = None;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "--help" => {
                println!("Usage: mktemp [-dqu] [-p DIR] [TEMPLATE]");
                return ExitCode::from(EXIT_OK);
            }
            "-d" | "--directory" => options.directory = true,
            "-u" | "--dry-run" => options.dry_run = true,
            "-q" | "--quiet" => options.quiet = true,
            "-t" => {
                options.prefix =
                    Some(std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string()));
            }
            "-p" | "--tmpdir" => {
                index += 1;
                let Some(directory) = args.get(index) else {
                    return ExitCode::from(usage_error(program, "-p needs a directory"));
                };
                options.prefix = Some(directory.clone());
            }
            value if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            value => {
                if template.is_some() {
                    return ExitCode::from(usage_error(program, "too many templates"));
                }
                template = Some(value.to_string());
            }
        }
        index += 1;
    }
    let template = apply_prefix(
        template.unwrap_or_else(|| "tmp.XXXXXXXXXX".to_string()),
        &options,
    );
    match make_name(&template, &options) {
        Ok(path) => {
            println!("{path}");
            ExitCode::from(EXIT_OK)
        }
        Err(error) => {
            if !options.quiet {
                eprintln!("{program}: {error}");
            }
            ExitCode::from(1)
        }
    }
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(std::env::args().next().as_deref(), "mktemp");
    let args: Vec<String> = std::env::args().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-mktemp-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn files_and_directories_are_created_with_private_modes() {
        let root = temp_root("create");
        let file_template = root.join("file.XXXXXX").display().to_string();
        let file = make_name(&file_template, &Options::default()).expect("file");
        assert!(Path::new(&file).is_file());
        assert_eq!(
            fs::metadata(&file).expect("meta").permissions().mode() & 0o777,
            0o600
        );
        let dir_template = root.join("dir.XXXXXX").display().to_string();
        let options = Options {
            directory: true,
            ..Options::default()
        };
        let directory = make_name(&dir_template, &options).expect("dir");
        assert!(Path::new(&directory).is_dir());
        assert_eq!(
            fs::metadata(&directory).expect("meta").permissions().mode() & 0o777,
            0o700
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn dry_run_does_not_create_and_bad_templates_fail() {
        let root = temp_root("dry");
        let template = root.join("name.XXXXXX").display().to_string();
        let options = Options {
            dry_run: true,
            ..Options::default()
        };
        let path = make_name(&template, &options).expect("dry");
        assert!(!Path::new(&path).exists());
        assert!(make_name("no-placeholders", &Options::default()).is_err());
        assert!(make_name("short.XX", &Options::default()).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn run_handles_help_and_bad_options() {
        assert_eq!(
            run(&["--help".to_string()], "mktemp"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["--bad".to_string()], "mktemp"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
    }
}
