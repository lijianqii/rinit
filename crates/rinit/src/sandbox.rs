//! `User=`/`Group=`/`NoNewPrivileges=`/capabilities to a [`Sandbox`] (plan §7.4).
//!
//! One place resolves the names, because two things have to agree about
//! them: the process rinit starts, and the notify socket it hands that process
//! (plan §1.3(12) chowns the socket to the unit's user). Resolving them
//! separately is how a unit ends up running as `nobody:nogroup` while its
//! socket belongs to `nobody:root`.
//!
//! Nothing is read unless a unit has a name to look up: a unit with no `User=`
//! and no `Group=` resolves without opening a file, so the common case - and a
//! unit that only narrows its capabilities - cannot be slowed down, or failed,
//! by a user database that is unreadable or absent.

use rinit_core::unit::config::{Cap, UnitConfig};
use rinit_core::unit::sandbox::Sandbox;
use rinit_core::users;
use rinit_runtime::syscall::cred;
use rinit_runtime::{RuntimeError, SyscallError};

/// The user database, read for a `User=` that is a name.
const PASSWD: &str = "/etc/passwd";
/// The group database, read for the supplementary groups of a named user.
const GROUP: &str = "/etc/group";

/// The privilege drop `config` asks for, with every name resolved to an id.
///
/// # Errors
/// [`RuntimeError`] when a `User=`/`Group=` name is not in the corresponding
/// database, when a capability name is not one this build knows, or when a
/// database has to be read and cannot be. All are fatal for the unit (plan
/// §7.4): defaulting an unknown name to root, or an unknown capability to "no
/// capability", would both be a service running with something other than what
/// its unit file asked for.
pub fn resolve(config: &UnitConfig) -> Result<Sandbox, RuntimeError> {
    if !needs_user_database(config) {
        // No name to look up: `resolve_from` needs no bodies, and a
        // capability-only unit does not depend on `/etc/passwd` existing.
        return resolve_from(config, "", "");
    }
    let passwd = read(PASSWD, "read /etc/passwd")?;
    let groups = read(GROUP, "read /etc/group")?;
    resolve_from(config, &passwd, &groups)
}

/// Whether `config` names a user or a group, i.e. whether `/etc/passwd` and
/// `/etc/group` have to be read at all.
fn needs_user_database(config: &UnitConfig) -> bool {
    !config.user.trim().is_empty() || !config.group.trim().is_empty()
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
        bounding_set: resolve_caps(&config.capability_bounding_set, "CapabilityBoundingSet=")?,
        ambient: resolve_caps(&config.ambient_capabilities, "AmbientCapabilities=")?,
    })
}

/// Resolve a capability directive's names to numbers, or fail closed.
///
/// The names arrive normalised by `rinit_core::unit::config::Cap` (lower-case,
/// `CAP_` stripped); the table that turns them into numbers lives in
/// `rinit_runtime::syscall::cred`, which is where plan §7.4 puts it, and an
/// unknown name is an error rather than a silently narrower set - a unit that
/// asked to keep `NET_BIND_SERVICE` and lost it to a typo would fail at
/// `bind()`, far from the unit file that caused it.
fn resolve_caps(
    names: &Option<Vec<Cap>>,
    op: &'static str,
) -> Result<Option<Vec<u32>>, RuntimeError> {
    let Some(names) = names else {
        return Ok(None);
    };
    let mut numbers = Vec::with_capacity(names.len());
    for name in names {
        let number = cred::capability_number(name.as_str()).ok_or_else(|| lookup_error(op))?;
        numbers.push(number);
    }
    Ok(Some(numbers))
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

    /// A config with capability directives and nothing else.
    fn capability_config(bounding: Option<&[&str]>, ambient: Option<&[&str]>) -> UnitConfig {
        let mut cfg = config("", "");
        cfg.capability_bounding_set = bounding.map(|names| {
            names
                .iter()
                .map(|n| Cap::parse(n).expect("valid capability name"))
                .collect()
        });
        cfg.ambient_capabilities = ambient.map(|names| {
            names
                .iter()
                .map(|n| Cap::parse(n).expect("valid capability name"))
                .collect()
        });
        cfg
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

    /// Capability names become numbers here, and nowhere else: the model keeps
    /// the validated name, the runtime owns the table (plan §7.4).
    #[test]
    fn capabilities_resolve_to_numbers() {
        let cfg = capability_config(
            Some(&["NET_BIND_SERVICE", "CAP_SYS_ADMIN"]),
            Some(&["net_bind_service"]),
        );
        let sandbox = resolve_from(&cfg, PASSWD, GROUP).expect("resolves");
        assert_eq!(sandbox.bounding_set, Some(vec![10, 21]));
        assert_eq!(sandbox.ambient, Some(vec![10]));
        assert!(
            !sandbox.is_inherited(),
            "a capability-only sandbox still needs the fork path"
        );
    }

    /// The three spellings of one capability are one capability - the model
    /// normalises, the table is keyed by the normalised name, and this is the
    /// test that fails if those two ever disagree.
    #[test]
    fn the_same_capability_has_one_number_however_it_is_written() {
        for spelling in [
            "NET_BIND_SERVICE",
            "CAP_NET_BIND_SERVICE",
            "net_bind_service",
        ] {
            let cfg = capability_config(Some(&[spelling]), None);
            let sandbox = resolve_from(&cfg, PASSWD, GROUP).expect("resolves");
            assert_eq!(sandbox.bounding_set, Some(vec![10]), "spelling: {spelling}");
        }
    }

    /// An empty `CapabilityBoundingSet=` is the strictest setting plan §7.4
    /// offers, not an absence of one: it drops every capability.
    #[test]
    fn an_empty_bounding_set_is_a_step_not_an_absence() {
        let cfg = capability_config(Some(&[]), None);
        let sandbox = resolve_from(&cfg, PASSWD, GROUP).expect("resolves");
        assert_eq!(sandbox.bounding_set, Some(Vec::new()));
        assert!(!sandbox.is_inherited());
        assert_eq!(sandbox.steps(), vec![SandboxStep::SetBoundingSet]);

        // Unset is the other thing entirely.
        let unset = resolve_from(&config("", ""), PASSWD, GROUP).expect("resolves");
        assert_eq!(unset.bounding_set, None);
        assert!(unset.is_inherited());
    }

    /// A capability name this build does not know fails the unit, naming the
    /// directive that asked for it. A service that silently lost the capability
    /// it asked to keep would fail later, at `bind()`, with nothing pointing
    /// back at the unit file.
    #[test]
    fn an_unknown_capability_fails_closed() {
        let cases = [
            (Some(&["bogus"][..]), None, "CapabilityBoundingSet="),
            (None, Some(&["bogus"][..]), "AmbientCapabilities="),
        ];
        for (bounding, ambient, expected) in cases {
            match resolve_from(&capability_config(bounding, ambient), PASSWD, GROUP) {
                Err(RuntimeError::Syscall(e)) => {
                    assert_eq!(e.op(), expected);
                    assert_eq!(e.errno(), libc::ENOENT);
                }
                other => panic!("expected a {expected} failure, got: {other:?}"),
            }
        }
    }

    /// A unit that only narrows capabilities goes through `resolve`, the entry
    /// point - not through a fast path that would drop them - and needs no
    /// `/etc/passwd`: it has no name to look up.
    #[test]
    fn a_capability_only_unit_keeps_them_and_needs_no_user_database() {
        let cfg = capability_config(Some(&["net_bind_service"]), Some(&["net_bind_service"]));
        let sandbox = resolve(&cfg).expect("a capability-only unit resolves");
        assert_eq!(sandbox.bounding_set, Some(vec![10]));
        assert_eq!(sandbox.ambient, Some(vec![10]));
        assert_eq!((sandbox.uid, sandbox.gid), (0, 0), "no ids were asked for");
    }
}
