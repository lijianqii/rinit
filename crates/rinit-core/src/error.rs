//! Core error types (plan §2.3 "错误层次").
//!
//! `CoreError` models pure-logic failures only: parsing, dependency resolution
//! and state-machine violations. It never carries a syscall errno - that is the
//! job of `rinit-runtime::SyscallError`.

use alloc::string::String;
use core::fmt;

/// An error produced by platform-independent rinit logic.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CoreError {
    /// A unit name was empty or contained characters outside the permitted set.
    InvalidUnitName(String),
    /// A unit file did not satisfy the format or the directive contract.
    Parse(crate::unit::parser::ParseError),
    /// The dependency graph could not be built or scheduled.
    Dependency(crate::unit::dependency::DependencyError),
    /// An event was not legal in the unit's current state.
    Transition(crate::unit::state::TransitionError),
    // M2+ adds: State(StateError).
}

impl From<crate::unit::parser::ParseError> for CoreError {
    fn from(e: crate::unit::parser::ParseError) -> Self {
        CoreError::Parse(e)
    }
}

impl From<crate::unit::dependency::DependencyError> for CoreError {
    fn from(e: crate::unit::dependency::DependencyError) -> Self {
        CoreError::Dependency(e)
    }
}

impl From<crate::unit::state::TransitionError> for CoreError {
    fn from(e: crate::unit::state::TransitionError) -> Self {
        CoreError::Transition(e)
    }
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoreError::InvalidUnitName(name) => {
                write!(f, "invalid unit name: {name:?}")
            }
            CoreError::Parse(e) => write!(f, "{e}"),
            CoreError::Dependency(e) => write!(f, "{e}"),
            CoreError::Transition(e) => write!(f, "{e}"),
        }
    }
}

impl core::error::Error for CoreError {}
