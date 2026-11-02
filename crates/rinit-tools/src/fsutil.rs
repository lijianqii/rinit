//! Filesystem operations shared by the file-oriented tools.
//!
//! The helpers deliberately use `symlink_metadata` so a recursive operation
//! never follows a symlink out of the tree it was asked to touch. Ownership is
//! not copied: the tools run as root on the device image, and a test build does
//! not have the privileges to reproduce an arbitrary owner.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

/// Copy one filesystem entry.
///
/// Directories require `recursive`; symlinks are recreated rather than
/// followed. `preserve` restores mode and timestamps after the data copy.
pub fn copy_entry(src: &Path, dst: &Path, recursive: bool, preserve: bool) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    let file_type = meta.file_type();

    if file_type.is_symlink() {
        let target = fs::read_link(src)?;
        remove_existing_non_dir(dst)?;
        std::os::unix::fs::symlink(target, dst)
    } else if file_type.is_dir() {
        if !recursive {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "omitting directory",
            ));
        }
        match fs::symlink_metadata(dst) {
            Ok(existing) if !existing.is_dir() => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "destination is not a directory",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(dst)?;
            }
            Err(error) => return Err(error),
        }
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_entry(&entry.path(), &dst.join(entry.file_name()), true, preserve)?;
        }
        if preserve {
            apply_metadata(dst, &meta)?;
        }
        Ok(())
    } else if file_type.is_file() {
        fs::copy(src, dst)?;
        if preserve {
            apply_metadata(dst, &meta)?;
        }
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported file type",
        ))
    }
}

/// Remove one entry, descending into directories only when `recursive`.
pub fn remove_entry(path: &Path, recursive: bool) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        if !recursive {
            return Err(io::Error::other("is a directory"));
        }
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            remove_entry(&entry.path(), true)?;
        }
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

/// Restore mode and atime/mtime from `meta` onto `path`.
pub fn apply_metadata(path: &Path, meta: &fs::Metadata) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(meta.mode() & 0o7777))?;
    set_times(
        path,
        meta.atime(),
        meta.atime_nsec(),
        meta.mtime(),
        meta.mtime_nsec(),
    )
}

/// Set atime and mtime with nanosecond precision.
pub fn set_times(
    path: &Path,
    atime: i64,
    atime_nsec: i64,
    mtime: i64,
    mtime_nsec: i64,
) -> io::Result<()> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let times = [
        libc::timespec {
            tv_sec: atime as libc::time_t,
            tv_nsec: atime_nsec as libc::c_long,
        },
        libc::timespec {
            tv_sec: mtime as libc::time_t,
            tv_nsec: mtime_nsec as libc::c_long,
        },
    ];
    // SAFETY: `c_path` is NUL-terminated and `times` has the two entries the
    // syscall expects; both live for the duration of the call.
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn remove_existing_non_dir(dst: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dst) {
        Ok(existing) if existing.is_dir() => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "destination is a directory",
        )),
        Ok(_) => fs::remove_file(dst),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-fsutil-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("temp root");
        root
    }

    #[test]
    fn copies_files_and_preserves_mode() {
        let root = temp_root("copy-file");
        let src = root.join("src");
        let dst = root.join("dst");
        fs::write(&src, b"payload").expect("write");
        fs::set_permissions(&src, fs::Permissions::from_mode(0o640)).expect("mode");
        copy_entry(&src, &dst, false, true).expect("copy");
        assert_eq!(fs::read(&dst).expect("read"), b"payload");
        assert_eq!(fs::metadata(&dst).expect("meta").mode() & 0o777, 0o640);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn copies_directories_recursively_and_recreates_symlinks() {
        let root = temp_root("copy-dir");
        let src = root.join("tree");
        let dst = root.join("copy");
        fs::create_dir_all(src.join("nested")).expect("dirs");
        fs::write(src.join("nested/file"), b"x").expect("file");
        symlink("nested/file", src.join("link")).expect("symlink");
        copy_entry(&src, &dst, true, false).expect("copy tree");
        assert_eq!(fs::read(dst.join("nested/file")).expect("read"), b"x");
        assert!(
            fs::symlink_metadata(dst.join("link"))
                .expect("link meta")
                .file_type()
                .is_symlink()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn refuses_a_directory_without_recursive() {
        let root = temp_root("no-recursive");
        let src = root.join("dir");
        fs::create_dir(&src).expect("dir");
        assert!(copy_entry(&src, &root.join("dst"), false, false).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn removes_trees_but_not_symlink_targets() {
        let root = temp_root("remove");
        let tree = root.join("tree");
        fs::create_dir_all(tree.join("nested")).expect("dirs");
        fs::write(tree.join("nested/file"), b"x").expect("file");
        let outside = root.join("outside");
        fs::write(&outside, b"keep").expect("outside");
        symlink(&outside, tree.join("link")).expect("symlink");
        remove_entry(&tree, true).expect("remove");
        assert!(!tree.exists());
        assert_eq!(fs::read(&outside).expect("outside remains"), b"keep");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn set_times_changes_the_recorded_timestamps() {
        let root = temp_root("times");
        let path = root.join("stamp");
        fs::write(&path, b"x").expect("write");
        set_times(&path, 1_000_000_000, 0, 1_000_000_100, 0).expect("set");
        let meta = fs::metadata(&path).expect("meta");
        assert_eq!(meta.atime(), 1_000_000_000);
        assert_eq!(meta.mtime(), 1_000_000_100);
        let _ = fs::remove_dir_all(root);
    }
}
