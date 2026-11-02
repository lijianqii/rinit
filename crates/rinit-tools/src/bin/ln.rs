//! `ln` - create hard and symbolic links.

use std::fs;
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
    if options.force && fs::symlink_metadata(link).is_ok() {
        fs::remove_file(link).map_err(|error| format!("{}: {error}", link.display()))?;
    }
    let result = if options.symbolic {
        std::os::unix::fs::symlink(target, link)
    } else {
        fs::hard_link(target, link)
    };
    result.map_err(|error| format!("{} -> {}: {error}", link.display(), target.display()))
}

fn run(args: &[String], program: &str) -> ExitCode {
    let mut options = Options::default();
    let mut paths: Vec<String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--help" => {
                println!("Usage: ln [-sfn] TARGET... LINK");
                return ExitCode::from(EXIT_OK);
            }
            "-s" | "--symbolic" => options.symbolic = true,
            "-f" | "--force" => options.force = true,
            "-n" | "--no-dereference" => options.no_dereference = true,
            value if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            value => paths.push(value.to_string()),
        }
    }
    let (dst, targets) = match paths.split_last() {
        Some((dst, targets)) if !targets.is_empty() => (dst, targets),
        _ => return ExitCode::from(usage_error(program, "TARGET and LINK are required")),
    };
    let dst_path = Path::new(dst);
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
        let target_path = Path::new(target);
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
    let program = program_name(std::env::args().next().as_deref(), "ln");
    let args: Vec<String> = std::env::args().skip(1).collect();
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
        assert_eq!(run(&[], "ln"), ExitCode::from(rinit_tools::EXIT_USAGE));
    }
}
