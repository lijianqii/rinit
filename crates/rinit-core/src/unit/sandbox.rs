//! The privilege drop a service gets before `exec` (plan §7.4).
//!
//! What lives here is the *plan*: which steps, in which order. The syscalls
//! that carry them out need a kernel and belong to `rinit-runtime`, but the
//! order is the part plan §7.4 calls order-sensitive, and keeping it as data
//! makes it a unit test rather than something only root on a real system can
//! check.
//!
//! The order, and why it is that order:
//!
//! 1. `PR_SET_NO_NEW_PRIVS` first: it is the one step that cannot fail for lack
//!    of a privilege, and it should be in place before the process has any
//!    chance to be talked into running something else.
//! 2. `setgid`, then the supplementary group list, then `setuid` last.
//!    Everything after `setuid` runs without what the remaining steps need - an
//!    unprivileged process cannot change its group list, and per plan §7.4
//!    cannot adjust its own capability sets either - so `setuid` is the point
//!    of no return and goes last.
//!
//! A sandbox with no steps is the common case, and it stays on `posix_spawn`:
//! only a unit that actually asks to be de-privileged pays for `fork`.

use alloc::vec::Vec;

/// One step of a privilege drop, in the order plan §7.4 requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SandboxStep {
    /// `prctl(PR_SET_NO_NEW_PRIVS, 1)`. After this, nothing the service or its
    /// children execute can gain privileges, including `setuid` binaries.
    NoNewPrivileges,
    /// `setgid(2)`: the primary group.
    SetGroup,
    /// `setgroups(2)`: the supplementary group list.
    SetGroups,
    /// `setuid(2)`: the user, and the last step for the reason in this module's
    /// header.
    SetUser,
}

/// How a unit's process should be set up between `fork` and `exec`.
///
/// Every field is independent: a unit with only `NoNewPrivileges=` set gets
/// only that step, and one that changes ids without touching its group list is
/// not given a group list it did not ask for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sandbox {
    /// The user to become. `0` means "stay root", which is also what an unset
    /// `User=` resolves to.
    pub uid: u32,
    /// The primary group to become. `0` means "stay root's".
    pub gid: u32,
    /// Supplementary groups.
    ///
    /// `None` leaves the child's group list alone; `Some(vec![])` sets an empty
    /// one. The distinction matters: a service dropped to an unprivileged user
    /// must not keep root's supplementary groups, which is what a plain
    /// `setuid` would leave it with.
    pub groups: Option<Vec<u32>>,
    /// Whether to deny privilege escalation to the service and its children.
    pub no_new_privileges: bool,
}

impl Sandbox {
    /// A sandbox that changes nothing: the child runs as rinit does.
    pub fn inherit() -> Self {
        Sandbox::default()
    }

    /// The steps this sandbox needs, in the order they must run.
    ///
    /// Empty for a sandbox that does not change anything, which is how the
    /// caller knows it can keep using `posix_spawn`.
    pub fn steps(&self) -> Vec<SandboxStep> {
        let mut steps = Vec::new();
        if self.no_new_privileges {
            steps.push(SandboxStep::NoNewPrivileges);
        }
        if self.gid != 0 {
            steps.push(SandboxStep::SetGroup);
        }
        if self.groups.is_some() {
            steps.push(SandboxStep::SetGroups);
        }
        if self.uid != 0 {
            steps.push(SandboxStep::SetUser);
        }
        steps
    }

    /// Whether the child runs exactly as rinit does, so no `fork` is needed.
    pub fn is_inherited(&self) -> bool {
        self.steps().is_empty()
    }

    /// Whether the child ends up as a different user or group than rinit.
    ///
    /// `groups` alone does not count: changing the group list of an
    /// otherwise-root process is not a privilege drop.
    pub fn drops_privileges(&self) -> bool {
        self.uid != 0 || self.gid != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn an_inheriting_sandbox_does_nothing() {
        let sandbox = Sandbox::inherit();
        assert_eq!(sandbox.steps(), Vec::new());
        assert!(sandbox.is_inherited());
        assert!(!sandbox.drops_privileges());
    }

    /// The whole point of the module: `setuid` is last, and `no_new_privs` is
    /// first, whatever combination of settings produced the plan.
    #[test]
    fn the_order_is_the_one_plan_74_prescribes() {
        let sandbox = Sandbox {
            uid: 65534,
            gid: 65534,
            groups: Some(vec![65534, 10]),
            no_new_privileges: true,
        };
        assert_eq!(
            sandbox.steps(),
            vec![
                SandboxStep::NoNewPrivileges,
                SandboxStep::SetGroup,
                SandboxStep::SetGroups,
                SandboxStep::SetUser,
            ]
        );
    }

    /// A unit may ask for any subset, and gets exactly that subset.
    #[test]
    fn only_the_requested_steps_appear() {
        let only_nnp = Sandbox {
            no_new_privileges: true,
            ..Sandbox::inherit()
        };
        assert_eq!(only_nnp.steps(), vec![SandboxStep::NoNewPrivileges]);
        assert!(!only_nnp.is_inherited());
        assert!(!only_nnp.drops_privileges(), "ids did not change");

        let only_user = Sandbox {
            uid: 1000,
            ..Sandbox::inherit()
        };
        assert_eq!(only_user.steps(), vec![SandboxStep::SetUser]);
        assert!(only_user.drops_privileges());
    }

    /// `User=` without `Group=` gives the manager a group list to set, but the
    /// primary gid is an *independent* step: a `Group=` on its own must still
    /// take effect.
    #[test]
    fn the_group_list_can_change_without_the_primary_group() {
        let sandbox = Sandbox {
            groups: Some(Vec::new()),
            ..Sandbox::inherit()
        };
        assert_eq!(sandbox.steps(), vec![SandboxStep::SetGroups]);
        assert!(
            !sandbox.drops_privileges(),
            "clearing a root process's groups does not change its ids"
        );
    }

    /// An empty group list is a step, not an absence of one - that is the
    /// difference between "clear the groups" and "leave root's groups alone".
    #[test]
    fn an_empty_group_list_still_clears_it() {
        let cleared = Sandbox {
            uid: 1000,
            groups: Some(Vec::new()),
            ..Sandbox::inherit()
        };
        let untouched = Sandbox {
            uid: 1000,
            ..Sandbox::inherit()
        };
        assert_eq!(cleared.steps().len(), untouched.steps().len() + 1);
        assert!(cleared.steps().contains(&SandboxStep::SetGroups));
        assert!(!untouched.steps().contains(&SandboxStep::SetGroups));
    }

    /// `setuid` after `setgid` and the group list, checked structurally so a
    /// step inserted in a later milestone cannot silently land after the point
    /// of no return.
    #[test]
    fn setuid_comes_after_every_step_it_depends_on() {
        let sandbox = Sandbox {
            uid: 1000,
            gid: 1000,
            groups: Some(vec![1000]),
            no_new_privileges: true,
        };
        let steps = sandbox.steps();
        let user = steps
            .iter()
            .position(|s| *s == SandboxStep::SetUser)
            .expect("SetUser must be in the plan");
        assert_eq!(user, steps.len() - 1, "setuid must be the last step");
        for other in [SandboxStep::SetGroup, SandboxStep::SetGroups] {
            let at = steps.iter().position(|s| *s == other).expect("step present");
            assert!(at < user, "{other:?} must precede setuid");
        }
        assert_eq!(steps.first(), Some(&SandboxStep::NoNewPrivileges));
    }
}
