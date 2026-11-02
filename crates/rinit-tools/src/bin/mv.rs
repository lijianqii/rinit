//! `mv` - move files and directory trees.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rinit_tools::fsutil;
use rinit_tools::{EXIT_OK, program_name, usage_error};

#[derive(Clone, Copy, Default)]
struct Options {
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

fn move_one(src: &Path, dst: &Path, options: Options) -> Result<(), String> {
    if options.no_clobber && fs::symlink_metadata(dst).is_ok() {
        return Ok(());
    }
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
            if fs::symlink_metadata(dst).is_ok() {
                fsutil::remove_entry(dst, true)
                    .map_err(|error| format!("{}: {error}", dst.display()))?;
            }
            fsutil::copy_entry(src, dst, true, true)
                .map_err(|error| format!("{} -> {}: {error}", src.display(), dst.display()))?;
            fsutil::remove_entry(src, true).map_err(|error| format!("{}: {error}", src.display()))
        }
        Err(error) => Err(format!("{} -> {}: {error}", src.display(), dst.display())),
    }
}

fn run(args: &[String], program: &str) -> ExitCode {
    let mut options = Options::default();
    let mut paths: Vec<String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--help" => {
                println!("Usage: mv [-fn] SOURCE... DEST");
                return ExitCode::from(EXIT_OK);
            }
            "-f" | "--force" => {}
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
            .and_then(|target| move_one(src, &target, options))
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
    let program = program_name(std::env::args().next().as_deref(), "mv");
    let args: Vec<String> = std::env::args().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-mv-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn renames_a_file_and_moves_it_into_a_directory() {
        let root = temp_root("file");
        let src = root.join("src");
        let dst = root.join("dst");
        fs::write(&src, b"payload").expect("src");
        move_one(&src, &dst, Options::default()).expect("rename");
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).expect("dst"), b"payload");

        let dir = root.join("dir");
        fs::create_dir(&dir).expect("dir");
        let target = target_for(&dst, &dir, true).expect("target");
        move_one(&dst, &target, Options::default()).expect("move");
        assert!(dir.join("dst").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn no_clobber_keeps_the_existing_file() {
        let root = temp_root("noclobber");
        let src = root.join("src");
        let dst = root.join("dst");
        fs::write(&src, b"new").expect("src");
        fs::write(&dst, b"old").expect("dst");
        let options = Options { no_clobber: true };
        move_one(&src, &dst, options).expect("skip");
        assert!(src.exists());
        assert_eq!(fs::read(&dst).expect("dst"), b"old");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn run_reports_usage_and_missing_sources() {
        let root = temp_root("run");
        assert_eq!(run(&["--help".to_string()], "mv"), ExitCode::from(EXIT_OK));
        assert_eq!(
            run(&["--bad".to_string()], "mv"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        assert_eq!(run(&[], "mv"), ExitCode::from(rinit_tools::EXIT_USAGE));
        let missing = root.join("missing").display().to_string();
        let dst = root.join("dst").display().to_string();
        assert_eq!(run(&[missing, dst], "mv"), ExitCode::from(1));
        let _ = fs::remove_dir_all(root);
    }
}
