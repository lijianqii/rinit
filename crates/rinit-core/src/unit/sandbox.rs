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
//! 2. The capability work that needs `CAP_SETPCAP`, then `setgid`, then the
//!    supplementary group list, then `setuid` last. Everything after `setuid`
//!    runs without what the remaining steps need - an unprivileged process
//!    cannot change its group list or its bounding set - so `setuid` is the
//!    point of no return for all of them.
//! 3. The ambient raise last of all, *after* `setuid`. This is where the plan's
//!    own wording has to give way to the kernel: §7.4 lists "`capset()` to set
//!    the bounding/ambient sets" as one step before `setgid`, but
//!    `cap_emulate_setxuid` (`security/commoncap.c`) clears the ambient set on
//!    every root-to-non-root `setuid`, `PR_SET_KEEPCAPS` or not - so an ambient
//!    capability raised earlier would be gone by `exec`. The bounding set still
//!    goes early, which is the part §7.4's rationale ("after `setuid` there may
//!    be no `CAP_SETPCAP` left to adjust") is about.
//!
//! The capability steps are also ordered against each other: the inheritable
//! set has to grow before the bounding set shrinks, because `cap_capset` only
//! accepts an inheritable bit inside `old_inheritable | old_bset`.
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
    /// `prctl(PR_SET_KEEPCAPS, 1)`: keep the permitted set across the `setuid`
    /// that the ambient raise has to happen after.
    KeepCapabilities,
    /// `capset(2)` with the requested capabilities added to the inheritable set,
    /// which an ambient capability has to be in before it can be raised.
    SetInheritable,
    /// `prctl(PR_CAPBSET_DROP, …)` for every capability the unit did not keep.
    /// What a root process is left with after `exec` is its bounding set, so
    /// this is what makes an empty `CapabilityBoundingSet=` mean "none".
    SetBoundingSet,
    /// `setgid(2)`: the primary group.
    SetGroup,
    /// `setgroups(2)`: the supplementary group list.
    SetGroups,
    /// `setuid(2)`: the user, and the point of no return for everything above
    /// but [`SandboxStep::RaiseAmbient`].
    SetUser,
    /// `prctl(PR_CAP_AMBIENT, …)`: grant (or clear) the ambient set, which the
    /// kernel only lets survive from after the `setuid` transition.
    RaiseAmbient,
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
    /// The capabilities to keep in the bounding set, by number.
    ///
    /// `None` leaves the bounding set alone; `Some(vec![])` - an empty
    /// `CapabilityBoundingSet=` - drops every capability, the strictest setting
    /// plan §7.4 offers. Numbers rather than names because the name table needs
    /// `std` and lives in `rinit-runtime`; the caller resolves them once, the
    /// same way it resolves `User=`.
    pub bounding_set: Option<Vec<u32>>,
    /// The capabilities to raise in the ambient set, by number.
    ///
    /// `None` leaves the ambient set alone; `Some(vec![])` clears it. Ambient
    /// capabilities are how a *non-root* service keeps a capability across
    /// `exec` (plan §7.4's `NET_BIND_SERVICE` example), which is why this is the
    /// one step that runs after `setuid`.
    pub ambient: Option<Vec<u32>>,
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
        // Both of these need `CAP_SETPCAP`, and the inheritable set has to grow
        // while the bounding set is still wide enough to allow it.
        if self.ambient.is_some() {
            steps.push(SandboxStep::KeepCapabilities);
            steps.push(SandboxStep::SetInheritable);
        }
        if self.bounding_set.is_some() {
            steps.push(SandboxStep::SetBoundingSet);
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
        // After the uid change, deliberately: the kernel clears the ambient set
        // on a root-to-non-root `setuid` (see this module's header).
        if self.ambient.is_some() {
            steps.push(SandboxStep::RaiseAmbient);
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
            ..Sandbox::inherit()
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
            ..Sandbox::inherit()
        };
        let steps = sandbox.steps();
        let user = steps
            .iter()
            .position(|s| *s == SandboxStep::SetUser)
            .expect("SetUser must be in the plan");
        assert_eq!(user, steps.len() - 1, "setuid must be the last step");
        for other in [SandboxStep::SetGroup, SandboxStep::SetGroups] {
            let at = steps
                .iter()
                .position(|s| *s == other)
                .expect("step present");
            assert!(at < user, "{other:?} must precede setuid");
        }
        assert_eq!(steps.first(), Some(&SandboxStep::NoNewPrivileges));
    }

    /// A unit that asks for everything gets every step - and the capability
    /// steps are split around `setuid`, which is the part of plan §7.4 the
    /// kernel rewrites.
    #[test]
    fn every_step_appears_in_kernel_order() {
        let sandbox = Sandbox {
            uid: 65534,
            gid: 65534,
            groups: Some(vec![65534]),
            no_new_privileges: true,
            bounding_set: Some(vec![10]),
            ambient: Some(vec![10]),
        };
        assert_eq!(
            sandbox.steps(),
            vec![
                SandboxStep::NoNewPrivileges,
                SandboxStep::KeepCapabilities,
                SandboxStep::SetInheritable,
                SandboxStep::SetBoundingSet,
                SandboxStep::SetGroup,
                SandboxStep::SetGroups,
                SandboxStep::SetUser,
                SandboxStep::RaiseAmbient,
            ]
        );
    }

    /// The kernel's constraints, asserted structurally: everything that needs a
    /// privilege precedes `setuid`, only the ambient raise follows it, and the
    /// inheritable set grows while the bounding set is still wide enough.
    #[test]
    fn capability_steps_sit_on_the_side_of_setuid_the_kernel_requires() {
        let sandbox = Sandbox {
            uid: 65534,
            gid: 65534,
            groups: Some(vec![65534]),
            no_new_privileges: true,
            bounding_set: Some(vec![10]),
            ambient: Some(vec![10]),
        };
        let steps = sandbox.steps();
        let at = |step: SandboxStep| {
            steps
                .iter()
                .position(|s| *s == step)
                .unwrap_or_else(|| panic!("{step:?} must be in the plan"))
        };

        let user = at(SandboxStep::SetUser);
        for step in [
            SandboxStep::KeepCapabilities,
            SandboxStep::SetInheritable,
            SandboxStep::SetBoundingSet,
            SandboxStep::SetGroup,
            SandboxStep::SetGroups,
        ] {
            assert!(
                at(step) < user,
                "{step:?} needs a privilege setuid gives up, so it must precede it"
            );
        }

        assert!(
            at(SandboxStep::RaiseAmbient) > user,
            "the kernel clears the ambient set on setuid, so the raise must follow it"
        );
        assert!(
            at(SandboxStep::SetInheritable) < at(SandboxStep::SetBoundingSet),
            "cap_capset accepts an inheritable bit only while the bounding set allows it"
        );
    }

    /// Capabilities alone still need the `fork`: neither a bounding set drop nor
    /// an ambient raise is something `posix_spawn` can be asked for.
    #[test]
    fn capabilities_alone_take_the_fork_path() {
        let bounding = Sandbox {
            bounding_set: Some(Vec::new()),
            ..Sandbox::inherit()
        };
        assert_eq!(bounding.steps(), vec![SandboxStep::SetBoundingSet]);
        assert!(
            !bounding.is_inherited(),
            "an empty bounding set is the strictest sandbox, not an absent one"
        );

        let ambient = Sandbox {
            ambient: Some(vec![10]),
            ..Sandbox::inherit()
        };
        assert_eq!(
            ambient.steps(),
            vec![
                SandboxStep::KeepCapabilities,
                SandboxStep::SetInheritable,
                SandboxStep::RaiseAmbient,
            ]
        );

        let cleared = Sandbox {
            ambient: Some(Vec::new()),
            ..Sandbox::inherit()
        };
        assert_eq!(
            cleared.steps(),
            ambient.steps(),
            "an empty ambient list is a clear, which is a step like any other"
        );
    }
}
