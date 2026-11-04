//! `/etc/fstab` parsing shared by `mount`, `swapon` and `swapoff`.
//!
//! The format is deliberately the six-field subset the image needs: no UUID or
//! LABEL resolution, no generators, and comments only at the end of a line.
//! Device and mount-point fields decode the traditional three-digit octal
//! escapes so a path containing a space is still one entry.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;

/// One parsed fstab line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Device or filesystem source.
    pub device: OsString,
    /// Mount point, or `none` for swap.
    pub mountpoint: OsString,
    /// Filesystem type.
    pub fstype: String,
    /// Comma-separated options.
    pub options: String,
}

/// Decode the `\040`, `\011`, `\012` and `\134` escapes used by fstab.
fn unescape(field: &str) -> OsString {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(byte) = bytes.get(index).copied() {
        if byte == b'\\'
            && let (Some(a), Some(b), Some(c)) = (
                bytes.get(index + 1).copied(),
                bytes.get(index + 2).copied(),
                bytes.get(index + 3).copied(),
            )
            && (b'0'..=b'7').contains(&a)
            && (b'0'..=b'7').contains(&b)
            && (b'0'..=b'7').contains(&c)
        {
            let value = (a - b'0') * 64 + (b - b'0') * 8 + (c - b'0');
            out.push(value);
            index += 4;
        } else {
            out.push(byte);
            index += 1;
        }
    }
    OsString::from_vec(out)
}

/// Parse fstab text. Malformed lines are returned as warnings, not entries.
pub fn parse(text: &str) -> (Vec<Entry>, Vec<String>) {
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [device, mountpoint, fstype, options, ..] = fields.as_slice() else {
            warnings.push(format!(
                "line {}: malformed fstab entry: {line}",
                number + 1
            ));
            continue;
        };
        entries.push(Entry {
            device: unescape(device),
            mountpoint: unescape(mountpoint),
            fstype: fstype.to_string(),
            options: options.to_string(),
        });
    }
    (entries, warnings)
}

/// Whether an option list contains `option`.
pub fn has_option(options: &str, option: &str) -> bool {
    options.split(',').any(|value| value == option)
}

/// Read `root/etc/fstab` and keep only swap entries.
///
/// `swapon -a` skips `noauto` entries; `swapoff -a` deliberately includes
/// them, so a swap device that was activated explicitly is still released.
pub fn read_swap_entries(
    root: &Path,
    include_noauto: bool,
) -> Result<(Vec<Entry>, Vec<String>), String> {
    let path = root.join("etc/fstab");
    let text =
        std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let (entries, warnings) = parse(&text);
    let entries = entries
        .into_iter()
        .filter(|entry| entry.fstype == "swap")
        .filter(|entry| include_noauto || !has_option(&entry.options, "noauto"))
        .collect();
    Ok((entries, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fields_and_skips_comments() {
        let (entries, warnings) =
            parse("# header\n/dev/vda1 / ext4 defaults 0 1\n\n/swapfile none swap sw,pri=5 0 0\n");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].device, "/dev/vda1");
        assert_eq!(entries[0].mountpoint, "/");
        assert_eq!(entries[0].fstype, "ext4");
        assert_eq!(entries[1].device, "/swapfile");
        assert_eq!(entries[1].options, "sw,pri=5");
    }

    #[test]
    fn decodes_octal_escapes_and_reports_malformed_lines() {
        let (entries, warnings) = parse("tmpfs /with\\040space tmpfs defaults 0 0\nbroken\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].mountpoint, "/with space");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("line 2"), "{warnings:?}");
    }

    #[test]
    fn option_lookup_is_exact() {
        assert!(has_option("rw,noauto,user", "noauto"));
        assert!(!has_option("rw,noauto,user", "auto"));
    }

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-fstab-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("etc")).expect("etc");
        root
    }

    #[test]
    fn swap_entries_honour_noauto_for_swapon_only() {
        let root = temp_root("swaps");
        std::fs::write(
            root.join("etc/fstab"),
            "/dev/vda1 / ext4 defaults 0 1\n\
             /swapfile none swap sw,pri=5 0 0\n\
             /spare none swap sw,noauto 0 0\n",
        )
        .expect("fstab");
        let (active, warnings) = read_swap_entries(&root, false).expect("fstab");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].device, "/swapfile");
        let (all, _) = read_swap_entries(&root, true).expect("fstab");
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].device, "/spare");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_missing_fstab_names_the_path() {
        let root = temp_root("missing");
        let error = read_swap_entries(&root, false).expect_err("missing fstab");
        assert!(error.contains("fstab"), "{error}");
        let _ = std::fs::remove_dir_all(root);
    }
}
