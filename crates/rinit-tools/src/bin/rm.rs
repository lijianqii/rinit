//! `rm` - remove files and directory trees.

use std::fs;
use std::path::{Component, Path};
use std::process::ExitCode;

use rinit_tools::fsutil;
use rinit_tools::{EXIT_OK, program_name, usage_error};

#[derive(Clone, Copy, Default)]
struct Options {
    recursive: bool,
    force: bool,
    dir: bool,
}

fn protected(path: &str) -> bool {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return true;
    }
    if let Some(last) = trimmed.rsplit('/').next()
        && (last == "." || last == "..")
    {
        return true;
    }
    let path = Path::new(trimmed);
    if let Some(Component::CurDir | Component::ParentDir | Component::RootDir) =
        path.components().next_back()
    {
        return true;
    }
    fs::canonicalize(path)
        .map(|resolved| resolved == Path::new("/"))
        .unwrap_or(false)
}

fn remove_path(path: &Path, options: Options) -> Result<(), String> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && options.force => {
            return Ok(());
        }
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    if meta.is_dir() {
        if options.recursive {
            fsutil::remove_entry(path, true).map_err(|error| format!("{}: {error}", path.display()))
        } else if options.dir {
            fs::remove_dir(path).map_err(|error| format!("{}: {error}", path.display()))
        } else {
            Err(format!("{}: is a directory", path.display()))
        }
    } else {
        fs::remove_file(path).map_err(|error| format!("{}: {error}", path.display()))
    }
}

fn run(args: &[String], program: &str) -> ExitCode {
    let mut options = Options::default();
    let mut paths: Vec<String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--help" => {
                println!("Usage: rm [-rRfd] FILE...");
                return ExitCode::from(EXIT_OK);
            }
            "-r" | "-R" | "--recursive" => options.recursive = true,
            "-f" | "--force" => options.force = true,
            "-d" | "--dir" => options.dir = true,
            value if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            value => paths.push(value.to_string()),
        }
    }
    if paths.is_empty() {
        return ExitCode::from(usage_error(program, "a file operand is required"));
    }
    let mut failed = false;
    for display in paths {
        if protected(&display) {
            eprintln!("{program}: refusing to remove '{display}'");
            failed = true;
            continue;
        }
        if let Err(error) = remove_path(Path::new(&display), options) {
            eprintln!("{program}: {error}");
            failed = true;
        }
    }
    ExitCode::from(if failed { 1 } else { EXIT_OK })
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(std::env::args().next().as_deref(), "rm");
    let args: Vec<String> = std::env::args().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-rm-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn removes_files_and_whole_trees() {
        let root = temp_root("tree");
        let file = root.join("file");
        fs::write(&file, b"x").expect("file");
        remove_path(&file, Options::default()).expect("remove file");
        assert!(!file.exists());
        let tree = root.join("tree");
        fs::create_dir_all(tree.join("nested")).expect("tree");
        fs::write(tree.join("nested/file"), b"x").expect("file");
        remove_path(&tree, Options::default()).expect_err("directory needs -r");
        let options = Options {
            recursive: true,
            ..Options::default()
        };
        remove_path(&tree, options).expect("remove tree");
        assert!(!tree.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn force_ignores_missing_paths() {
        let root = temp_root("force");
        let missing = root.join("missing");
        assert!(remove_path(&missing, Options::default()).is_err());
        let options = Options {
            force: true,
            ..Options::default()
        };
        remove_path(&missing, options).expect("force");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn dir_flag_removes_only_empty_directories() {
        let root = temp_root("dir");
        let empty = root.join("empty");
        fs::create_dir(&empty).expect("empty");
        let options = Options {
            dir: true,
            ..Options::default()
        };
        remove_path(&empty, options).expect("remove empty");
        assert!(!empty.exists());
        let full = root.join("full");
        fs::create_dir(&full).expect("full");
        fs::write(full.join("file"), b"x").expect("file");
        assert!(remove_path(&full, options).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn symlink_targets_survive() {
        let root = temp_root("symlink");
        let target = root.join("target");
        fs::write(&target, b"x").expect("target");
        let link = root.join("link");
        std::os::unix::fs::symlink(&target, &link).expect("link");
        remove_path(&link, Options::default()).expect("remove link");
        assert!(!link.exists());
        assert!(target.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn protects_parent_and_current_directory_components() {
        for path in [
            "/", "/.", "//.", "/usr/..", "/tmp/..", "foo/..", "foo/.", "..", ".", "",
        ] {
            assert!(protected(path), "{path} must be protected");
        }
        assert!(!protected("./build"));
        assert!(!protected("/tmp/build"));
    }

    #[test]
    fn run_refuses_protected_paths_and_bad_options() {
        assert_eq!(run(&["/".to_string()], "rm"), ExitCode::from(1));
        assert_eq!(run(&["..".to_string()], "rm"), ExitCode::from(1));
        assert_eq!(run(&["/usr/..".to_string()], "rm"), ExitCode::from(1));
        assert_eq!(run(&["/.".to_string()], "rm"), ExitCode::from(1));
        assert_eq!(run(&["--help".to_string()], "rm"), ExitCode::from(EXIT_OK));
        assert_eq!(
            run(&["--bad".to_string()], "rm"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        assert_eq!(run(&[], "rm"), ExitCode::from(rinit_tools::EXIT_USAGE));
    }
}
