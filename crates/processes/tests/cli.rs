//! End-to-end tests against a fake `/proc` tree.
//!
//! The binary has `--proc-root` precisely so the reader, filters, tree builder
//! and renderers can be exercised without depending on the host's live process
//! set.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!("processes-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stat_line(pid: u32, ppid: u32, comm: &str) -> String {
    // Fields after `)`: state, ppid, ... utime(11), stime(12), ... starttime(19),
    // vsize(20), rss(21). The values in between are placeholders; the parser
    // only consumes those positions.
    format!("{pid} ({comm}) S {ppid} 1 1 0 -1 0 0 0 0 0 1 2 0 0 20 0 1 0 0 1048576 4\n")
}

fn write_process(root: &Path, pid: u32, ppid: u32, name: &str, args: &[u8]) {
    let dir = root.join(pid.to_string());
    std::fs::create_dir_all(&dir).expect("process directory");
    std::fs::write(dir.join("stat"), stat_line(pid, ppid, name)).expect("stat");
    std::fs::write(
        dir.join("status"),
        format!("Name:\t{name}\nUid:\t0\t0\t0\t0\nThreads:\t1\nVmSwap:\t0 kB\n"),
    )
    .expect("status");
    if !args.is_empty() {
        std::fs::write(dir.join("cmdline"), args).expect("cmdline");
    }
}

fn tree(tag: &str) -> TempDir {
    let root = TempDir::new(&format!("tree-{tag}"));
    std::fs::write(root.path().join("uptime"), "100.0 50.0\n").expect("uptime");
    write_process(root.path(), 1, 0, "init", b"init\0--one\0");
    write_process(root.path(), 2, 1, "child", b"child\0");
    std::fs::create_dir_all(root.path().join("not-a-pid")).expect("non-pid");
    std::fs::create_dir_all(root.path().join("3")).expect("bad process");
    std::fs::write(root.path().join("3/stat"), "this is not a stat line\n").expect("bad stat");
    root
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_processes"))
        .arg("--proc-root")
        .arg(root)
        .args(args)
        .output()
        .expect("run processes")
}

#[test]
fn a_fake_proc_tree_renders_table_tree_and_json() {
    let root = tree("table");

    let output = run(root.path(), &["--no-header"]);
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("init"), "{text}");
    assert!(text.contains("child"), "{text}");
    assert!(!text.contains("not-a-pid"), "{text}");

    let output = run(root.path(), &["--tree", "--ascii", "--no-header"]);
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("+- child"), "{text}");

    let output = run(root.path(), &["--json", "--pid=1"]);
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("\"pid\":1"), "{text}");
    assert!(text.contains("init --one"), "{text}");
    assert!(!text.contains("\"pid\":2"), "{text}");
}

/// A parent/child cycle has no root. The tree walk must terminate through its
/// visited set rather than recurse forever or invent a root.
#[test]
fn a_ppid_cycle_does_not_hang_the_tree() {
    let root = TempDir::new("cycle");
    std::fs::write(root.path().join("uptime"), "100.0 50.0\n").expect("uptime");
    write_process(root.path(), 4, 5, "four", b"four\0");
    write_process(root.path(), 5, 4, "five", b"five\0");

    let output = run(root.path(), &["--tree", "--no-header"]);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
}

#[test]
fn filters_and_limits_are_applied_before_rendering() {
    let root = tree("filter");

    let output = run(root.path(), &["--no-header", "--pid=2"]);
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("child"), "{text}");
    assert!(!text.contains("init"), "{text}");

    let output = run(root.path(), &["--no-header", "--limit=0"]);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
}

#[test]
fn a_missing_proc_root_is_a_reported_error() {
    let output = run(Path::new("/no/such/proc"), &[]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        text.contains("No such file") || text.contains("not found"),
        "{text}"
    );
}
