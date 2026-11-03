//! Path resolution shared by `realpath` and `readlink`.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

/// Resolve `path` to an absolute path.
///
/// With `allow_missing`, the longest existing prefix is canonicalized and the
/// remaining components are applied lexically. This is the `-m` behaviour; the
/// strict mode delegates to `canonicalize` and therefore requires every
/// component to exist.
pub fn resolve(path: &Path, allow_missing: bool) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if !allow_missing {
        return std::fs::canonicalize(&absolute);
    }
    if let Ok(resolved) = std::fs::canonicalize(&absolute) {
        return Ok(resolved);
    }
    let mut prefix = absolute.clone();
    let mut tail: Vec<OsString> = Vec::new();
    loop {
        if let Ok(base) = std::fs::canonicalize(&prefix) {
            let mut result = base;
            for component in tail.iter().rev() {
                if component == OsStr::new("..") {
                    result.pop();
                } else if component != OsStr::new(".") {
                    result.push(component);
                }
            }
            return Ok(result);
        }
        let Some(component) = prefix
            .components()
            .next_back()
            .map(|component| component.as_os_str().to_os_string())
        else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("cannot resolve '{}'", path.display()),
            ));
        };
        tail.push(component);
        if !prefix.pop() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("cannot resolve '{}'", path.display()),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-path-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn strict_resolution_requires_every_component() {
        let root = temp_root("strict");
        let file = root.join("file");
        std::fs::write(&file, b"x").expect("file");
        assert_eq!(
            resolve(&file, false).expect("resolve"),
            std::fs::canonicalize(&file).expect("canonical")
        );
        assert!(resolve(&root.join("missing"), false).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn permissive_resolution_handles_missing_tails_and_parents() {
        let root = temp_root("missing");
        let file = root.join("file");
        std::fs::write(&file, b"x").expect("file");
        assert_eq!(
            resolve(&root.join("missing"), true).expect("resolve"),
            std::fs::canonicalize(&root)
                .expect("canonical")
                .join("missing")
        );
        assert_eq!(
            resolve(&root.join("missing/../file"), true).expect("resolve"),
            std::fs::canonicalize(&file).expect("canonical")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn symlinks_are_resolved_when_the_target_exists() {
        let root = temp_root("symlink");
        let file = root.join("target");
        let link = root.join("link");
        std::fs::write(&file, b"x").expect("target");
        symlink(&file, &link).expect("link");
        assert_eq!(
            resolve(&link, false).expect("resolve"),
            std::fs::canonicalize(&file).expect("canonical")
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
