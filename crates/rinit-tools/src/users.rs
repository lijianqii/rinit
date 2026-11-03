//! User and group name lookup from the image's local files.
//!
//! The tools deliberately read `/etc/passwd` and `/etc/group` instead of
//! pulling in NSS: the device image ships no NSS modules, and the numeric ID is
//! a useful fallback when a name is absent.

use std::fs;

pub fn user_name(uid: u32) -> Option<String> {
    let text = fs::read_to_string("/etc/passwd").ok()?;
    user_name_from(&text, uid)
}

pub fn group_name(gid: u32) -> Option<String> {
    let text = fs::read_to_string("/etc/group").ok()?;
    group_name_from(&text, gid)
}

pub fn user_name_from(text: &str, uid: u32) -> Option<String> {
    for line in text.lines() {
        let mut fields = line.split(':');
        let Some(name) = fields.next() else {
            continue;
        };
        let Some(_password) = fields.next() else {
            continue;
        };
        let Some(id) = fields.next() else {
            continue;
        };
        if id.parse::<u32>().ok() == Some(uid) {
            return Some(name.to_string());
        }
    }
    None
}

pub fn group_name_from(text: &str, gid: u32) -> Option<String> {
    for line in text.lines() {
        let mut fields = line.split(':');
        let Some(name) = fields.next() else {
            continue;
        };
        let Some(_password) = fields.next() else {
            continue;
        };
        let Some(id) = fields.next() else {
            continue;
        };
        if id.parse::<u32>().ok() == Some(gid) {
            return Some(name.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_lookup_matches_numeric_ids() {
        let text = "root:x:0:0::/root:/bin/sh\nnobody:x:65534:65534::/:/bin/false\n";
        assert_eq!(user_name_from(text, 0).as_deref(), Some("root"));
        assert_eq!(user_name_from(text, 65534).as_deref(), Some("nobody"));
        assert_eq!(user_name_from(text, 1234), None);
    }

    #[test]
    fn group_lookup_matches_numeric_ids() {
        let text = "root:x:0:\nnogroup:x:65534:\n";
        assert_eq!(group_name_from(text, 0).as_deref(), Some("root"));
        assert_eq!(group_name_from(text, 65534).as_deref(), Some("nogroup"));
        assert_eq!(group_name_from(text, 1234), None);
    }
}
