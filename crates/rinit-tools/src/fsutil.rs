//! Filesystem operations shared by the file-oriented tools.
//!
//! The helpers deliberately use `symlink_metadata` so a recursive operation
//! never follows a symlink out of the tree it was asked to touch. Ownership is
//! not copied: the tools run as root on the device image, and a test build does
//! not have the privileges to reproduce an arbitrary owner.

use std::ffi::{CStr, CString};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
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
        copy_directory(src, dst, preserve)
    } else if file_type.is_file() {
        copy_file(src, dst, preserve)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported file type",
        ))
    }
}

/// Open a directory without following a final symlink.
fn open_directory(path: &Path) -> io::Result<fs::File> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: the path is NUL-terminated and the flags describe a directory
    // open with O_NOFOLLOW.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: open returned a fresh descriptor we own.
        Ok(unsafe { fs::File::from_raw_fd(fd) })
    }
}

fn open_parent(path: &Path) -> io::Result<(fs::File, CString)> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination has no file name")
    })?;
    let parent = open_directory_follow(parent)?;
    let name = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    Ok((parent, name))
}

/// Open a directory for use as a parent dirfd, following a final symlink when
/// the caller supplied one. The descriptor is what keeps the parent stable
/// while the child name is resolved with `openat`/`mkdirat`.
fn open_directory_follow(path: &Path) -> io::Result<fs::File> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: the path is NUL-terminated and the flags describe a directory
    // open that may follow the final component.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: open returned a fresh descriptor we own.
        Ok(unsafe { fs::File::from_raw_fd(fd) })
    }
}

fn copy_file(src: &Path, dst: &Path, preserve: bool) -> io::Result<()> {
    let mut source = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(src)?;
    let source_meta = source.metadata()?;
    let (parent, name) = open_parent(dst)?;
    // SAFETY: the parent is a directory descriptor, `name` is NUL-terminated,
    // and the flags create the file without following a final symlink.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            source_meta.mode() & 0o7777,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a fresh descriptor we own.
    let mut destination = unsafe { fs::File::from_raw_fd(fd) };
    let destination_meta = destination.metadata()?;
    if source_meta.dev() == destination_meta.dev() && source_meta.ino() == destination_meta.ino() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source and destination are the same file",
        ));
    }
    // Truncate only after the same-inode check: `cp a a` must not empty the
    // source before the copy has read a single byte.
    destination.set_len(0)?;
    io::copy(&mut source, &mut destination)?;
    if preserve {
        destination.set_permissions(fs::Permissions::from_mode(source_meta.mode() & 0o7777))?;
        set_file_times(destination.as_raw_fd(), &source_meta)?;
    }
    Ok(())
}

fn set_file_times(fd: libc::c_int, meta: &fs::Metadata) -> io::Result<()> {
    let times = [
        libc::timespec {
            tv_sec: meta.atime() as libc::time_t,
            tv_nsec: meta.atime_nsec() as libc::c_long,
        },
        libc::timespec {
            tv_sec: meta.mtime() as libc::time_t,
            tv_nsec: meta.mtime_nsec() as libc::c_long,
        },
    ];
    // SAFETY: fd is an open file and `times` holds two valid timespecs.
    if unsafe { libc::futimens(fd, times.as_ptr()) } != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn copy_directory(src: &Path, dst: &Path, preserve: bool) -> io::Result<()> {
    let source = open_directory(src)?;
    let source_meta = source.metadata()?;
    let mode = source_meta.mode() & 0o7777;
    let (destination, created) = match open_parent(dst) {
        Ok((parent, name)) => {
            let mut existing: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: `parent` is a directory descriptor and `name` is a valid
            // NUL-terminated child name.
            let found = unsafe {
                libc::fstatat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    &mut existing,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            let created = if found == 0 {
                if existing.st_mode & libc::S_IFMT != libc::S_IFDIR {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "destination is not a directory",
                    ));
                }
                false
            } else {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::NotFound {
                    return Err(error);
                }
                // A read-only source directory must not make its new
                // destination read-only before the copy has written the
                // children into it.
                // SAFETY: the parent is a directory descriptor and `name` is
                // valid.
                let made =
                    unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), mode | 0o700) };
                if made != 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(error);
                    }
                    // Another process created it between fstatat and mkdirat;
                    // do not chmod a directory this call did not create.
                    false
                } else {
                    true
                }
            };

            // SAFETY: `parent` is a directory descriptor, `name` is valid, and
            // the final component is not followed.
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: openat returned a fresh descriptor we own.
            (unsafe { fs::File::from_raw_fd(fd) }, created)
        }
        // `cp -a src/. .` and `cp -a src/. /` name an existing directory rather
        // than a child of its parent; copy the entries into it directly.
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
            let destination = open_directory_follow(dst)?;
            if !destination.metadata()?.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "destination is not a directory",
                ));
            }
            (destination, false)
        }
        Err(error) => return Err(error),
    };
    if created {
        destination.set_permissions(fs::Permissions::from_mode(mode | 0o700))?;
    }
    copy_dir_entries(source.as_raw_fd(), destination.as_raw_fd(), preserve)?;
    if created {
        destination.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    if preserve {
        set_file_times(destination.as_raw_fd(), &source_meta)?;
    }
    Ok(())
}

fn copy_dir_entries(
    source_fd: libc::c_int,
    destination_fd: libc::c_int,
    preserve: bool,
) -> io::Result<()> {
    // SAFETY: dup creates a private descriptor for fdopendir to own.
    let duplicated = unsafe { libc::dup(source_fd) };
    if duplicated < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: duplicated is an open directory descriptor and fdopendir owns it.
    let dir = unsafe { libc::fdopendir(duplicated) };
    if dir.is_null() {
        // SAFETY: fdopendir failed, so the duplicate is still ours.
        unsafe { libc::close(duplicated) };
        return Err(io::Error::last_os_error());
    }

    let mut result = Ok(());
    loop {
        // SAFETY: errno is reset because readdir uses NULL for both EOF and error.
        unsafe { *libc::__errno_location() = 0 };
        // SAFETY: dir is a valid DIR stream.
        let entry = unsafe { libc::readdir(dir) };
        if entry.is_null() {
            // SAFETY: errno storage is valid.
            let errno = unsafe { *libc::__errno_location() };
            if errno != 0 {
                result = Err(io::Error::from_raw_os_error(errno));
            }
            break;
        }
        // SAFETY: a non-null dirent has a NUL-terminated d_name.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let Ok(name_c) = CString::new(bytes) else {
            result = Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory entry contains NUL",
            ));
            break;
        };
        // SAFETY: both descriptors are valid directories and name_c is valid.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                source_fd,
                name_c.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            result = Err(io::Error::last_os_error());
            break;
        }
        let mode = stat.st_mode & 0o7777;
        if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
            // SAFETY: openat does not follow the final component.
            let child_source = unsafe {
                libc::openat(
                    source_fd,
                    name_c.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if child_source < 0 {
                result = Err(io::Error::last_os_error());
                break;
            }
            // SAFETY: mkdirat creates the destination relative to its dirfd.
            // Keep the owner's write bit while the children are copied; the
            // final chmod below restores the source mode.
            let mkdir = unsafe { libc::mkdirat(destination_fd, name_c.as_ptr(), mode | 0o700) };
            if mkdir != 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::AlreadyExists {
                    unsafe { libc::close(child_source) };
                    result = Err(error);
                    break;
                }
            }
            // SAFETY: the child undertakes the same no-follow open as above.
            let child_destination = unsafe {
                libc::openat(
                    destination_fd,
                    name_c.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if child_destination < 0 {
                unsafe { libc::close(child_source) };
                result = Err(io::Error::last_os_error());
                break;
            }
            // SAFETY: both child descriptors are valid directory descriptors
            // and are consumed by the recursive call's dup/fdopendir path.
            if let Err(error) = copy_dir_entries(child_source, child_destination, preserve) {
                unsafe {
                    libc::close(child_source);
                    libc::close(child_destination);
                }
                result = Err(error);
                break;
            }
            // SAFETY: descriptors are still ours after the recursive read.
            unsafe {
                libc::fchmod(child_destination, mode);
                if preserve {
                    let mut child_meta: libc::stat = std::mem::zeroed();
                    libc::fstat(child_source, &mut child_meta);
                    // `futimens` is easiest through a File view, but the fd is
                    // still our own here.
                    let file = fs::File::from_raw_fd(child_destination);
                    let times = [
                        libc::timespec {
                            tv_sec: child_meta.st_atime as libc::time_t,
                            tv_nsec: child_meta.st_atime_nsec as libc::c_long,
                        },
                        libc::timespec {
                            tv_sec: child_meta.st_mtime as libc::time_t,
                            tv_nsec: child_meta.st_mtime_nsec as libc::c_long,
                        },
                    ];
                    libc::futimens(file.as_raw_fd(), times.as_ptr());
                    libc::close(child_source);
                    // File drops and closes child_destination.
                    drop(file);
                } else {
                    libc::close(child_source);
                    libc::close(child_destination);
                }
            }
        } else if stat.st_mode & libc::S_IFMT == libc::S_IFLNK {
            let mut target = vec![0u8; 4096];
            // SAFETY: readlinkat writes at most target.len() bytes.
            let read = unsafe {
                libc::readlinkat(
                    source_fd,
                    name_c.as_ptr(),
                    target.as_mut_ptr().cast(),
                    target.len(),
                )
            };
            if read < 0 {
                result = Err(io::Error::last_os_error());
                break;
            }
            target.truncate(read as usize);
            // SAFETY: unlinkat ignores a missing destination.
            unsafe { libc::unlinkat(destination_fd, name_c.as_ptr(), 0) };
            let target_c = CString::new(target)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "symlink contains NUL"))?;
            // SAFETY: symlinkat creates the link relative to the destination fd.
            if unsafe { libc::symlinkat(target_c.as_ptr(), destination_fd, name_c.as_ptr()) } != 0 {
                result = Err(io::Error::last_os_error());
                break;
            }
        } else if stat.st_mode & libc::S_IFMT == libc::S_IFREG {
            // SAFETY: openat does not follow the final component.
            let child_source = unsafe {
                libc::openat(
                    source_fd,
                    name_c.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if child_source < 0 {
                result = Err(io::Error::last_os_error());
                break;
            }
            // SAFETY: openat creates the destination file without following a
            // final symlink. Truncation waits for the same-inode check below.
            let child_destination = unsafe {
                libc::openat(
                    destination_fd,
                    name_c.as_ptr(),
                    libc::O_WRONLY | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    mode,
                )
            };
            if child_destination < 0 {
                unsafe { libc::close(child_source) };
                result = Err(io::Error::last_os_error());
                break;
            }
            // SAFETY: both descriptors are fresh and owned here.
            let mut source = unsafe { fs::File::from_raw_fd(child_source) };
            let mut destination = unsafe { fs::File::from_raw_fd(child_destination) };
            let source_meta = match source.metadata() {
                Ok(meta) => meta,
                Err(error) => {
                    result = Err(error);
                    break;
                }
            };
            let destination_meta = match destination.metadata() {
                Ok(meta) => meta,
                Err(error) => {
                    result = Err(error);
                    break;
                }
            };
            if source_meta.dev() == destination_meta.dev()
                && source_meta.ino() == destination_meta.ino()
            {
                result = Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "source and destination are the same file",
                ));
                break;
            }
            if let Err(error) = destination.set_len(0) {
                result = Err(error);
                break;
            }
            if let Err(error) = io::copy(&mut source, &mut destination) {
                result = Err(error);
                break;
            }
            if preserve && unsafe { libc::fchmod(destination.as_raw_fd(), mode) } != 0 {
                result = Err(io::Error::last_os_error());
                break;
            }
            if preserve {
                let mut child_meta: libc::stat = unsafe { std::mem::zeroed() };
                // SAFETY: source is valid and its fd is open.
                unsafe { libc::fstat(source.as_raw_fd(), &mut child_meta) };
                let times = [
                    libc::timespec {
                        tv_sec: child_meta.st_atime as libc::time_t,
                        tv_nsec: child_meta.st_atime_nsec as libc::c_long,
                    },
                    libc::timespec {
                        tv_sec: child_meta.st_mtime as libc::time_t,
                        tv_nsec: child_meta.st_mtime_nsec as libc::c_long,
                    },
                ];
                // SAFETY: destination is valid and times holds two timespecs.
                unsafe { libc::futimens(destination.as_raw_fd(), times.as_ptr()) };
            }
        } else {
            result = Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported file type",
            ));
            break;
        }
    }
    // SAFETY: dir was returned by fdopendir and is no longer used.
    unsafe { libc::closedir(dir) };
    result
}

/// Remove one entry, descending into directories only when `recursive`.
///
/// The traversal is fd-relative and opens every child directory with
/// `O_NOFOLLOW`. A path component swapped for a symlink after the first check
/// therefore fails instead of redirecting the deletion outside the tree.
pub fn remove_entry(path: &Path, recursive: bool) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        if !recursive {
            return Err(io::Error::other("is a directory"));
        }
        remove_tree(path)
    } else {
        fs::remove_file(path)
    }
}

fn remove_tree(path: &Path) -> io::Result<()> {
    let path_c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: `path_c` is NUL-terminated and the flags only describe a
    // directory open with the final component not followed.
    let fd = unsafe {
        libc::open(
            path_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let result = remove_tree_fd(fd);
    if result.is_ok() {
        // The fd traversal below removed the contents. `rmdir` cannot follow a
        // symlink, so a replaced final component fails rather than escaping.
        fs::remove_dir(path)
    } else {
        result
    }
}

fn remove_tree_fd(fd: libc::c_int) -> io::Result<()> {
    // SAFETY: fd is a directory we own; fdopendir takes ownership of it.
    let dir = unsafe { libc::fdopendir(fd) };
    if dir.is_null() {
        // SAFETY: fd is still ours when fdopendir fails.
        unsafe { libc::close(fd) };
        return Err(io::Error::last_os_error());
    }

    let mut result = Ok(());
    loop {
        // SAFETY: dir is a valid DIR stream. errno is set to zero because
        // readdir returns NULL both at EOF and on error.
        unsafe { *libc::__errno_location() = 0 };
        // SAFETY: dir is valid and readdir returns a valid dirent or NULL.
        let entry = unsafe { libc::readdir(dir) };
        if entry.is_null() {
            // SAFETY: errno storage is valid.
            let errno = unsafe { *libc::__errno_location() };
            if errno != 0 {
                result = Err(io::Error::from_raw_os_error(errno));
            }
            break;
        }
        // SAFETY: a non-null dirent has a NUL-terminated d_name.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let Ok(name_c) = CString::new(bytes) else {
            result = Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory entry contains NUL",
            ));
            break;
        };
        // SAFETY: dir and name_c are valid; fstatat does not follow the final
        // component when AT_SYMLINK_NOFOLLOW is set.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                libc::dirfd(dir),
                name_c.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            result = Err(io::Error::last_os_error());
            break;
        }
        if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
            // SAFETY: dirfd/name_c are valid and the child is opened without
            // following a final symlink.
            let child = unsafe {
                libc::openat(
                    libc::dirfd(dir),
                    name_c.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if child < 0 {
                result = Err(io::Error::last_os_error());
                break;
            }
            if let Err(error) = remove_tree_fd(child) {
                result = Err(error);
                break;
            }
            // SAFETY: the child has been emptied, and unlinkat removes the
            // directory relative to the already-open parent.
            if unsafe { libc::unlinkat(libc::dirfd(dir), name_c.as_ptr(), libc::AT_REMOVEDIR) } != 0
            {
                result = Err(io::Error::last_os_error());
                break;
            }
        } else {
            // SAFETY: unlinkat with flags 0 removes a non-directory entry and
            // does not follow a symlink.
            if unsafe { libc::unlinkat(libc::dirfd(dir), name_c.as_ptr(), 0) } != 0 {
                result = Err(io::Error::last_os_error());
                break;
            }
        }
    }
    // SAFETY: dir was returned by fdopendir and is no longer used.
    unsafe { libc::closedir(dir) };
    result
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
