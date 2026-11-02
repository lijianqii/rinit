//! `cp` - copy files and directory trees.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rinit_tools::fsutil;
use rinit_tools::{EXIT_OK, program_name, usage_error};

#[derive(Clone, Copy, Default)]
struct Options {
    recursive: bool,
    preserve: bool,
    force: bool,
    no_clobber: bool,
}

fn target_for(src: &Path, dst: &Path, dst_is_dir: bool) -> Result<PathBuf, String> {
    if dst_is_dir {
        let name = src
            .file_name()
            .ok_or_else(|| format!("cannot determine the name of '{}'", src.display()))?;
        Ok(dst.join(name))
    } else {
        Ok(dst.to_path_buf())
    }
}

fn copy_one(src: &Path, dst: &Path, options: Options) -> Result<(), String> {
    if options.no_clobber && fs::symlink_metadata(dst).is_ok() {
        return Ok(());
    }
    let meta = fs::symlink_metadata(src).map_err(|error| format!("{}: {error}", src.display()))?;
    if meta.is_dir() && !options.recursive {
        return Err(format!("{}: omitting directory", src.display()));
    }
    if options.force && !meta.is_dir() {
        match fs::symlink_metadata(dst) {
            Ok(existing) if !existing.is_dir() => {
                fs::remove_file(dst).map_err(|error| format!("{}: {error}", dst.display()))?;
            }
            _ => {}
        }
    }
    fsutil::copy_entry(src, dst, options.recursive, options.preserve)
        .map_err(|error| format!("{} -> {}: {error}", src.display(), dst.display()))
}

fn run(args: &[String], program: &str) -> ExitCode {
    let mut options = Options::default();
    let mut paths: Vec<String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--help" => {
                println!("Usage: cp [-rRafpn] SOURCE... DEST");
                return ExitCode::from(EXIT_OK);
            }
            "-r" | "-R" | "--recursive" => options.recursive = true,
            "-a" | "--archive" => {
                options.recursive = true;
                options.preserve = true;
            }
            "-p" | "--preserve" => options.preserve = true,
            "-f" | "--force" => options.force = true,
            "-n" | "--no-clobber" => options.no_clobber = true,
            value if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            value => paths.push(value.to_string()),
        }
    }
    let (dst, sources) = match paths.split_last() {
        Some((dst, sources)) if !sources.is_empty() => (dst, sources),
        _ => return ExitCode::from(usage_error(program, "SOURCE and DEST are required")),
    };
    let dst_path = Path::new(dst);
    let dst_is_dir = fs::metadata(dst_path)
        .map(|meta| meta.is_dir())
        .unwrap_or(false);
    if sources.len() > 1 && !dst_is_dir {
        return ExitCode::from(usage_error(program, "DEST must be a directory"));
    }
    let mut failed = false;
    for source in sources {
        let src = Path::new(source);
        match target_for(src, dst_path, dst_is_dir)
            .and_then(|target| copy_one(src, &target, options))
        {
            Ok(()) => {}
            Err(error) => {
                eprintln!("{program}: {error}");
                failed = true;
            }
        }
    }
    ExitCode::from(if failed { 1 } else { EXIT_OK })
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(std::env::args().next().as_deref(), "cp");
    let args: Vec<String> = std::env::args().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-cp-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn copies_a_file_and_preserves_mode() {
        let root = temp_root("file");
        let src = root.join("src");
        let dst = root.join("dst");
        fs::write(&src, b"payload").expect("src");
        fs::set_permissions(&src, fs::Permissions::from_mode(0o640)).expect("mode");
        let options = Options {
            preserve: true,
            ..Options::default()
        };
        copy_one(&src, &dst, options).expect("copy");
        assert_eq!(fs::read(&dst).expect("dst"), b"payload");
        assert_eq!(fs::metadata(&dst).expect("meta").mode() & 0o777, 0o640);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn copies_into_an_existing_directory_and_respects_no_clobber() {
        let root = temp_root("dir");
        let src = root.join("src");
        let dir = root.join("dir");
        fs::create_dir(&dir).expect("dir");
        fs::write(&src, b"new").expect("src");
        let target = target_for(&src, &dir, true).expect("target");
        copy_one(&src, &target, Options::default()).expect("copy");
        assert_eq!(fs::read(dir.join("src")).expect("copy"), b"new");
        fs::write(&src, b"changed").expect("src");
        let options = Options {
            no_clobber: true,
            ..Options::default()
        };
        copy_one(&src, &target, options).expect("skip");
        assert_eq!(fs::read(dir.join("src")).expect("copy"), b"new");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recursive_is_required_for_directories() {
        let root = temp_root("recursive");
        let src = root.join("tree");
        fs::create_dir_all(src.join("nested")).expect("tree");
        fs::write(src.join("nested/file"), b"x").expect("file");
        assert!(copy_one(&src, &root.join("copy"), Options::default()).is_err());
        let options = Options {
            recursive: true,
            ..Options::default()
        };
        copy_one(&src, &root.join("copy"), options).expect("copy");
        assert_eq!(fs::read(root.join("copy/nested/file")).expect("read"), b"x");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn run_reports_usage_and_missing_sources() {
        let root = temp_root("run");
        assert_eq!(run(&["--help".to_string()], "cp"), ExitCode::from(EXIT_OK));
        assert_eq!(
            run(&["--bad".to_string()], "cp"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        assert_eq!(run(&[], "cp"), ExitCode::from(rinit_tools::EXIT_USAGE));
        let missing = root.join("missing").display().to_string();
        let dst = root.join("dst").display().to_string();
        assert_eq!(run(&[missing, dst], "cp"), ExitCode::from(1));
        let _ = fs::remove_dir_all(root);
    }
}
