//! Every unit rinit ships has to parse (plan §1.0, §1.1).
//!
//! A unit file that does not parse is a boot failure - `plan_boot` treats it as
//! fatal, deliberately (plan §1.3(1)) - and the only other place that would
//! notice is the guest, several minutes of cross-compile away. This reads the
//! repository's `units/` through the same loader boot uses, so a typo in a
//! shipped unit fails `make check` instead of an aarch64 boot.
//!
//! There are two directories and they mean different things. `units/` is the
//! **boot set**: the console and the two targets, and the whole of what
//! `make run` packs. `units/demo/` is the **acceptance set**: the §7.x units the
//! guest system test starts from the console one at a time, packed only into the
//! image `make test-system` boots. `scripts/build-initramfs.sh` flattens both
//! into one `/etc/rinit/units`, so the tests read them as one directory too -
//! which is also why a name in both is an error rather than a shadowing.
//!
//! The second half is narrower and on purpose: `priv.service` (the §7.4
//! de-privileging acceptance unit) relies on the tokeniser keeping a
//! double-quoted shell command in one piece, `$(...)`, `|` and `^` included.
//! That assumption is worth a test rather than a five-minute round trip, and if
//! it ever stops holding, this fails instead of the guest's assertion.

use std::path::PathBuf;

use rinit_core::UnitName;
use rinit_core::unit::cgroup::CgroupLimits;
use rinit_core::unit::config::{RestartPolicy, ServiceKind, SizeBytes};
use rinit_core::unit::parser::parse;
use rinit_runtime::units::{UnitFile, load_unit_files};

fn units_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../units")
}

fn demo_dir() -> PathBuf {
    units_dir().join("demo")
}

/// Both directories, concatenated, as the image sees them.
fn load_shipped() -> (Vec<UnitFile>, Vec<String>) {
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    for dir in [units_dir(), demo_dir()] {
        let (mut found, mut warned) =
            load_unit_files(&dir).unwrap_or_else(|e| panic!("load {}: {e}", dir.display()));
        files.append(&mut found);
        warnings.append(&mut warned);
    }
    (files, warnings)
}

/// `units/` is what `make run` packs: the console, its alternative, and the two
/// targets. The §7.x acceptance units live in `units/demo/`, and this is what
/// keeps the run image a run image - a demo unit moved back here would ship in
/// every developer's boot with nothing that starts it.
#[test]
fn the_boot_set_is_the_console_and_its_targets() {
    let (files, warnings) = load_unit_files(&units_dir()).expect("load the boot set");
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    let mut names: Vec<&str> = files.iter().map(|f| f.name.canonical()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "getty.service",
            "multi-user.target",
            "rinit-init.target",
            "shell.service"
        ],
        "units/ is the boot set; the acceptance units belong in units/demo/"
    );
}

/// The packing script flattens both directories into one `/etc/rinit/units` and
/// refuses a duplicate name; this refuses it at `make check` time, where the
/// failure is cheaper to read than a build error minutes into `make run`.
#[test]
fn no_unit_name_is_in_both_directories() {
    let (boot, _) = load_unit_files(&units_dir()).expect("load units/");
    let (demo, warnings) = load_unit_files(&demo_dir()).expect("load units/demo/");
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    assert!(
        demo.len() >= 5,
        "units/demo/ should hold the acceptance units, found {}",
        demo.len()
    );

    for shipped in &boot {
        assert!(
            !demo
                .iter()
                .any(|d| d.name.canonical() == shipped.name.canonical()),
            "{} is in both units/ and units/demo/",
            shipped.name.canonical()
        );
    }
}

/// The template unit (plan §7.7): the guest asserts that starting
/// `worker@42.service` runs a command with `%i`, `%n` and `%p` already expanded -
/// including in `Environment=` - so this pins the file that has to expand that
/// way, and that the expansion is what parses.
#[test]
fn the_template_unit_expands_into_what_the_guest_checks() {
    let (files, _) = load_shipped();
    let file = files
        .iter()
        .find(|f| f.name.canonical() == "worker@.service")
        .expect("worker@.service is shipped");
    assert!(
        file.name.is_template(),
        "the file name is what makes it a template, and not a unit"
    );

    let instance = UnitName::parse("worker@42.service").expect("valid name");
    let expanded =
        rinit_core::substitute(&file.contents, &instance).expect("every specifier is known");
    let cfg = parse(&instance, &expanded).expect("the expansion parses");

    assert_eq!(
        cfg.exec_start.argv().last().map(String::as_str),
        Some("echo worker-instance=42 name=worker@42.service prefix=worker env=$INSTANCE"),
        "the specifiers are expanded before the parser ever sees the command"
    );
    assert_eq!(
        cfg.environment,
        vec![("INSTANCE".to_string(), "42".to_string())],
        "and in `Environment=`, which is why the demo prints both"
    );
}

#[test]
fn every_shipped_unit_parses() {
    let (files, warnings) = load_shipped();
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
    let (files, _) = load_shipped();
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
    let (files, _) = load_shipped();
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

/// The cgroup units the guest asserts on (plan §7.3).
///
/// What matters here is the same tokeniser assumption as `priv.service`: the
/// whole `sh -c` command has to survive as one argument, because it is what
/// reads the kernel's own view back. A unit file that quietly lost `$$` would
/// still start - and would report the wrong process's cgroup.
#[test]
fn the_cgroup_units_ask_for_what_the_guest_checks() {
    let (files, _) = load_shipped();
    let body = |name: &str| {
        let file = files
            .iter()
            .find(|f| f.name.canonical() == name)
            .unwrap_or_else(|| panic!("{name} is shipped"));
        parse(&file.name, &file.contents)
            .unwrap_or_else(|e| panic!("{} does not parse: {e}", file.path.display()))
    };

    let limit = body("cglimit.service");
    let limits = CgroupLimits::from_config(&limit);
    assert!(
        !limits.is_empty(),
        "cglimit.service is the unit the limits are asserted on"
    );
    let files: Vec<_> = limits
        .files()
        .into_iter()
        .map(|(_, file, value)| (file, value))
        .collect();
    assert_eq!(
        files,
        vec![
            ("cpu.max", "50000 100000".to_string()),
            ("memory.max", (32u64 * 1024 * 1024).to_string()),
            ("memory.high", (24u64 * 1024 * 1024).to_string()),
            ("pids.max", "64".to_string()),
        ],
        "the four attributes the guest reads back, in the order they are written"
    );

    let argv = limit.exec_start.argv();
    assert_eq!(argv.first().map(String::as_str), Some("/bin/sh"));
    let command = argv.get(2).expect("the shell command is one argument");
    for needle in [
        "/sys/fs/cgroup/rinit/cglimit.service",
        "self=$(cat /proc/$$/cgroup)",
        "child=$(cat /proc/self/cgroup)",
        "cpu=$(cat $d/cpu.max)",
        "mem=$(cat $d/memory.max)",
        "high=$(cat $d/memory.high)",
        "pids=$(cat $d/pids.max)",
    ] {
        assert!(
            command.contains(needle),
            "the quoted command must survive tokenisation intact, missing {needle:?} in: {command:?}"
        );
    }

    let run = body("cgrun.service");
    let run_limits = CgroupLimits::from_config(&run);
    assert_eq!(
        run_limits.memory_max,
        SizeBytes::parse("32M"),
        "a running unit, so the limits exist while it is up"
    );
    assert_eq!(run_limits.tasks_max, Some(64));
    assert_eq!(run.restart, RestartPolicy::No, "the stop has to be a stop");
    assert_eq!(
        run.exec_start.argv(),
        ["/bin/sleep", "60"],
        "a service that stays up long enough for the console to look at it"
    );

    // §7.3's OOM link: the limit that makes the cgroup's own OOM killer fire,
    // and the policy that has to restart the unit once it has. `Type=simple` is
    // part of the assertion - the shell has to be `Active` when the kill lands,
    // which is the state the restart policy is written against.
    let oom = body("oom.service");
    let oom_limits = CgroupLimits::from_config(&oom);
    assert_eq!(
        oom_limits.memory_max,
        SizeBytes::parse("16M"),
        "the limit the guest's own kernel is expected to enforce"
    );
    assert_eq!(
        oom.restart,
        RestartPolicy::OnFailure,
        "an OOM kill is the failure the unit has to come back from"
    );
    assert_eq!(oom.service_kind, ServiceKind::Simple);
    let argv = oom.exec_start.argv();
    assert_eq!(argv.first().map(String::as_str), Some("/bin/sh"));
    let command = argv.get(2).expect("the shell command is one argument");
    assert_eq!(
        command, "x=a; while :; do x=$x$x; done",
        "the doubling loop has to reach the shell exactly as written, `$` included"
    );

    // §7.10's `forking` unit: the type, the pid file it names, and the `$!` the
    // shell needs to write it. What the guest asserts is that rinit *finds* the
    // daemon through that file, so a unit whose `PIDFile=` never survived the
    // tokeniser would still start a daemon nobody could see.
    let forking = body("forking.service");
    assert_eq!(forking.service_kind, ServiceKind::Forking);
    assert_eq!(
        forking.pid_file.as_deref(),
        Some("/run/forking.pid"),
        "the file rinit reads and the file the command writes have to be one"
    );
    let argv = forking.exec_start.argv();
    assert_eq!(argv.first().map(String::as_str), Some("/bin/sh"));
    let command = argv.get(2).expect("the shell command is one argument");
    assert!(
        command.contains("echo $! > /run/forking.pid"),
        "the backgrounded daemon's pid has to reach the file, got: {command}"
    );
}
