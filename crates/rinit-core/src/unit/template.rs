//! Template units and the specifiers they expand (plan §7.7).
//!
//! A unit file whose name contains an `@` with nothing after it -
//! `worker@.service` - is a *template*: not a unit itself, but the definition
//! every instance of it is made from. `rctl start worker@1.service` is what
//! brings one into being, and the plan's rules for it are three:
//!
//! 1. the instance is the part after the `@` (`%i`), the full name is
//!    `worker@1.service` (`%n`), and the prefix is `worker` (`%p`);
//! 2. the substitution happens **once, when the instance is parsed** - there is
//!    no run-time interpolation, which is both cheaper and the reason a service
//!    can never be handed a name that changed after it started;
//! 3. an `@` with no instance cannot be started, and says so rather than starting
//!    something ambiguous.
//!
//! The specifiers are expanded over the file's whole *text* rather than over the
//! fields one by one, because that is what "at parse time" means: whatever is in
//! the file is what the parser then sees, comments and all. `%%` is a literal
//! per cent, and anything else after a `%` is an error - a template with a typo
//! must not silently hand a service the wrong argument.

use alloc::string::String;

use super::UnitName;

/// Expand the template specifiers in `text` for the instance `name`.
///
/// Returns `None` when `text` contains a `%` this build does not know: the
/// caller has the unit name and the file, and can say which one it was.
pub fn substitute(text: &str, name: &UnitName) -> Option<String> {
    let instance = name.instance()?;
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            'i' => out.push_str(instance),
            'n' => out.push_str(name.canonical()),
            'p' => out.push_str(name.prefix()),
            '%' => out.push('%'),
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(name: &str) -> UnitName {
        UnitName::parse(name).expect("valid name")
    }

    #[test]
    fn each_specifier_expands() {
        let name = unit("worker@blue.service");
        assert_eq!(
            substitute("-%i-%n-%p-", &name).as_deref(),
            Some("-blue-worker@blue.service-worker-")
        );
    }

    /// `%%` is a literal per cent, and a `%` this build does not know is an
    /// error rather than a literal: a typo in a template must not reach a
    /// service's command line as `%x`.
    #[test]
    fn a_literal_per_cent_is_doubled_and_anything_else_is_refused() {
        let name = unit("worker@1.service");
        assert_eq!(substitute("100%%", &name).as_deref(), Some("100%"));
        assert_eq!(substitute("%Y", &name), None);
        assert_eq!(substitute("trailing%", &name), None);
    }

    /// A template's own name has no instance, so there is nothing to expand - and
    /// the caller must not be able to pretend otherwise.
    #[test]
    fn a_template_without_an_instance_has_nothing_to_expand() {
        assert_eq!(substitute("%i", &unit("worker@.service")), None);
        assert_eq!(substitute("%i", &unit("plain.service")), None);
    }

    /// The instance keeps everything after the first `@`, so an instance name
    /// that itself contains one still expands as a whole.
    #[test]
    fn an_instance_with_an_at_sign_in_it_expands_as_one_name() {
        let name = unit("worker@a@b.service");
        assert_eq!(
            substitute("%i/%n", &name).as_deref(),
            Some("a@b/worker@a@b.service")
        );
    }
}
