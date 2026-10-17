//! Unit directory loading (plan §4.3 `/etc/rinit/units/`).
//!
//! Discovery is deliberately separate from parsing: this module finds and reads
//! the files, and `rinit_core`'s parser decides whether each one is valid.
//! Keeping them apart means the parser stays a pure function and the file I/O
//! stays testable with a temporary directory.
//!
//! Every file in the directory whose name ends in a known unit suffix is a unit
//! candidate. Its file name *is* its identity, so `net.target` and `net`
//! refer to the same unit and cannot both exist.

use std::fs;
use std::path::{Path, PathBuf};

use rinit_core::UnitName;

use crate::error::{RuntimeError, SyscallError};

/// Default unit directory (plan §4.3).
pub const DEFAULT_UNIT_DIR: &str = "/etc/rinit/units";

/// The default boot target (plan §7.2: `multi-user.target` is `default.target`).
pub const DEFAULT_TARGET: &str = "multi-user.target";

/// One unit file, with its name and raw contents.
#[derive(Debug, Clone)]
pub struct UnitFile {
    /// The unit's name, taken from the file name.
    pub name: UnitName,
    /// Absolute path it was read from, for error messages.
    pub path: PathBuf,
    /// The file's contents.
    pub contents: String,
}

/// Load every unit file in `dir`, sorted by file name.
///
/// # Errors
/// Returns a [`RuntimeError::Io`] if the directory cannot be listed. Individual
/// unreadable files are skipped with the returned warnings rather than failing
/// the whole load, so one bad symlink cannot make the system unbootable.
pub fn load_unit_files(dir: &Path) -> Result<(Vec<UnitFile>, Vec<String>), RuntimeError> {
    let entries = fs::read_dir(dir).map_err(|e| RuntimeError::Io {
        op: "read_dir(units)",
        source: e,
    })?;

    let mut files: Vec<UnitFile> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warnings.push(format!("skipping an unreadable directory entry: {e}"));
                continue;
            }
        };
        let path = entry.path();

        // Only regular files with a recognised suffix are units; this skips
        // editor backups, dotfiles and anything else that lives there.
        let Some(name) = unit_name_of(&path) else {
            continue;
        };

        match fs::read_to_string(&path) {
            Ok(contents) => files.push(UnitFile {
                name,
                path,
                contents,
            }),
            Err(e) => warnings.push(format!("skipping {}: {e}", path.display())),
        }
    }

    // Sorting here means the caller sees a stable order regardless of what the
    // filesystem returns, which keeps the whole boot reproducible.
    files.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((files, warnings))
}

/// Derive a unit name from a file name, or `None` if it is not a unit file.
///
/// An explicit suffix is required. `UnitName::parse` alone would accept a bare
/// `README` and canonicalise it to `README.service`, which would make every
/// stray file in the directory a unit - and a file without `ExecStart=` then
/// fails to parse and takes the boot down with it. Bare names default to
/// `.service` only inside dependency *directives* (plan §1.1), never here.
fn unit_name_of(path: &Path) -> Option<UnitName> {
    let file_name = path.file_name()?.to_str()?;
    const SUFFIXES: &[&str] = &[".service", ".target"];
    if !SUFFIXES.iter().any(|s| file_name.ends_with(s)) {
        return None;
    }
    // `UnitName::parse` then rejects anything with an invalid character, so
    // `bad name.service` cannot become a unit either.
    UnitName::parse(file_name).ok()
}

/// Read a single unit file by path.
///
/// # Errors
/// Returns [`RuntimeError::Io`] if the file cannot be read, or
/// [`RuntimeError::Syscall`] if its name is not a valid unit name.
pub fn read_unit_file(path: &Path) -> Result<UnitFile, RuntimeError> {
    let name = unit_name_of(path)
        .ok_or_else(|| RuntimeError::Syscall(SyscallError::new("unit file name", libc::EINVAL)))?;
    let contents = fs::read_to_string(path).map_err(|e| RuntimeError::Io {
        op: "read_to_string(unit)",
        source: e,
    })?;
    Ok(UnitFile {
        name,
        path: path.to_path_buf(),
        contents,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory that cleans itself up, so these tests never touch
    /// the real `/etc/rinit/units`.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("rinit-units-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temp dir");
            TempDir(path)
        }

        fn write(&self, name: &str, contents: &str) {
            fs::write(self.0.join(name), contents).expect("write unit file");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn loads_unit_files_sorted_by_name() {
        let dir = TempDir::new("sorted");
        dir.write("zeta.service", "[Service]\nExecStart=/bin/true\n");
        dir.write("alpha.target", "[Unit]\n");
        dir.write("beta.service", "[Service]\nExecStart=/bin/true\n");

        let (files, warnings) = load_unit_files(&dir.0).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let names: Vec<&str> = files.iter().map(|f| f.name.canonical()).collect();
        assert_eq!(names, ["alpha.target", "beta.service", "zeta.service"]);
    }

    /// Files that are not units must be ignored, not rejected: a stray backup
    /// in the unit directory must not break the boot.
    #[test]
    fn ignores_files_that_are_not_units() {
        let dir = TempDir::new("ignore");
        dir.write("good.service", "[Service]\nExecStart=/bin/true\n");
        dir.write("README", "not a unit");
        dir.write(".hidden", "not a unit");
        dir.write("bad name.service", "invalid");
        dir.write("notes.txt", "not a unit");

        let (files, _) = load_unit_files(&dir.0).unwrap();
        assert_eq!(files.len(), 1, "only good.service should load");
        assert_eq!(files[0].name.canonical(), "good.service");
    }

    /// A unit with no recognised suffix is not a unit.
    #[test]
    fn ignores_files_without_a_known_suffix() {
        let dir = TempDir::new("suffix");
        dir.write("plain", "[Service]\nExecStart=/bin/true\n");
        let (files, _) = load_unit_files(&dir.0).unwrap();
        assert!(files.is_empty());
    }

    /// The file name is the unit's identity, so `x` and `x.service` are the
    /// same unit and the loader must not produce both.
    #[test]
    fn the_file_name_is_the_identity() {
        let dir = TempDir::new("identity");
        dir.write("web.service", "[Service]\nExecStart=/bin/true\n");
        let (files, _) = load_unit_files(&dir.0).unwrap();
        assert_eq!(files[0].name.canonical(), "web.service");
        assert_eq!(files[0].path.file_name().unwrap(), "web.service");
    }

    #[test]
    fn a_missing_directory_is_an_error_not_a_panic() {
        let err = load_unit_files(Path::new("/nonexistent/rinit/units")).unwrap_err();
        assert!(matches!(
            err,
            RuntimeError::Io {
                op: "read_dir(units)",
                ..
            }
        ));
    }

    /// An empty directory is legal; it just yields nothing to start.
    #[test]
    fn an_empty_directory_loads_nothing() {
        let dir = TempDir::new("empty");
        let (files, warnings) = load_unit_files(&dir.0).unwrap();
        assert!(files.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn contents_are_read_verbatim() {
        let dir = TempDir::new("contents");
        dir.write("a.service", "[Unit]\nDescription=has trailing spaces   \n");
        let (files, _) = load_unit_files(&dir.0).unwrap();
        assert!(files[0].contents.contains("trailing spaces"));
    }

    #[test]
    fn read_unit_file_rejects_a_non_unit_name() {
        let dir = TempDir::new("read-reject");
        fs::write(dir.0.join("README"), "x").unwrap();
        let err = read_unit_file(&dir.0.join("README")).unwrap_err();
        assert!(matches!(err, RuntimeError::Syscall(_)), "got {err:?}");
    }

    #[test]
    fn read_unit_file_returns_contents_and_name() {
        let dir = TempDir::new("read-ok");
        dir.write("a.service", "[Service]\nExecStart=/bin/true\n");
        let file = read_unit_file(&dir.0.join("a.service")).unwrap();
        assert_eq!(file.name.canonical(), "a.service");
        assert!(file.contents.contains("ExecStart"));
    }

    /// Plan §4.3's "one bad entry must not make the system unbootable": a unit
    /// file that cannot be read is skipped with a warning, and the rest of the
    /// directory still loads.
    ///
    /// A *directory* named like a unit is the portable way to reach that path: it
    /// has the suffix the loader looks for, and reading it fails for everyone -
    /// including root, which a `chmod 000` file would not.
    #[test]
    fn an_unreadable_unit_file_is_skipped_with_a_warning() {
        let dir = TempDir::new("unreadable");
        dir.write("good.service", "[Service]\nExecStart=/bin/true\n");
        fs::create_dir(dir.0.join("bad.service")).unwrap();

        let (files, warnings) = load_unit_files(&dir.0).unwrap();

        assert_eq!(files.len(), 1, "the readable unit still loads");
        assert_eq!(files[0].name.canonical(), "good.service");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].starts_with("skipping"), "{warnings:?}");
        assert!(
            warnings[0].contains("bad.service"),
            "the warning must name the file: {warnings:?}"
        );
    }

    /// The single-file path reports the same failure as an error rather than a
    /// warning, because there is no "rest of the directory" to carry on with.
    #[test]
    fn read_unit_file_reports_an_unreadable_file() {
        let dir = TempDir::new("read-unreadable");
        fs::create_dir(dir.0.join("bad.service")).unwrap();

        let err = read_unit_file(&dir.0.join("bad.service")).unwrap_err();

        assert!(
            matches!(
                err,
                RuntimeError::Io {
                    op: "read_to_string(unit)",
                    ..
                }
            ),
            "got {err:?}"
        );
    }
}
