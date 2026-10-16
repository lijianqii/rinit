//! `User=`/`Group=`/`NoNewPrivileges=` to a [`Sandbox`] (plan §7.4).
//!
//! One place resolves the names, because two things have to agree about
//! them: the process rinit starts, and the notify socket it hands that process
//! (plan §1.3(12) chowns the socket to the unit's user). Resolving them
//! separately is how a unit ends up running as `nobody:nogroup` while its
//! socket belongs to `nobody:root`.
//!
//! Nothing is read unless a unit asks for something: a unit with no `User=`, no
//! `Group=` and no `NoNewPrivileges=` resolves to [`Sandbox::inherit`] without
//! opening a file, so the common case cannot be slowed down - or failed - by a
//! user database that is unreadable or absent.

use rinit_core::unit::config::UnitConfig;
use rinit_core::unit::sandbox::Sandbox;
use rinit_core::users;
use rinit_runtime::{RuntimeError, SyscallError};

/// The user database, read for a `User=` that is a name.
const PASSWD: &str = "/etc/passwd";
/// The group database, read for the supplementary groups of a named user.
const GROUP: &str = "/etc/group";

/// The privilege drop `config` asks for, with every name resolved to an id.
///
/// # Errors
/// [`RuntimeError`] when a `User=`/`Group=` name is not in the corresponding
/// database, or the database cannot be read. Both are fatal for the unit
/// (plan §7.4): defaulting an unknown name to root would run a service as root
/// because of a typo.
pub fn resolve(config: &UnitConfig) -> Result<Sandbox, RuntimeError> {
    if asks_for_nothing(config) {
        return Ok(Sandbox::inherit());
    }
    let passwd = read(PASSWD, "read /etc/passwd")?;
    let groups = read(GROUP, "read /etc/group")?;
    resolve_from(config, &passwd, &groups)
}

/// Whether `config` asks for nothing at all, so no database has to be read.
fn asks_for_nothing(config: &UnitConfig) -> bool {
    config.user.trim().is_empty() && config.group.trim().is_empty() && !config.no_new_privileges
}

/// [`resolve`] against explicit database bodies.
///
/// Split out so the resolution *rules* - which gid a `User=` without `Group=`
/// gets, which group list a de-privileged service gets - are testable without
/// depending on the running host's user database.
pub fn resolve_from(
    config: &UnitConfig,
    passwd: &str,
    groups: &str,
) -> Result<Sandbox, RuntimeError> {
    let user = classify(config.user.trim(), passwd)?;
    let group = config.group.trim();

    // A `Group=` always wins. Without one, the group follows the user: its
    // primary gid for a name, and the number itself for a numeric `User=`
    // (which is what systemd does for an id that is not in the user database).
    let gid = if !group.is_empty() {
        match group.parse::<u32>() {
            Ok(id) => id,
            Err(_) => {
                users::group_gid(groups, group).ok_or_else(|| lookup_error("Group= lookup"))?
            }
        }
    } else {
        match &user {
            User::Root => 0,
            User::Id(id) => *id,
            User::Named { primary_gid, .. } => *primary_gid,
        }
    };

    // The group list is set only when the unit actually changes user. Leaving a
    // de-privileged service with root's supplementary groups would defeat the
    // drop, and a unit that only sets `Group=` (running as root) has no reason
    // to lose its own.
    let group_list = match &user {
        User::Root => None,
        // A numeric id has no name to look memberships up with, so the list is
        // the primary group alone. It is still a list: the point is that root's
        // groups are gone.
        User::Id(_) => Some(vec![gid]),
        User::Named { name, .. } => {
            let mut list = users::supplementary_gids(groups, name);
            // `initgroups` semantics: the primary group belongs in the list
            // even when no `/etc/group` row lists the user.
            if !list.contains(&gid) {
                list.push(gid);
            }
            Some(list)
        }
    };

    Ok(Sandbox {
        uid: match &user {
            User::Root => 0,
            User::Id(id) => *id,
            User::Named { uid, .. } => *uid,
        },
        gid,
        groups: group_list,
        no_new_privileges: config.no_new_privileges,
    })
}

/// What a `User=` value turned out to be (plan §1.1: a name, an id, or empty).
enum User {
    /// Empty: root, unchanged.
    Root,
    /// A plain number, taken as a uid.
    Id(u32),
    /// A name found in `/etc/passwd`.
    Named {
        name: String,
        uid: u32,
        primary_gid: u32,
    },
}

/// Resolve a `User=` value against a `/etc/passwd` body.
///
/// # Errors
/// `ENOENT` for a name that is not in the database.
fn classify(user: &str, passwd: &str) -> Result<User, RuntimeError> {
    if user.is_empty() {
        return Ok(User::Root);
    }
    if let Ok(id) = user.parse::<u32>() {
        return Ok(User::Id(id));
    }
    let (uid, primary_gid) =
        users::passwd_ids(passwd, user).ok_or_else(|| lookup_error("User= lookup"))?;
    Ok(User::Named {
        name: user.to_string(),
        uid,
        primary_gid,
    })
}

/// The error an unresolvable name produces.
///
/// A syscall-layer error with `ENOENT`, the shape the notify socket's own
/// lookup had before this module existed. The caller reports the unit and the
/// reason, so this carries only which key could not be resolved.
fn lookup_error(op: &'static str) -> RuntimeError {
    RuntimeError::Syscall(SyscallError::new(op, libc::ENOENT))
}

/// Read a user or group database, naming the file the failure was about.
fn read(path: &str, op: &'static str) -> Result<String, RuntimeError> {
    std::fs::read_to_string(path).map_err(|e| RuntimeError::Io { op, source: e })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rinit_core::unit::config::UnitConfig;
    use rinit_core::unit::sandbox::SandboxStep;

    const PASSWD: &str = "\
root:x:0:0:root:/root:/bin/sh
daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin
nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin
";

    const GROUP: &str = "root:x:0:\nwheel:x:10:nobody\nnogroup:x:65534:\n";

    fn config(user: &str, group: &str) -> UnitConfig {
        let name = rinit_core::UnitName::parse("t.service").expect("name");
        let mut cfg = UnitConfig::new(name);
        cfg.user = user.to_string();
        cfg.group = group.to_string();
        cfg
    }

    fn sandbox(user: &str, group: &str) -> Sandbox {
        resolve_from(&config(user, group), PASSWD, GROUP).expect("resolves")
    }

    /// The common case: nothing asked for, nothing changed - and in particular
    /// no group list, so the child keeps whatever rinit has.
    #[test]
    fn a_unit_that_asks_for_nothing_inherits() {
        let sandbox = sandbox("", "");
        assert!(sandbox.is_inherited());
        assert_eq!(sandbox, Sandbox::inherit());
    }

    /// `User=` alone means that user's *primary* group, not root's.
    #[test]
    fn a_named_user_gets_its_primary_group() {
        let sandbox = sandbox("nobody", "");
        assert_eq!(sandbox.uid, 65534);
        assert_eq!(sandbox.gid, 65534);
        assert_eq!(sandbox.groups, Some(vec![10, 65534]));
        assert!(sandbox.drops_privileges());
    }

    /// An explicit `Group=` wins over the user's primary group, and is
    /// resolvable by name or by number.
    #[test]
    fn an_explicit_group_wins() {
        let by_name = sandbox("nobody", "wheel");
        assert_eq!((by_name.uid, by_name.gid), (65534, 10));
        let by_id = sandbox("nobody", "10");
        assert_eq!((by_id.uid, by_id.gid), (65534, 10));
    }

    /// A numeric `User=` is taken as an id, and its group defaults to that same
    /// number (systemd's rule for an id that is not a name in the database).
    #[test]
    fn a_numeric_user_defaults_its_group_to_itself() {
        let sandbox = sandbox("1000", "");
        assert_eq!((sandbox.uid, sandbox.gid), (1000, 1000));
        assert_eq!(
            sandbox.groups,
            Some(vec![1000]),
            "a numeric user has no name to look memberships up with"
        );
    }

    /// `Group=` on its own changes the group but not the user, and does not
    /// strip root's supplementary groups.
    #[test]
    fn a_group_alone_keeps_the_user_and_its_group_list() {
        let sandbox = sandbox("", "nogroup");
        assert_eq!((sandbox.uid, sandbox.gid), (0, 65534));
        assert_eq!(
            sandbox.groups, None,
            "a group list is only set when the user changes too"
        );
        assert!(sandbox.drops_privileges(), "the group did change");
        assert!(!sandbox.is_inherited(), "the group step is in the plan");
        assert_eq!(
            sandbox.steps(),
            vec![SandboxStep::SetGroup],
            "and it is the only step"
        );
    }

    /// `NoNewPrivileges=true` alone is a sandbox, and it changes no ids.
    #[test]
    fn no_new_privileges_alone_is_enough() {
        let mut cfg = config("", "");
        cfg.no_new_privileges = true;
        let sandbox = resolve_from(&cfg, PASSWD, GROUP).expect("resolves");
        assert!(sandbox.no_new_privileges);
        assert_eq!((sandbox.uid, sandbox.gid), (0, 0));
        assert!(sandbox.groups.is_none());
        assert!(!sandbox.drops_privileges());
    }

    /// The primary group is in the list even when no `/etc/group` row mentions
    /// the user, which is what `initgroups` would do.
    #[test]
    fn the_primary_group_is_always_in_the_list() {
        let sandbox = sandbox("daemon", "");
        assert_eq!((sandbox.uid, sandbox.gid), (1, 1));
        assert_eq!(sandbox.groups, Some(vec![1]));
    }

    /// An unknown name fails closed: it must not quietly become root. The error
    /// is the same one the notify socket's lookup produced (`User= lookup`,
    /// `ENOENT`), so both callers can report it the same way.
    #[test]
    fn an_unknown_name_is_an_error() {
        let cases = [
            ("nobodyyy", "", "User= lookup"),
            ("nobody", "wheelx", "Group= lookup"),
        ];
        for (user, group, expected) in cases {
            match resolve_from(&config(user, group), PASSWD, GROUP) {
                Err(RuntimeError::Syscall(e)) => {
                    assert_eq!(e.op(), expected);
                    assert_eq!(e.errno(), libc::ENOENT);
                }
                other => panic!("expected a {expected} failure, got: {other:?}"),
            }
        }
    }

    /// A missing database is an error too, not a silent root.
    #[test]
    fn an_empty_database_fails_closed() {
        assert!(resolve_from(&config("nobody", ""), "", "").is_err());
    }
}
