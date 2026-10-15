//! Unit naming and typing (plan §1.3(1), §7.2).
//!
//! A unit name carries both a canonical string (always suffixed) and its
//! `UnitType`, resolved from the suffix. Names without a recognised suffix
//! default to `.service`, matching the dependency-name rule in plan §1.1.

use alloc::string::{String, ToString};
use core::fmt;

use crate::CoreError;

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
    /// `worker@1.service`), if any. Template expansion itself is M3+/P3.
    pub fn instance(&self) -> Option<&str> {
        let stem = self
            .canonical
            .strip_suffix(self.ty.suffix())
            .unwrap_or(&self.canonical);
        stem.split_once('@')
            .map(|(_, instance)| instance)
            .filter(|i| !i.is_empty())
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
}
