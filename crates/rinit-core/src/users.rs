//! Name-to-id lookup from `/etc/passwd` and `/etc/group` (plan §1.3(12)).
//!
//! rinit is built with `crt-static` (`.cargo/config.toml`), and a *static*
//! glibc `getpwnam` still needs the NSS `libnss_files.so` at run time: it is
//! `dlopen`ed, and a 2 MB initramfs does not carry it. The linker says as much
//! on every aarch64 build:
//!
//! ```text
//! warning: Using 'getpwnam' in statically linked applications requires at
//! runtime the shared libraries from the glibc version used for linking
//! ```
//!
//! So the two lookups `User=`/`Group=` need are parsed here instead, from the
//! same files `login` already reads. Being pure, this is also the part that can
//! be tested without a system user database - the "files" backend of every
//! NSS configuration, reduced to what an initramfs actually ships.

/// The uid of `name` in an `/etc/passwd` body, if present.
///
/// A row is `name:passwd:uid:gid:gecos:home:shell`.
pub fn passwd_uid(contents: &str, name: &str) -> Option<u32> {
    id_of(contents, name)
}

/// The gid of `name` in an `/etc/group` body, if present.
///
/// A row is `name:passwd:gid:members`. The id is the third field, the same
/// column as `/etc/passwd`, which is why one parser serves both.
pub fn group_gid(contents: &str, name: &str) -> Option<u32> {
    id_of(contents, name)
}

/// Find the row whose first `:`-separated field is exactly `name` and parse its
/// third field as an id.
///
/// The match is on the whole field: a prefix like `rootx` must not resolve to
/// `root`'s id. A row that is too short, or whose id is not a number, is
/// skipped rather than aborting the search, so one malformed line cannot hide
/// the valid rows after it.
fn id_of(contents: &str, name: &str) -> Option<u32> {
    if name.is_empty() {
        return None;
    }
    contents.lines().find_map(|line| {
        let mut fields = line.split(':');
        if fields.next() != Some(name) {
            return None;
        }
        // Skip the password field, then read the id. `trim` also strips the
        // `\r` of a CRLF file and any stray padding.
        fields.nth(1)?.trim().parse().ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "\
root:x:0:0:root:/root:/bin/sh
daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin
harrison:x:1000:1000:Harrison:/home/harrison:/bin/bash
";

    const GROUP: &str = "root:x:0:\nwheel:x:10:harrison\nharrison:x:1000:\n";

    #[test]
    fn finds_a_user_by_name() {
        assert_eq!(passwd_uid(PASSWD, "root"), Some(0));
        assert_eq!(passwd_uid(PASSWD, "harrison"), Some(1000));
    }

    #[test]
    fn finds_a_group_by_name() {
        assert_eq!(group_gid(GROUP, "root"), Some(0));
        assert_eq!(group_gid(GROUP, "wheel"), Some(10));
    }

    #[test]
    fn an_unknown_name_is_none() {
        assert_eq!(passwd_uid(PASSWD, "nobody"), None);
        assert_eq!(group_gid(GROUP, "nobody"), None);
    }

    /// The id is the third field, never the second (`gid`) or the first.
    #[test]
    fn only_the_third_field_is_read() {
        let one = "u:x:5:9:g:/h:/s\n";
        assert_eq!(
            passwd_uid(one, "u"),
            Some(5),
            "the gid must not be returned"
        );
    }

    /// A name must match the whole field, so `rootx` is not `root`.
    #[test]
    fn a_prefix_does_not_match() {
        assert_eq!(passwd_uid("rootx:x:7:7::/:/b\n", "root"), None);
        assert_eq!(passwd_uid(PASSWD, "roo"), None);
    }

    /// A malformed row is skipped, but the well-formed rows still resolve.
    #[test]
    fn a_malformed_row_does_not_hide_later_rows() {
        let body = "broken:row\nroot:x:0:0::/:/b\nalso-broken:x\n";
        assert_eq!(passwd_uid(body, "root"), Some(0));
    }

    /// A non-numeric id is treated as absent rather than as some default.
    #[test]
    fn a_non_numeric_id_is_none() {
        assert_eq!(passwd_uid("u:x:notanumber:0::/:/b\n", "u"), None);
    }

    #[test]
    fn the_final_line_without_a_newline_is_read() {
        assert_eq!(passwd_uid("root:x:0:0::/:/b", "root"), Some(0));
    }

    #[test]
    fn blank_and_unrelated_lines_are_skipped() {
        let body = "\n# a comment that is not a real passwd row\nroot:x:0:0::/:/b\n";
        assert_eq!(passwd_uid(body, "root"), Some(0));
    }

    #[test]
    fn an_empty_name_never_matches() {
        assert_eq!(passwd_uid(":x:0:0::/:/b\n", ""), None);
        assert_eq!(group_gid(GROUP, ""), None);
    }

    /// The first match wins, matching the files backend.
    #[test]
    fn the_first_match_wins() {
        assert_eq!(
            passwd_uid("dup:x:1:1::/:/b\ndup:x:2:2::/:/b\n", "dup"),
            Some(1)
        );
    }
}
