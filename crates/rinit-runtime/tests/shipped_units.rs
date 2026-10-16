//! Every unit rinit ships has to parse (plan §1.0, §1.1).
//!
//! A unit file that does not parse is a boot failure - `plan_boot` treats it as
//! fatal, deliberately (plan §1.3(1)) - and the only other place that would
//! notice is the guest, several minutes of cross-compile away. This reads the
//! repository's `units/` through the same loader boot uses, so a typo in a
//! shipped unit fails `make check` instead of an aarch64 boot.
//!
//! The second half is narrower and on purpose: `priv.service` (the §7.4
//! de-privileging acceptance unit) relies on the tokeniser keeping a
//! double-quoted shell command in one piece, `$(...)`, `|` and `^` included.
//! That assumption is worth a test rather than a five-minute round trip, and if
//! it ever stops holding, this fails instead of the guest's assertion.

use std::path::PathBuf;

use rinit_core::unit::parser::parse;
use rinit_runtime::units::load_unit_files;

fn units_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../units")
}

#[test]
fn every_shipped_unit_parses() {
    let (files, warnings) = load_unit_files(&units_dir()).expect("load units/");
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    assert!(files.len() >= 5, "expected the shipped units to be there");

    for file in &files {
        if let Err(e) = parse(&file.name, &file.contents) {
            panic!("{} does not parse: {e}", file.path.display());
        }
    }
}

/// `priv.service` is what the guest assertion is built on, so its parsed form is
/// checked here: a change to the unit that quietly stopped de-privileging would
/// otherwise still let the unit start, just as root.
#[test]
fn the_priv_drop_unit_asks_for_everything_the_guest_checks() {
    let (files, _) = load_unit_files(&units_dir()).expect("load units/");
    let file = files
        .iter()
        .find(|f| f.name.canonical() == "priv.service")
        .expect("priv.service is shipped");

    let cfg = parse(&file.name, &file.contents).expect("priv.service parses");
    assert_eq!(cfg.user, "nobody");
    assert_eq!(cfg.group, "nogroup");
    assert!(cfg.no_new_privileges);

    let argv = cfg.exec_start.argv();
    assert_eq!(argv.first().map(String::as_str), Some("/bin/sh"));
    assert_eq!(argv.get(1).map(String::as_str), Some("-c"));
    let command = argv.get(2).expect("the shell command is one argument");
    assert!(
        command.contains("uid=$(id -u)") && command.contains("nnp=$(grep ^NoNewPrivs"),
        "the whole quoted command must survive tokenisation, got: {command:?}"
    );
}

/// The capability units the guest asserts on (plan §7.4). What matters is the
/// empty list staying distinguishable from an absent one - `CapabilityBoundingSet=`
/// is the *strictest* setting, and a parser that collapsed it into "unset" would
/// turn the assertion below into a service running as root with everything.
#[test]
fn the_capability_units_ask_for_what_the_guest_checks() {
    let (files, _) = load_unit_files(&units_dir()).expect("load units/");
    let body = |name: &str| {
        let file = files
            .iter()
            .find(|f| f.name.canonical() == name)
            .unwrap_or_else(|| panic!("{name} is shipped"));
        (
            file.name.clone(),
            file.contents.clone(),
            file.path.display().to_string(),
        )
    };

    let (name, contents, path) = body("caps.service");
    let caps = parse(&name, &contents).unwrap_or_else(|e| panic!("{path} does not parse: {e}"));
    assert_eq!(
        caps.capability_bounding_set,
        Some(Vec::new()),
        "an empty CapabilityBoundingSet= drops every capability"
    );
    assert!(caps.ambient_capabilities.is_none());
    assert_eq!(caps.user, "", "this one stays root on purpose");

    let (name, contents, path) = body("ambient.service");
    let ambient = parse(&name, &contents).unwrap_or_else(|e| panic!("{path} does not parse: {e}"));
    assert_eq!(ambient.user, "nobody");
    assert!(ambient.no_new_privileges);
    for caps in [
        ambient.capability_bounding_set.clone().expect("set"),
        ambient.ambient_capabilities.clone().expect("set"),
    ] {
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].as_str(), "net_bind_service");
    }
}
