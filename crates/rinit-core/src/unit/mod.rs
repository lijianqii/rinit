//! Unit naming and typing (plan §1.3(1), §7.2).
//!
//! This module owns the *identity* of a unit: its canonical, always-suffixed
//! name and the [`UnitType`] resolved from that suffix. Names without a
//! recognised suffix default to `.service`, matching the dependency-name rule
//! in plan §1.1.
//!
//! Submodules (plan §1.2):
//!
//! - [`config`] — the parsed unit-file data model (plan §1.1).
//! - [`parser`] — the `&str -> Result<UnitConfig, _>` unit-file parser
//!   (plan §1.0 grammar).
//! - [`dependency`] — dependency DAG, topological order, cycle detection
//!   (plan §1.3(1)).
//! - [`sandbox`] — the ordered privilege-drop plan for a service (plan §7.4).
//! - [`cgroup`] — the cgroup v2 limits a unit asks for, and the files they
//!   become (plan §7.3).
//! - [`template`] — template units and the `%i`/`%n`/`%p` specifiers
//!   (plan §7.7).

pub mod cgroup;
pub mod config;
pub mod dependency;
pub mod job;
pub mod notify;
pub mod parser;
pub mod restart;
pub mod sandbox;
pub mod state;
pub mod template;

use alloc::string::{String, ToString};
use core::fmt;

use crate::CoreError;

pub use cgroup::{CgroupLimits, controllers_to_enable, has_controller};
pub use config::{
    Cap, CpuQuota, Dependencies, ExecCommand, RestartPolicy, ServiceKind, SizeBytes, UnitConfig,
};
pub use dependency::{DependencyError, DependencyGraph};
pub use job::{Job, JobQueue, JobTarget, RequestSource};
pub use notify::{NotifyError, NotifyMessage, parse as parse_notify};
pub use parser::{ParseError, parse as parse_unit_file, parse_environment_file};
pub use restart::{BackoffState, Jitter};
pub use sandbox::{Sandbox, SandboxStep};
pub use state::{Effect, Event, ExitOutcome, FailedReason, TransitionError, UnitState, transition};
pub use template::substitute;

/// The kind of a unit (plan §1.1 `[Unit] Type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UnitType {
    /// A service: has a process lifetime managed by rinit.
    Service,
    /// A target: a process-less aggregation point in the dependency graph.
    Target,
}

impl UnitType {
    /// The canonical filename suffix for this unit type.
    pub const fn suffix(self) -> &'static str {
        match self {
            UnitType::Service => ".service",
            UnitType::Target => ".target",
        }
    }
}

impl fmt::Display for UnitType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            UnitType::Service => "service",
            UnitType::Target => "target",
        })
    }
}

/// A validated unit name with its resolved type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnitName {
    canonical: String,
    ty: UnitType,
}

impl UnitName {
    /// Parse and validate a unit name.
    ///
    /// A name ending in `.service` / `.target` keeps that suffix; any other
    /// name is treated as a service and canonicalised with a `.service` suffix
    /// (plan §1.1 "依赖名解析"). An empty name, an empty stem, or a name with
    /// characters outside `[A-Za-z0-9:_.@-]` is rejected.
    ///
    /// Suffix matching is exact and case-sensitive: `foo.SERVICE` is *not* a
    /// recognised suffix and therefore canonicalises to `foo.SERVICE.service`.
    /// This matches the plan's simplified rules rather than systemd's
    /// case-insensitive lookup, and keeping it explicit avoids surprise names.
    pub fn parse(name: &str) -> Result<Self, CoreError> {
        let name = name.trim();
        if !is_valid_unit_name(name) {
            return Err(CoreError::InvalidUnitName(name.to_string()));
        }

        let (canonical, ty) = if let Some(stem) = name.strip_suffix(UnitType::Service.suffix()) {
            if stem.is_empty() {
                return Err(CoreError::InvalidUnitName(name.to_string()));
            }
            (name.to_string(), UnitType::Service)
        } else if let Some(stem) = name.strip_suffix(UnitType::Target.suffix()) {
            if stem.is_empty() {
                return Err(CoreError::InvalidUnitName(name.to_string()));
            }
            (name.to_string(), UnitType::Target)
        } else {
            let mut canonical = name.to_string();
            canonical.push_str(UnitType::Service.suffix());
            (canonical, UnitType::Service)
        };

        Ok(UnitName { canonical, ty })
    }

    /// The canonical, always-suffixed name (e.g. `"net.target"`).
    pub fn canonical(&self) -> &str {
        &self.canonical
    }

    /// The resolved unit type.
    pub const fn unit_type(&self) -> UnitType {
        self.ty
    }

    /// The instance part of a template instance (e.g. `"1"` for
    /// `worker@1.service`), if any (plan §7.7).
    ///
    /// Only the *first* `@` separates the template from the instance, so
    /// `worker@a@b.service` has instance `"a@b"`. A template's own name
    /// (`worker@.service`) has no instance: that is what makes it a template.
    pub fn instance(&self) -> Option<&str> {
        self.stem()
            .split_once('@')
            .map(|(_, instance)| instance)
            .filter(|i| !i.is_empty())
    }

    /// Whether this name *is* a template: a name with an `@` and no instance,
    /// like `worker@.service` (plan §7.7).
    pub fn is_template(&self) -> bool {
        self.stem().contains('@') && self.instance().is_none()
    }

    /// The template an instance came from: `worker@1.service` → `worker@.service`
    /// (plan §7.7).
    ///
    /// `None` for a template itself and for a name with no `@` at all, which are
    /// the two things that are not instances of anything.
    pub fn template(&self) -> Option<UnitName> {
        let (prefix, instance) = self.stem().split_once('@')?;
        if instance.is_empty() {
            return None;
        }
        let mut canonical = String::with_capacity(self.canonical.len());
        canonical.push_str(prefix);
        canonical.push('@');
        canonical.push_str(self.ty.suffix());
        UnitName::parse(&canonical).ok()
    }

    /// The part before the `@` (or the whole stem), which is what `%p` expands
    /// to (plan §7.7).
    pub fn prefix(&self) -> &str {
        self.stem()
            .split_once('@')
            .map_or(self.stem(), |(prefix, _)| prefix)
    }

    /// The canonical name without its type suffix.
    fn stem(&self) -> &str {
        self.canonical
            .strip_suffix(self.ty.suffix())
            .unwrap_or(&self.canonical)
    }
}

impl fmt::Display for UnitName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical)
    }
}

/// Validate a raw unit name string against `[A-Za-z0-9:_.@-]`.
fn is_valid_unit_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    name.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'.' | b'@' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::vec::Vec;

    #[test]
    fn parses_service_with_suffix() {
        let u = UnitName::parse("network.service").unwrap();
        assert_eq!(u.canonical(), "network.service");
        assert_eq!(u.unit_type(), UnitType::Service);
        assert_eq!(u.instance(), None);
    }

    #[test]
    fn parses_target_with_suffix() {
        let u = UnitName::parse("multi-user.target").unwrap();
        assert_eq!(u.canonical(), "multi-user.target");
        assert_eq!(u.unit_type(), UnitType::Target);
    }

    #[test]
    fn defaults_bare_name_to_service() {
        let u = UnitName::parse("dbus").unwrap();
        assert_eq!(u.canonical(), "dbus.service");
        assert_eq!(u.unit_type(), UnitType::Service);
    }

    #[test]
    fn trims_surrounding_whitespace() {
        let u = UnitName::parse("  app.service  ").unwrap();
        assert_eq!(u.canonical(), "app.service");
    }

    #[test]
    fn rejects_empty_and_emptystem() {
        assert_eq!(
            UnitName::parse(""),
            Err(CoreError::InvalidUnitName(String::new()))
        );
        assert!(UnitName::parse(".service").is_err());
        assert!(UnitName::parse(".target").is_err());
    }

    #[test]
    fn rejects_illegal_characters() {
        assert!(UnitName::parse("has space").is_err());
        assert!(UnitName::parse("path/evil").is_err());
        assert!(UnitName::parse("weird=thing").is_err());
    }

    #[test]
    fn extracts_template_instance() {
        let u = UnitName::parse("worker@1.service").unwrap();
        assert_eq!(u.instance(), Some("1"));
        let bare = UnitName::parse("worker@.service").unwrap();
        assert_eq!(bare.instance(), None);
    }

    // --- boundary conditions -------------------------------------------

    /// Trimming can empty the name entirely; the empty check must run *after*
    /// the trim, otherwise `"   "` would sail through.
    #[test]
    fn rejects_whitespace_only_name() {
        assert!(UnitName::parse("   ").is_err());
        assert!(UnitName::parse("\t\n ").is_err());
    }

    /// Only the trailing suffix counts: a dotted name such as
    /// `foo.service.service` keeps its (double) suffix rather than being
    /// re-canonicalised.
    #[test]
    fn keeps_double_suffix_verbatim() {
        let u = UnitName::parse("foo.service.service").unwrap();
        assert_eq!(u.canonical(), "foo.service.service");
        assert_eq!(u.unit_type(), UnitType::Service);
    }

    /// Suffix matching is case-sensitive, so an upper-case suffix is just part
    /// of the stem and the name gains a `.service` suffix.
    #[test]
    fn upper_case_suffix_is_not_recognised() {
        let u = UnitName::parse("foo.SERVICE").unwrap();
        assert_eq!(u.canonical(), "foo.SERVICE.service");
        assert_eq!(u.unit_type(), UnitType::Service);
    }

    /// Plan §7.7's three name questions: whether a name is a template, which
    /// template an instance came from, and what `%p` expands to.
    #[test]
    fn templates_and_their_instances_are_told_apart() {
        let instance = UnitName::parse("worker@1.service").unwrap();
        assert!(!instance.is_template());
        assert_eq!(
            instance.template().map(|t| t.canonical().to_string()),
            Some("worker@.service".to_string())
        );
        assert_eq!(instance.prefix(), "worker");

        let template = UnitName::parse("worker@.service").unwrap();
        assert!(template.is_template());
        assert_eq!(template.template(), None, "a template is not an instance");
        assert_eq!(template.prefix(), "worker", "and still knows its prefix");

        let plain = UnitName::parse("worker.service").unwrap();
        assert!(!plain.is_template());
        assert_eq!(plain.template(), None);
        assert_eq!(plain.prefix(), "worker");

        // A target template is the same question about a different suffix.
        let target = UnitName::parse("check@a.target").unwrap();
        assert_eq!(
            target.template().map(|t| t.canonical().to_string()),
            Some("check@.target".to_string())
        );
    }

    /// `split_once('@')` keeps everything after the first `@` as the instance.
    #[test]
    fn instance_splits_on_first_at_sign() {
        let u = UnitName::parse("worker@a@b.service").unwrap();
        assert_eq!(u.instance(), Some("a@b"));
    }

    /// Targets carry instances too; the suffix used for stripping is the
    /// target's own, not a hard-coded `.service`.
    #[test]
    fn extracts_instance_from_target_name() {
        let u = UnitName::parse("slice@2.target").unwrap();
        assert_eq!(u.unit_type(), UnitType::Target);
        assert_eq!(u.instance(), Some("2"));
    }

    /// Every character allowed by the spec is accepted.
    #[test]
    fn accepts_full_permitted_charset() {
        let u = UnitName::parse("aZ0:_.@-").unwrap();
        assert_eq!(u.canonical(), "aZ0:_.@-.service");
    }

    /// Non-ASCII bytes are outside `[A-Za-z0-9:_.@-]` and must be rejected.
    #[test]
    fn rejects_non_ascii() {
        assert!(UnitName::parse("服务").is_err());
        assert!(UnitName::parse("café.service").is_err());
    }

    // --- `Display` (both impls were uncovered) -------------------------

    #[test]
    fn unit_name_displays_canonical_form() {
        assert_eq!(UnitName::parse("dbus").unwrap().to_string(), "dbus.service");
        assert_eq!(
            UnitName::parse("net.target").unwrap().to_string(),
            "net.target"
        );
    }

    #[test]
    fn unit_type_displays_bare_word() {
        assert_eq!(UnitType::Service.to_string(), "service");
        assert_eq!(UnitType::Target.to_string(), "target");
        assert_eq!(format!("{}", UnitType::Target), "target");
    }

    /// The error path is user-facing; its message must name the offending
    /// input so a bad unit file is debuggable.
    #[test]
    fn invalid_name_error_is_informative() {
        let err = UnitName::parse("bad name").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("invalid unit name"), "got: {msg}");
        assert!(msg.contains("bad name"), "got: {msg}");
    }

    // --- ordering / hashing (DAG determinism depends on these) --------

    /// A bare name and its explicit form are the same unit.
    #[test]
    fn bare_and_suffixed_names_are_equal() {
        assert_eq!(
            UnitName::parse("dbus").unwrap(),
            UnitName::parse("dbus.service").unwrap()
        );
    }

    /// `Ord` is derived from the canonical string, so sorting is deterministic
    /// and reproducible - the property topological sort relies on to emit a
    /// stable startup order.
    #[test]
    fn ordering_is_lexicographic_on_canonical_form() {
        let mut names: Vec<UnitName> = ["c.service", "a.service", "b.service"]
            .iter()
            .map(|n| UnitName::parse(n).unwrap())
            .collect();
        names.sort();
        let sorted: Vec<&str> = names.iter().map(UnitName::canonical).collect();
        assert_eq!(sorted, ["a.service", "b.service", "c.service"]);
    }

    /// Equality is the contract M1's unit registry relies on to collapse the
    /// `Requires=dbus` and `Requires=dbus.service` spellings of one unit into a
    /// single node. (Hashing is not exercised here: `alloc` has no hash-map
    /// collection, and deriving `Hash` alongside `Eq` is a stdlib guarantee.)
    #[test]
    fn equal_names_collapse_in_a_sequence() {
        let a = UnitName::parse("app").unwrap();
        let b = UnitName::parse("app.service").unwrap();
        let c = UnitName::parse("other").unwrap();
        assert_eq!(a, b, "both spellings name the same unit");
        assert_ne!(a, c);

        let mut names: Vec<&UnitName> = Vec::new();
        for name in [&a, &b, &c, &a] {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        assert_eq!(names.len(), 2, "duplicates must collapse: {names:?}");
    }
}

/// Property tests (plan §4.1/§4.4). The unit parser is the input boundary for
/// everything in M1, so the invariants that must hold for *any* input are
/// checked by construction rather than by example.
#[cfg(test)]
mod proptests {
    use alloc::format;
    use alloc::string::String;
    use alloc::vec;

    use proptest::prelude::*;

    use super::*;

    /// Alphabet restricted to the characters a unit name may contain, so the
    /// generator mostly produces *valid* names instead of mostly-invalid ones.
    fn valid_stem() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                Just(String::from("a")),
                Just(String::from("Z")),
                Just(String::from("7")),
                Just(String::from(":")),
                Just(String::from("_")),
                Just(String::from(".")),
                Just(String::from("@")),
                Just(String::from("-")),
            ],
            1..24,
        )
        .prop_map(|chars| chars.into_iter().collect())
    }

    // Parsing must never panic, whatever the input (plan §4.1: property tests
    // over random input must not panic).
    proptest! {
        #[test]
        fn parse_never_panics(raw in ".{0,64}") {
            let _ = UnitName::parse(&raw);
        }
    }

    // A name built from the permitted alphabet always parses, and always ends
    // with its own type's suffix.
    proptest! {
        #[test]
        fn permitted_alphabet_always_parses(stem in valid_stem()) {
            let name = UnitName::parse(&stem)
                .unwrap_or_else(|e| panic!("{stem:?} should parse, got {e}"));
            prop_assert_eq!(name.canonical(), format!("{stem}.service"));
            prop_assert_eq!(name.unit_type(), UnitType::Service);
        }
    }

    // Canonicalisation is idempotent: re-parsing a canonical name is a no-op.
    // Without this, `Requires=` resolution would keep growing the suffix.
    proptest! {
        #[test]
        fn canonicalisation_is_idempotent(stem in valid_stem()) {
            let once = UnitName::parse(&stem).unwrap();
            let twice = UnitName::parse(once.canonical()).unwrap();
            prop_assert_eq!(&once, &twice);
        }
    }

    // Parsing is stable under surrounding whitespace.
    proptest! {
        #[test]
        fn surrounding_whitespace_is_irrelevant(stem in valid_stem(), pad in 0..4usize) {
            let spaced = format!("{}{}{}", " ".repeat(pad), stem, "\t".repeat(pad));
            prop_assert_eq!(&UnitName::parse(&spaced).unwrap(), &UnitName::parse(&stem).unwrap());
        }
    }

    // A name containing a character outside `[A-Za-z0-9:_.@-]` is rejected,
    // rather than being silently accepted or truncated.
    //
    // Note the deliberate omission of whitespace: `parse` trims surrounding
    // whitespace first, so `"app "` is *valid*. That is covered separately by
    // `surrounding_whitespace_is_irrelevant`; here we only use characters that
    // trimming cannot remove.
    proptest! {
        #[test]
        fn forbidden_characters_are_always_rejected(
            stem in valid_stem(),
            bad in prop::sample::select(vec!['/', '=', '\0', '#', '*', '%', '$', '&'])
        ) {
            let candidate = format!("{}{}", stem, bad);
            let parsed = UnitName::parse(&candidate);
            prop_assert!(
                parsed.is_err(),
                "{:?} must be rejected because of {:?}",
                candidate,
                bad
            );
        }
    }
}
