//! `rm` - remove files and directory trees.

use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

use rinit_tools::fsutil;
use rinit_tools::{EXIT_OK, program_name, usage_error};

#[derive(Clone, Copy, Default)]
struct Options {
    recursive: bool,
    force: bool,
    dir: bool,
}

fn protected(path: impl AsRef<Path>) -> bool {
    let path = path.as_ref();
    let trimmed = path.as_os_str().to_string_lossy();
    let trimmed = trimmed.trim_end_matches('/');
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
                println!("Usage: rm [-rRfd] FILE...");
                return ExitCode::from(EXIT_OK);
            }
            Some("-r" | "-R" | "--recursive") => options.recursive = true,
            Some("-f" | "--force") => options.force = true,
            Some("-d" | "--dir") => options.dir = true,
            Some(value) if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            _ => paths.push(PathBuf::from(arg.as_ref())),
        }
    }
    paths.extend(trailing.iter().map(|arg| PathBuf::from(arg.as_ref())));
    if paths.is_empty() {
        return ExitCode::from(usage_error(program, "a file operand is required"));
    }
    let mut failed = false;
    for path in paths {
        if protected(&path) {
            eprintln!("{program}: refusing to remove '{}'", path.display());
            failed = true;
            continue;
        }
        if let Err(error) = remove_path(&path, options) {
            eprintln!("{program}: {error}");
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
        "rm",
    );
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
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
    fn a_double_dash_allows_a_file_named_like_an_option() {
        let root = temp_root("dashdash");
        let file = root.join("-file");
        fs::write(&file, b"x").expect("file");
        assert_eq!(
            run(&["--".to_string(), file.display().to_string()], "rm"),
            ExitCode::from(EXIT_OK)
        );
        assert!(!file.exists());
        let _ = fs::remove_dir_all(root);
    }

    /// A path is bytes on Linux. The generic argument parser must treat a
    /// non-UTF-8 operand as a path instead of rejecting or mangling it.
    #[test]
    fn a_non_utf8_path_is_removed() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let root = temp_root("nonutf8");
        let name = OsString::from_vec(vec![b'f', 0xff, b'x']);
        let path = root.join(&name);
        fs::write(&path, b"x").expect("file");
        assert_eq!(
            run(std::slice::from_ref(&path), "rm"),
            ExitCode::from(EXIT_OK)
        );
        assert!(!path.exists());
        let _ = fs::remove_dir_all(root);
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
        assert_eq!(
            run(&[] as &[String], "rm"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
    }
}
