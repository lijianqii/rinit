//! `cp` - copy files and directory trees.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
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
        // `cp -a src/. dest` copies the *contents* of src into dest, and
        // `cp -a . dest` copies the current directory's contents. Path's
        // `file_name()` returns `src` and `None` respectively; both must map to
        // dest itself rather than dest/src or an error.
        let bytes = src.as_os_str().as_bytes();
        if bytes == b"." || bytes.ends_with(b"/.") {
            return Ok(dst.to_path_buf());
        }
        let name = src
            .file_name()
            .ok_or_else(|| format!("cannot determine the name of '{}'", src.display()))?;
        Ok(dst.join(name))
    } else {
        Ok(dst.to_path_buf())
    }
}

fn canonical_destination(dst: &Path) -> Result<PathBuf, String> {
    match fs::canonicalize(dst) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = dst
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let name = dst
                .file_name()
                .ok_or_else(|| format!("cannot determine the name of '{}'", dst.display()))?;
            let parent = fs::canonicalize(parent)
                .map_err(|error| format!("{}: {error}", parent.display()))?;
            Ok(parent.join(name))
        }
        Err(error) => Err(format!("{}: {error}", dst.display())),
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
    if meta.is_dir() {
        let source =
            fs::canonicalize(src).map_err(|error| format!("{}: {error}", src.display()))?;
        let destination = canonical_destination(dst)?;
        if destination.starts_with(&source) {
            return Err(format!(
                "{}: cannot copy a directory into itself ({})",
                src.display(),
                dst.display()
            ));
        }
    }
    if let Ok(existing) = fs::symlink_metadata(dst)
        && !existing.is_dir()
        && existing.dev() == meta.dev()
        && existing.ino() == meta.ino()
    {
        return Err(format!(
            "{} and {} are the same file",
            src.display(),
            dst.display()
        ));
    }
    if options.force && (meta.is_file() || meta.file_type().is_symlink()) {
        match fs::symlink_metadata(dst) {
            Ok(existing) if !existing.is_dir() => {
                // Do not destroy the destination until the source has been
                // proven readable. Otherwise `cp -f missing dest` loses dest.
                if meta.file_type().is_symlink() {
                    fs::read_link(src).map_err(|error| format!("{}: {error}", src.display()))?;
                } else if meta.is_file() {
                    fs::File::open(src).map_err(|error| format!("{}: {error}", src.display()))?;
                }
                fs::remove_file(dst).map_err(|error| format!("{}: {error}", dst.display()))?;
            }
            _ => {}
        }
    }
    fsutil::copy_entry(src, dst, options.recursive, options.preserve)
        .map_err(|error| format!("{} -> {}: {error}", src.display(), dst.display()))
}

fn apply_short_options(value: &str, options: &mut Options) -> Result<(), String> {
    for ch in value[1..].chars() {
        match ch {
            'r' | 'R' => options.recursive = true,
            'a' => {
                options.recursive = true;
                options.preserve = true;
            }
            'f' => options.force = true,
            'p' => options.preserve = true,
            'n' => options.no_clobber = true,
            other => return Err(format!("unknown option '-{other}'")),
        }
    }
    Ok(())
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
                println!("Usage: cp [-rRafpn] SOURCE... DEST");
                return ExitCode::from(EXIT_OK);
            }
            Some("-r" | "-R" | "--recursive") => options.recursive = true,
            Some("-a" | "--archive") => {
                options.recursive = true;
                options.preserve = true;
            }
            Some("-p" | "--preserve") => options.preserve = true,
            Some("-f" | "--force") => options.force = true,
            Some("-n" | "--no-clobber") => options.no_clobber = true,
            Some(value)
                if value.starts_with('-') && value.len() > 1 && !value.starts_with("--") =>
            {
                if let Err(error) = apply_short_options(value, &mut options) {
                    return ExitCode::from(usage_error(program, &error));
                }
            }
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
    let program = program_name(
        std::env::args_os()
            .next()
            .as_deref()
            .and_then(|arg| arg.to_str()),
        "cp",
    );
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
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
    fn clustered_short_options_are_parsed() {
        let mut options = Options::default();
        apply_short_options("-rf", &mut options).expect("parse");
        assert!(options.recursive);
        assert!(options.force);
        apply_short_options("-Rp", &mut options).expect("parse");
        assert!(options.preserve);
        assert!(apply_short_options("-x", &mut options).is_err());
    }

    #[test]
    fn double_dash_allows_dash_leading_operands() {
        let root = temp_root("double-dash");
        let dest = root.join("dest");
        fs::create_dir(&dest).expect("dest");
        let dash = root.join("-r");
        let good = root.join("good");
        fs::write(&dash, b"dash").expect("dash");
        fs::write(&good, b"good").expect("good");
        let args = vec![
            "--".to_string(),
            dash.display().to_string(),
            good.display().to_string(),
            dest.display().to_string(),
        ];
        assert_eq!(run(&args, "cp"), ExitCode::from(EXIT_OK));
        assert_eq!(fs::read(dest.join("-r")).expect("dash copy"), b"dash");
        assert_eq!(fs::read(dest.join("good")).expect("good copy"), b"good");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_failed_source_does_not_stop_the_next_one() {
        let root = temp_root("partial-failure");
        let dest = root.join("dest");
        fs::create_dir(&dest).expect("dest");
        let good = root.join("good");
        fs::write(&good, b"good").expect("good");
        let args = vec![
            root.join("missing").display().to_string(),
            good.display().to_string(),
            dest.display().to_string(),
        ];
        assert_eq!(run(&args, "cp"), ExitCode::from(1));
        assert_eq!(fs::read(dest.join("good")).expect("good copy"), b"good");
        let _ = fs::remove_dir_all(root);
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
    fn force_does_not_destroy_the_destination_when_the_source_cannot_be_read() {
        let root = temp_root("force-missing");
        let missing = root.join("missing");
        let dst = root.join("dst");
        fs::write(&dst, b"keep me").expect("dst");
        let options = Options {
            force: true,
            ..Options::default()
        };
        assert!(copy_one(&missing, &dst, options).is_err());
        assert_eq!(fs::read(&dst).expect("dst survives"), b"keep me");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn copying_a_file_onto_itself_is_refused_without_truncating_it() {
        let root = temp_root("same-file");
        let path = root.join("file");
        fs::write(&path, b"payload").expect("file");
        assert!(copy_one(&path, &path, Options::default()).is_err());
        assert_eq!(fs::read(&path).expect("source survives"), b"payload");

        let options = Options {
            force: true,
            ..Options::default()
        };
        assert!(copy_one(&path, &path, options).is_err());
        assert_eq!(fs::read(&path).expect("source survives -f"), b"payload");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn copying_a_hard_link_onto_its_source_is_refused() {
        let root = temp_root("same-inode");
        let src = root.join("src");
        let link = root.join("link");
        fs::write(&src, b"payload").expect("src");
        fs::hard_link(&src, &link).expect("hard link");
        assert!(copy_one(&src, &link, Options::default()).is_err());
        assert_eq!(fs::read(&src).expect("source survives"), b"payload");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recursive_copy_of_a_read_only_directory_works() {
        let root = temp_root("read-only");
        let src = root.join("tree");
        fs::create_dir(&src).expect("tree");
        fs::write(src.join("file"), b"payload").expect("file");
        fs::set_permissions(&src, fs::Permissions::from_mode(0o555)).expect("mode");
        let options = Options {
            recursive: true,
            ..Options::default()
        };
        copy_one(&src, &root.join("copy"), options).expect("copy");
        assert_eq!(
            fs::read(root.join("copy/file")).expect("copied file"),
            b"payload"
        );
        let _ = fs::set_permissions(&src, fs::Permissions::from_mode(0o755));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_trailing_dot_copies_directory_contents() {
        let root = temp_root("trailing-dot");
        let src = root.join("src");
        let dest = root.join("dest");
        fs::create_dir_all(&src).expect("src");
        fs::create_dir(&dest).expect("dest");
        fs::write(src.join("file"), b"payload").expect("file");
        let target = target_for(&src.join("."), &dest, true).expect("target");
        assert_eq!(target, dest);
        let options = Options {
            recursive: true,
            preserve: true,
            ..Options::default()
        };
        copy_one(&src.join("."), &target, options).expect("copy");
        assert_eq!(fs::read(dest.join("file")).expect("file"), b"payload");
        assert!(!dest.join("src").exists());
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
    fn recursive_copy_recreates_symlinks_without_following_them() {
        let root = temp_root("symlink-copy");
        let src = root.join("tree");
        let target = root.join("outside");
        fs::create_dir_all(&src).expect("tree");
        fs::write(&target, b"outside").expect("target");
        std::os::unix::fs::symlink(&target, src.join("link")).expect("link");
        let options = Options {
            recursive: true,
            ..Options::default()
        };
        copy_one(&src, &root.join("copy"), options).expect("copy");
        let copied = root.join("copy/link");
        assert_eq!(fs::read_link(&copied).expect("readlink"), target);
        assert_eq!(fs::read(&target).expect("target"), b"outside");
        assert!(!root.join("copy/outside").exists());
        let _ = fs::remove_dir_all(root);
    }

    /// Copying a directory into its own subtree must fail before the first
    /// mkdir, or the directory walk sees the new child and recurses forever.
    #[test]
    fn copying_a_directory_into_itself_is_refused() {
        let root = temp_root("self");
        let src = root.join("tree");
        fs::create_dir_all(&src).expect("tree");
        fs::write(src.join("file"), b"x").expect("file");
        let options = Options {
            recursive: true,
            ..Options::default()
        };

        for target in [src.join("sub"), src.join("sub/deeper")] {
            assert!(
                copy_one(&src, &target, options).is_err(),
                "{} must be refused",
                target.display()
            );
            assert!(!target.exists(), "the refused target must not be created");
        }
        let _ = fs::remove_dir_all(root);
    }

    /// `cp` must copy a source whose name is not valid UTF-8.
    #[test]
    fn a_non_utf8_source_copies() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let root = temp_root("nonutf8");
        let name = OsString::from_vec(vec![b's', 0xff, b'r', b'c']);
        let src = root.join(&name);
        let dst = root.join("copy");
        fs::write(&src, b"bytes").expect("source");
        assert_eq!(
            run(&[src.clone(), dst.clone()], "cp"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(fs::read(&dst).expect("copy"), b"bytes");
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
        assert_eq!(
            run(&[] as &[String], "cp"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        let missing = root.join("missing").display().to_string();
        let dst = root.join("dst").display().to_string();
        assert_eq!(run(&[missing, dst], "cp"), ExitCode::from(1));
        let _ = fs::remove_dir_all(root);
    }
}
