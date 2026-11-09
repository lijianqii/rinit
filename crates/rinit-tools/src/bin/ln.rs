//! `ln` - create hard and symbolic links.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, usage_error};

#[derive(Clone, Copy, Default)]
struct Options {
    symbolic: bool,
    force: bool,
    no_dereference: bool,
}

fn destination_for(target: &Path, dst: &Path, options: Options) -> Result<PathBuf, String> {
    let dst_is_dir = if options.no_dereference {
        fs::symlink_metadata(dst)
            .map(|meta| meta.is_dir())
            .unwrap_or(false)
    } else {
        fs::metadata(dst).map(|meta| meta.is_dir()).unwrap_or(false)
    };
    if dst_is_dir {
        let name = target
            .file_name()
            .ok_or_else(|| format!("cannot determine the name of '{}'", target.display()))?;
        Ok(dst.join(name))
    } else {
        Ok(dst.to_path_buf())
    }
}

fn link_one(target: &Path, link: &Path, options: Options) -> Result<(), String> {
    let create = || {
        if options.symbolic {
            std::os::unix::fs::symlink(target, link)
        } else {
            fs::hard_link(target, link)
        }
    };
    match create() {
        Ok(()) => Ok(()),
        Err(error) if options.force && error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Try to create first: a missing/invalid target must not delete an
            // existing link. Only replace it after the failure is specifically
            // "the link already exists".
            if let Ok(existing) = fs::symlink_metadata(link) {
                if existing.is_dir() {
                    return Err(format!("{}: is a directory", link.display()));
                }
                if let Ok(target_meta) = fs::symlink_metadata(target)
                    && target_meta.dev() == existing.dev()
                    && target_meta.ino() == existing.ino()
                {
                    return Err(format!(
                        "{} and {} are the same file",
                        target.display(),
                        link.display()
                    ));
                }
            }
            fs::remove_file(link).map_err(|error| format!("{}: {error}", link.display()))?;
            create().map_err(|error| format!("{} -> {}: {error}", link.display(), target.display()))
        }
        Err(error) => Err(format!(
            "{} -> {}: {error}",
            link.display(),
            target.display()
        )),
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
                println!("Usage: ln [-sfn] TARGET... LINK");
                return ExitCode::from(EXIT_OK);
            }
            Some("-s" | "--symbolic") => options.symbolic = true,
            Some("-f" | "--force") => options.force = true,
            Some("-n" | "--no-dereference") => options.no_dereference = true,
            Some(value) if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            _ => paths.push(PathBuf::from(arg.as_ref())),
        }
    }
    paths.extend(trailing.iter().map(|arg| PathBuf::from(arg.as_ref())));
    let (dst, targets) = match paths.split_last() {
        Some((dst, targets)) if !targets.is_empty() => (dst, targets),
        _ => return ExitCode::from(usage_error(program, "TARGET and LINK are required")),
    };
    let dst_path = dst.as_path();
    if targets.len() > 1 {
        let is_dir = fs::metadata(dst_path)
            .map(|meta| meta.is_dir())
            .unwrap_or(false);
        if !is_dir {
            return ExitCode::from(usage_error(program, "LINK must be a directory"));
        }
    }
    let mut failed = false;
    for target in targets {
        let target_path = target.as_path();
        match destination_for(target_path, dst_path, options)
            .and_then(|link| link_one(target_path, &link, options))
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
        "ln",
    );
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-ln-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn creates_hard_links_and_symlinks() {
        let root = temp_root("links");
        let target = root.join("target");
        fs::write(&target, b"x").expect("target");
        let hard = root.join("hard");
        link_one(&target, &hard, Options::default()).expect("hard");
        assert_eq!(
            fs::metadata(&hard).expect("hard").ino(),
            fs::metadata(&target).expect("target").ino()
        );
        let soft = root.join("soft");
        let options = Options {
            symbolic: true,
            ..Options::default()
        };
        link_one(&target, &soft, options).expect("soft");
        assert_eq!(fs::read_link(&soft).expect("readlink"), target);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn force_refuses_to_replace_the_source_with_itself() {
        let root = temp_root("same");
        let path = root.join("file");
        fs::write(&path, b"payload").expect("file");
        let options = Options {
            force: true,
            ..Options::default()
        };
        assert!(link_one(&path, &path, options).is_err());
        assert_eq!(fs::read(&path).expect("source survives"), b"payload");

        let symbolic = Options {
            symbolic: true,
            force: true,
            ..Options::default()
        };
        assert!(link_one(&path, &path, symbolic).is_err());
        assert_eq!(fs::read(&path).expect("source survives -s"), b"payload");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn force_replaces_an_existing_link() {
        let root = temp_root("force");
        let target = root.join("target");
        let link = root.join("link");
        fs::write(&target, b"target").expect("target");
        fs::write(&link, b"old").expect("link");
        let options = Options {
            force: true,
            ..Options::default()
        };
        link_one(&target, &link, options).expect("replace");
        assert_eq!(fs::read(&link).expect("link"), b"target");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn force_does_not_destroy_the_destination_when_the_target_is_missing() {
        let root = temp_root("force-missing");
        let missing = root.join("missing");
        let link = root.join("link");
        fs::write(&link, b"keep me").expect("link");
        let options = Options {
            force: true,
            ..Options::default()
        };
        assert!(link_one(&missing, &link, options).is_err());
        assert_eq!(fs::read(&link).expect("link survives"), b"keep me");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn multiple_targets_go_into_a_directory() {
        let root = temp_root("dir");
        let dir = root.join("dir");
        fs::create_dir(&dir).expect("dir");
        let first = root.join("first");
        let second = root.join("second");
        fs::write(&first, b"1").expect("first");
        fs::write(&second, b"2").expect("second");
        let mut failed = false;
        for target in [&first, &second] {
            let link = destination_for(target, &dir, Options::default()).expect("link");
            if link_one(target, &link, Options::default()).is_err() {
                failed = true;
            }
        }
        assert!(!failed);
        assert!(dir.join("first").exists());
        assert!(dir.join("second").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn run_reports_usage_and_bad_options() {
        assert_eq!(run(&["--help".to_string()], "ln"), ExitCode::from(EXIT_OK));
        assert_eq!(
            run(&["--bad".to_string()], "ln"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        assert_eq!(
            run(&[] as &[String], "ln"),
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
            std::env::temp_dir().join(format!("rinit-{}-{}-{tag}", "ln", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn run_links_and_reports_usage_errors() {
        let root = root("run");
        let target = root.join("target");
        fs::write(&target, b"x").expect("target");
        let link = root.join("link");
        assert_eq!(
            run(
                &[
                    "-s".to_string(),
                    target.display().to_string(),
                    link.display().to_string()
                ],
                "ln"
            ),
            ExitCode::from(EXIT_OK)
        );
        assert!(link.exists());
        assert_eq!(
            run(
                &[
                    "-s".to_string(),
                    "-f".to_string(),
                    target.display().to_string(),
                    link.display().to_string()
                ],
                "ln"
            ),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(run(&["--help".to_string()], "ln"), ExitCode::from(EXIT_OK));
        assert_eq!(
            run(&["--bad".to_string()], "ln"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        assert_eq!(
            run(&[] as &[String], "ln"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
