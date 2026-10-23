//! Name-to-id lookup from `/etc/passwd` and `/etc/group` (plan §1.3(12), used
//! again by the privilege drop in §7.4).
//!
//! The image carries dynamic glibc for `rinit`/`rctl`/`processes`, but it does
//! not carry the NSS `libnss_files.so` module. A glibc `getpwnam` would need
//! that module at run time, so the two lookups `User=`/`Group=` need are parsed
//! here instead, from the same files `login` already reads. Being pure, this is also the part that can
//! be tested without a system user database - the "files" backend of every
//! NSS configuration, reduced to what an initramfs actually ships.

use alloc::vec::Vec;

/// The uid of `name` in an `/etc/passwd` body, if present.
///
/// A row is `name:passwd:uid:gid:gecos:home:shell`.
pub fn passwd_uid(contents: &str, name: &str) -> Option<u32> {
    passwd_ids(contents, name).map(|(uid, _)| uid)
}

/// The uid and primary gid of `name` in an `/etc/passwd` body, if present.
///
/// The primary gid is what a service runs under when it sets `User=` but no
/// `Group=` (plan §7.4), so both ids come out of one pass over the file rather
/// than two.
pub fn passwd_ids(contents: &str, name: &str) -> Option<(u32, u32)> {
    if name.is_empty() {
        return None;
    }
    contents.lines().find_map(|line| {
        let mut fields = line.split(':');
        if fields.next() != Some(name) {
            return None;
        }
        // Skip the password field, then read the uid and gid. `trim` also
        // strips the `\r` of a CRLF file and any stray padding.
        let _password = fields.next()?;
        let uid = fields.next()?.trim().parse().ok()?;
        let gid = fields.next()?.trim().parse().ok()?;
        Some((uid, gid))
    })
}

/// The gids of every group in an `/etc/group` body that lists `name` as a
/// member, in file order and without repeats.
///
/// A row is `name:passwd:gid:members`, where `members` is a comma-separated
/// list. This is the set `initgroups(3)` would compute, parsed here because
/// `initgroups` is as NSS-backed as `getpwnam` and would need
/// `libnss_files.so` at run time - the same reason [`passwd_uid`] does not call
/// `getpwnam` (see this module's header). The rows are not the whole answer:
/// the primary gid belongs in the child's group list too, and the caller adds
/// it.
///
/// Membership is matched on the whole field, so `harry` does not pick up
/// `harrison`'s groups. A malformed row is skipped rather than ending the
/// search.
pub fn supplementary_gids(contents: &str, name: &str) -> Vec<u32> {
    let mut gids = Vec::new();
    if name.is_empty() {
        return gids;
    }
    for line in contents.lines() {
        let mut fields = line.split(':');
        // The group's own name is not what matters here - membership is - but
        // the two fields before the id still have to be skipped.
        if fields.next().is_none() {
            continue;
        }
        let _password = fields.next();
        let Some(gid) = fields.next().and_then(|f| f.trim().parse::<u32>().ok()) else {
            continue;
        };
        let members = fields.next().unwrap_or("");
        if members.split(',').any(|member| member.trim() == name) && !gids.contains(&gid) {
            gids.push(gid);
        }
    }
    gids
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
    use alloc::vec;

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

    /// `User=` without `Group=` runs the service under the user's *primary*
    /// group, so the gid field of the passwd row has to be readable.
    #[test]
    fn the_primary_gid_comes_from_the_passwd_row() {
        assert_eq!(passwd_ids(PASSWD, "harrison"), Some((1000, 1000)));
        assert_eq!(passwd_ids("u:x:7:9:g:/h:/s\n", "u"), Some((7, 9)));
    }

    #[test]
    fn an_unknown_user_has_no_ids() {
        assert_eq!(passwd_ids(PASSWD, "nobody"), None);
    }

    /// A row that stops before the gid cannot supply one, and must not be
    /// mistaken for a user whose group is `0`.
    #[test]
    fn a_row_without_a_gid_is_not_a_match() {
        assert_eq!(passwd_ids("u:x:7::/h:/s\n", "u"), None);
        assert_eq!(passwd_uid("u:x:7::/h:/s\n", "u"), None);
    }

    /// Supplementary group membership: every group that lists the user.
    #[test]
    fn supplementary_groups_are_the_ones_listing_the_user() {
        let body = "root:x:0:\nwheel:x:10:harrison\naudio:x:63:harrison,dave\n";
        assert_eq!(supplementary_gids(body, "harrison"), vec![10, 63]);
    }

    /// Membership is matched on the whole name, so `harry` is not a member of
    /// `harrison`'s groups.
    #[test]
    fn a_member_prefix_does_not_count() {
        let body = "wheel:x:10:harrison\n";
        assert_eq!(supplementary_gids(body, "harry"), Vec::<u32>::new());
        assert_eq!(supplementary_gids(body, "harrison"), vec![10]);
    }

    /// A group listed twice is one entry, and a membership list padded with
    /// spaces still matches (`wheel:x:10: a, harrison `).
    #[test]
    fn members_are_read_once_each_and_trimmed() {
        let body = "a:x:1:harrison\nb:x:1:harrison\nc:x:2: a, harrison \n";
        assert_eq!(supplementary_gids(body, "harrison"), vec![1, 2]);
    }

    #[test]
    fn an_unknown_user_has_no_supplementary_groups() {
        assert_eq!(supplementary_gids(GROUP, "nobody"), Vec::<u32>::new());
        assert_eq!(supplementary_gids(GROUP, ""), Vec::<u32>::new());
    }

    /// A group with an empty member list, or a malformed row, contributes
    /// nothing - and neither hides the valid rows.
    #[test]
    fn malformed_group_rows_are_skipped() {
        let body = "broken\nno-id:x::harrison\nok:x:5:harrison\n";
        assert_eq!(supplementary_gids(body, "harrison"), vec![5]);
    }
}
