//! `mv` - move files and directory trees.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};

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

/// Move across filesystems without deleting the destination until the copy is
/// complete.
///
/// The copy goes to a sibling temporary name in the destination directory, then
/// a same-filesystem `rename` replaces the destination. If the copy fails, the
/// destination is untouched; if the final rename fails (for example a non-empty
/// directory target), the temporary tree is removed and the source remains.
fn move_across_filesystems(src: &Path, dst: &Path) -> Result<(), String> {
    static TEMPORARY: AtomicU64 = AtomicU64::new(0);
    let parent = dst
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // Keep the temporary name short: prefixing the destination name can
    // exceed NAME_MAX for a 255-byte source. It is still unique per process
    // and attempt.
    let temporary_name = OsString::from(format!(
        ".rinit-mv.{}.{}",
        std::process::id(),
        TEMPORARY.fetch_add(1, Ordering::Relaxed)
    ));
    let temporary = parent.join(temporary_name);

    let _ = fsutil::remove_entry(&temporary, true);
    if let Err(error) = fsutil::copy_entry(src, &temporary, true, true) {
        let _ = fsutil::remove_entry(&temporary, true);
        return Err(format!("{} -> {}: {error}", src.display(), dst.display()));
    }
    if let Err(error) = fs::rename(&temporary, dst) {
        let _ = fsutil::remove_entry(&temporary, true);
        return Err(format!("{} -> {}: {error}", src.display(), dst.display()));
    }
    fsutil::remove_entry(src, true).map_err(|error| format!("{}: {error}", src.display()))
}

fn move_one(src: &Path, dst: &Path, options: Options) -> Result<(), String> {
    if options.no_clobber && fs::symlink_metadata(dst).is_ok() {
        return Ok(());
    }
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
            move_across_filesystems(src, dst)
        }
        Err(error) => Err(format!("{} -> {}: {error}", src.display(), dst.display())),
    }
}

fn run<A: AsRef<OsStr>>(args: &[A], program: &str) -> ExitCode {
    let mut options = Options::default();
    let mut paths: Vec<PathBuf> = Vec::new();
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
                println!("Usage: mv [-fn] SOURCE... DEST");
                return ExitCode::from(EXIT_OK);
            }
            Some("-f" | "--force") => {}
            Some("-n" | "--no-clobber") => options.no_clobber = true,
            Some(value) if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            _ => paths.push(PathBuf::from(arg.as_ref())),
        }
    }
    paths.extend(trailing.iter().map(|arg| PathBuf::from(arg.as_ref())));
    let (dst, sources) = match paths.split_last() {
        Some((dst, sources)) if !sources.is_empty() => (dst, sources),
        _ => return ExitCode::from(usage_error(program, "SOURCE and DEST are required")),
    };
    let dst_path = dst.as_path();
    let dst_is_dir = fs::metadata(dst_path)
        .map(|meta| meta.is_dir())
        .unwrap_or(false);
    if sources.len() > 1 && !dst_is_dir {
        return ExitCode::from(usage_error(program, "DEST must be a directory"));
    }
    let mut failed = false;
    for source in sources {
        let src = source.as_path();
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
    let program = program_name(
        std::env::args_os()
            .next()
            .as_deref()
            .and_then(|arg| arg.to_str()),
        "mv",
    );
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
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

    /// The cross-device path must not delete a destination before the copy has
    /// succeeded. A non-empty directory is the easy way to make the final
    /// rename fail after the temporary copy.
    #[test]
    fn a_failed_cross_device_move_leaves_both_sides_intact() {
        let root = temp_root("cross-fail");
        let src = root.join("src");
        let dst = root.join("dst");
        fs::write(&src, b"source").expect("src");
        fs::create_dir(&dst).expect("dst");
        fs::write(dst.join("keep"), b"keep").expect("keep");

        assert!(move_across_filesystems(&src, &dst).is_err());
        assert_eq!(fs::read(&src).expect("src"), b"source");
        assert_eq!(fs::read(dst.join("keep")).expect("keep"), b"keep");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_cross_device_file_move_replaces_the_destination_atomically() {
        let root = temp_root("cross-ok");
        let src = root.join("src");
        let dst = root.join("dst");
        fs::write(&src, b"new").expect("src");
        fs::write(&dst, b"old").expect("dst");

        move_across_filesystems(&src, &dst).expect("move");
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).expect("dst"), b"new");
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
        assert_eq!(
            run(&[] as &[String], "mv"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        let missing = root.join("missing").display().to_string();
        let dst = root.join("dst").display().to_string();
        assert_eq!(run(&[missing, dst], "mv"), ExitCode::from(1));
        let _ = fs::remove_dir_all(root);
    }
}
