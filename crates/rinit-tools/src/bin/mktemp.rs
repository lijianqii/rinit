//! `mktemp` - create a temporary file or directory.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, usage_error};

const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

#[derive(Default)]
struct Options {
    directory: bool,
    dry_run: bool,
    quiet: bool,
    prefix: Option<PathBuf>,
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

fn split_template(template: &OsStr) -> Result<(&[u8], usize, &[u8]), String> {
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
        bytes.get(..start).unwrap_or_default(),
        count,
        bytes.get(last + 1..).unwrap_or_default(),
    ))
}

fn make_name(template: impl AsRef<OsStr>, options: &Options) -> Result<PathBuf, String> {
    let (prefix, count, suffix) = split_template(template.as_ref())?;
    for _ in 0..100 {
        let random = random_bytes(count);
        let mut name = Vec::with_capacity(prefix.len() + count + suffix.len());
        name.extend_from_slice(prefix);
        name.extend(random.iter().map(|byte| {
            ALPHABET
                .get(*byte as usize % ALPHABET.len())
                .copied()
                .unwrap_or(b'a')
        }));
        name.extend_from_slice(suffix);
        let path = PathBuf::from(OsString::from_vec(name));
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
            Err(error) => return Err(format!("{}: {error}", path.display())),
        }
    }
    Err("could not create a unique temporary name".to_string())
}

fn default_temp_dir() -> OsString {
    std::env::var_os("TMPDIR")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from("/tmp"))
}

fn apply_prefix(template: OsString, options: &Options) -> OsString {
    let Some(prefix) = &options.prefix else {
        return template;
    };
    if template.as_bytes().contains(&b'/') {
        return template;
    }
    prefix.join(template).into_os_string()
}

fn run<A: AsRef<OsStr>>(args: &[A], program: &str) -> ExitCode {
    let mut options = Options::default();
    let mut template: Option<OsString> = None;
    let marker = args.iter().position(|arg| arg.as_ref() == OsStr::new("--"));
    let (options_args, trailing): (&[A], &[A]) = match marker {
        Some(index) => (
            args.get(..index).unwrap_or(&[]),
            args.get(index + 1..).unwrap_or(&[]),
        ),
        None => (args, &[] as &[A]),
    };
    let mut index = 0;
    while let Some(arg) = options_args.get(index) {
        match arg.as_ref().to_str() {
            Some("--help") => {
                println!("Usage: mktemp [-dqu] [-p DIR] [TEMPLATE]");
                return ExitCode::from(EXIT_OK);
            }
            Some("-d" | "--directory") => options.directory = true,
            Some("-u" | "--dry-run") => options.dry_run = true,
            Some("-q" | "--quiet") => options.quiet = true,
            Some("-t") => {
                options.prefix = Some(PathBuf::from(default_temp_dir()));
            }
            Some("-p" | "--tmpdir") => {
                index += 1;
                let Some(directory) = options_args.get(index) else {
                    return ExitCode::from(usage_error(program, "-p needs a directory"));
                };
                options.prefix = Some(PathBuf::from(directory.as_ref()));
            }
            Some(value) if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            _ => {
                if template.is_some() {
                    return ExitCode::from(usage_error(program, "too many templates"));
                }
                template = Some(OsString::from(arg.as_ref()));
            }
        }
        index += 1;
    }
    for value in trailing {
        if template.is_some() {
            return ExitCode::from(usage_error(program, "too many templates"));
        }
        template = Some(OsString::from(value.as_ref()));
    }
    // GNU/BusyBox `mktemp` with no template uses `$TMPDIR` (or `/tmp`), while
    // an explicit relative template is created in the current directory.
    let template: OsString = match template {
        Some(template) => template,
        None => {
            if options.prefix.is_none() {
                options.prefix = Some(PathBuf::from(default_temp_dir()));
            }
            OsString::from("tmp.XXXXXXXXXX")
        }
    };
    let template = apply_prefix(template, &options);
    match make_name(&template, &options) {
        Ok(path) => {
            println!("{}", path.display());
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
    let program = program_name(
        std::env::args_os()
            .next()
            .as_deref()
            .and_then(|arg| arg.to_str()),
        "mktemp",
    );
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-mktemp-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn a_bare_template_uses_the_temporary_directory() {
        let options = Options {
            prefix: Some(PathBuf::from("/tmp")),
            ..Options::default()
        };
        assert_eq!(
            apply_prefix(OsString::from("tmp.XXXXXX"), &options),
            "/tmp/tmp.XXXXXX"
        );
        assert!(Path::new(&default_temp_dir()).is_absolute());
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

#[cfg(test)]
mod coverage_tests {
    use super::*;
    use std::path::PathBuf;

    fn root(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("rinit-{}-{}-{tag}", "mktemp", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn run_parses_options_and_reports_usage_errors() {
        let root = root("run");
        let template = root.join("x.XXXXXX").display().to_string();
        assert_eq!(
            run(&["-u".to_string(), template.clone()], "mktemp"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["-d".to_string(), template], "mktemp"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["--help".to_string()], "mktemp"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["--bad".to_string()], "mktemp"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        assert_eq!(
            run(&["-p".to_string()], "mktemp"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        assert_eq!(
            run(&["a".to_string(), "b".to_string()], "mktemp"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
