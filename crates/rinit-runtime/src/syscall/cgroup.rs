//! cgroup v2 file operations (plan §7.3).
//!
//! The cgroup directory is a *parameter* here, not a constant. Production
//! passes paths under `/sys/fs/cgroup`; the tests below pass a directory of
//! ordinary files, which is what makes the write order, the pid placement and
//! the cleanup rules checkable on a developer host with no cgroup hierarchy, no
//! root and no `/sys`.
//!
//! What the files are: attributes, written whole and last-write-wins. The write
//! is an `O_WRONLY|O_TRUNC` open of a file that must already exist - *not*
//! `std::fs::write`, which adds `O_CREAT`. On cgroupfs the difference is
//! invisible (kernfs cannot create files, so `O_CREAT` does nothing there), but
//! it matters for what the code means: an attribute that is not there - because
//! its controller was never enabled - has to be reported as a failed write, not
//! turned into a file that then accepts the value and enforces nothing. It is
//! also what lets the tests below stand in for cgroupfs faithfully, since an
//! ordinary directory *can* create files.
//!
//! No policy lives here. Whether a skipped limit is a warning or a failed start,
//! and whether a `rmdir` that returns `EBUSY` matters, are plan §7.3 decisions
//! and belong to the caller (`rinit`'s `cgroup` module).

use std::io::Write;
use std::path::Path;

#[cfg(test)]
use std::path::PathBuf;

use crate::error::SyscallError;

/// The file that exists in every cgroup of a real v2 hierarchy, and in no other
/// mount of that name.
///
/// Plan §7.3 makes exactly this file the availability test ("`cgroup.controllers`
/// 不存在时降级"), and it is the right one: its presence is what distinguishes a
/// unified v2 hierarchy from a v1 mount or an empty directory.
const CONTROLLERS: &str = "cgroup.controllers";

/// Where the unified hierarchy is mounted (plan §1.3(9) item 6, plan §7.3).
///
/// The first-wave mounts put it there, and the cgroup code then treats it as
/// the directory it configures - one constant, so the two cannot drift.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// The file that turns a controller on for a cgroup's *children*.
const SUBTREE_CONTROL: &str = "cgroup.subtree_control";

/// Writing a pid here moves that process into the cgroup.
const PROCS: &str = "cgroup.procs";

/// `populated <0|1>` here says whether the cgroup still has live members.
const EVENTS: &str = "cgroup.events";

/// Whether `dir` is a cgroup of the unified (v2) hierarchy.
///
/// A read failure of any kind - missing, permission, not a directory - means
/// "not usable", which is what the caller needs; the reason is not worth
/// distinguishing, because the response is the same warning either way.
pub fn is_v2(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(CONTROLLERS)).is_ok()
}

/// The controllers available to `dir`, as the kernel lists them.
///
/// One space-separated line, e.g. `cpuset cpu io memory pids`.
pub fn read_controllers(dir: &Path) -> Result<String, SyscallError> {
    let path = dir.join(CONTROLLERS);
    std::fs::read_to_string(&path).map_err(|e| SyscallError::from_io("cgroup.controllers", &e))
}

/// Enable controllers (`"+cpu +memory"`) for `dir`'s children.
///
/// # Errors
/// The kernel refuses this - `ENOENT` for a controller it does not have,
/// `EBUSY` when the cgroup has processes of its own - and the caller decides
/// what that means.
pub fn enable_controllers(dir: &Path, line: &str) -> Result<(), SyscallError> {
    write_file(&dir.join(SUBTREE_CONTROL), line, "cgroup.subtree_control")
}

/// Create `dir` (and its parents) if it does not exist.
pub fn create(dir: &Path) -> Result<(), SyscallError> {
    std::fs::create_dir_all(dir).map_err(|e| SyscallError::from_io("cgroup mkdir", &e))
}

/// Write one limit file inside `dir` (e.g. `memory.max` = `33554432`).
pub fn write_limit(dir: &Path, file: &str, value: &str) -> Result<(), SyscallError> {
    // Naming the attribute is what makes a failed start diagnosable; the names
    // are the closed set `CgroupLimits::files()` produces.
    let op: &'static str = match file {
        "cpu.max" => "cgroup cpu.max",
        "memory.max" => "cgroup memory.max",
        "memory.high" => "cgroup memory.high",
        "pids.max" => "cgroup pids.max",
        _ => "cgroup limit",
    };
    write_file(&dir.join(file), value, op)
}

/// Move `pid` into `dir` by writing it to `cgroup.procs`.
///
/// The kernel moves the process and everything it has already forked, and any
/// process it forks afterwards is born in the same cgroup (plan §7.3(3)).
pub fn place_process(dir: &Path, pid: libc::pid_t) -> Result<(), SyscallError> {
    write_file(&dir.join(PROCS), &pid.to_string(), "cgroup.procs")
}

/// The pids currently in `dir`, as `cgroup.procs` lists them.
///
/// Tokens that are not pids are skipped rather than turned into an error: this
/// is read on the stop path, where being unable to parse one line must not
/// leave the rest of a process tree unsignalled.
pub fn processes(dir: &Path) -> Result<Vec<libc::pid_t>, SyscallError> {
    let path = dir.join(PROCS);
    let body =
        std::fs::read_to_string(&path).map_err(|e| SyscallError::from_io("cgroup.procs", &e))?;
    Ok(body
        .split_whitespace()
        .filter_map(|token| token.parse().ok())
        .collect())
}

/// How many bytes a pid needs as decimal text (`i32::MIN` is 11 characters).
const PID_TEXT_MAX: usize = 11;

/// Put *this* process into the cgroup whose `cgroup.procs` is `procs`.
///
/// This is [`place_process`] from the other side, and it exists because of when
/// it runs: in the child, between `fork` and `exec` (plan §7.3's "fork first,
/// place the pid, and only then drop privileges"). The child is the process
/// that has to do it, because after the `setuid` in plan §7.4 it no longer owns
/// anything in `/sys/fs/cgroup` and could not write this file - and doing it
/// here is what closes the window a parent-side write cannot: the parent only
/// learns the spawn succeeded once the child has `exec`'d, by which time the
/// service could already have forked something that escaped the limits.
///
/// It is deliberately not `place_process`: a process that exists only between
/// `fork` and `exec` must not allocate, and `std::fs` builds a `PathBuf`, an
/// `io::Error` and a formatted `String` on the way. This is one `open`, one
/// `write` and one `close`, with the pid rendered into a stack buffer.
///
/// # Errors
/// The raw errno, which the caller reports to the parent the same way it
/// reports a failed `setuid`.
pub fn join_from_child(procs: &std::ffi::CStr, pid: libc::pid_t) -> Result<(), i32> {
    let mut text = [0u8; PID_TEXT_MAX];
    let len = decimal(pid, &mut text);

    // SAFETY: `procs` is a valid NUL-terminated path, and `O_WRONLY` with no
    // `O_CREAT` opens the attribute the parent created (see the module docs).
    let fd = unsafe { libc::open(procs.as_ptr(), libc::O_WRONLY) };
    if fd < 0 {
        return Err(last_errno());
    }
    // SAFETY: `fd` is open for writing and `text[..len]` is initialised.
    let written = unsafe { libc::write(fd, text.as_ptr().cast(), len) };
    let mut errno = if written < 0 { last_errno() } else { 0 };
    if errno == 0 && written as usize != len {
        // A partial write would place a different pid.
        errno = libc::EIO;
    }
    // SAFETY: `fd` was opened above and is not used again; without this the
    // service would inherit an open descriptor to its own `cgroup.procs`.
    unsafe { libc::close(fd) };

    if errno == 0 { Ok(()) } else { Err(errno) }
}

/// Render `pid` as decimal text into the front of `buf`, returning its length.
///
/// Hand-rolled because the caller cannot allocate; `snprintf` would work but
/// drags locale machinery into a process that must stay predictable between
/// `fork` and `exec`. The digits are written from the back of the buffer
/// forwards, which needs no index arithmetic and cannot run out: a `pid_t` is at
/// most ten digits, and `buf` holds eleven.
fn decimal(pid: libc::pid_t, buf: &mut [u8; PID_TEXT_MAX]) -> usize {
    let mut value = u64::from(pid.unsigned_abs());
    let mut written = 0usize;
    for slot in buf.iter_mut().rev() {
        *slot = b'0' + (value % 10) as u8;
        value /= 10;
        written += 1;
        if value == 0 {
            break;
        }
    }
    let start = buf.len() - written;
    buf.copy_within(start.., 0);
    written
}

/// The current `errno`, defaulting to `EIO` if the platform reports none.
fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

/// Whether `dir` still has members, from `cgroup.events`.
///
/// # Errors
/// [`SyscallError`] with `EINVAL` when the file exists but does not carry the
/// `populated` line. The caller would otherwise have to guess, and guessing
/// "empty" here is guessing that a process is dead.
pub fn is_populated(dir: &Path) -> Result<bool, SyscallError> {
    let path = dir.join(EVENTS);
    let body =
        std::fs::read_to_string(&path).map_err(|e| SyscallError::from_io("cgroup.events", &e))?;
    for line in body.lines() {
        if let Some(value) = line.strip_prefix("populated") {
            return match value.trim() {
                "0" => Ok(false),
                "1" => Ok(true),
                _ => Err(SyscallError::new("cgroup.events populated", libc::EINVAL)),
            };
        }
    }
    Err(SyscallError::new("cgroup.events populated", libc::EINVAL))
}

/// `memory.events` carries the memory controller's counters as `key value`
/// lines; `oom_kill` is the one plan §7.3 needs.
const MEMORY_EVENTS: &str = "memory.events";

/// How many times the cgroup OOM killer has killed a process in `dir`.
///
/// Plan §7.3 asks for the OOM kill to be *detected* rather than assumed from the
/// signal, and this is the only place the kernel records one: `oom_kill` in
/// `memory.events` counts kills the memory controller performed in this cgroup
/// (as opposed to the system-wide OOM killer, which says nothing about a unit).
/// A `SIGKILL` is also how a stop timeout and an external `kill -9` look, so the
/// counter is what tells them apart.
///
/// `Ok(None)` is the answer for both halves of "this kernel does not report it":
/// no `memory.events` at all (no `CONFIG_MEMCG`) and no `oom_kill` line in one
/// that exists. Neither is an error - a unit with a `CgroupMemoryMax=` has to
/// keep working on a kernel that cannot report OOM kills, exactly as
/// [`crate::syscall::cgroup`]'s limit writes tolerate a missing attribute - and
/// neither is `0`, which would claim "no kills yet" about a file nobody read.
///
/// # Errors
/// A file that exists and cannot be read (permissions, I/O). A malformed value
/// reads as absent rather than as an error: the counter is diagnostic, and a
/// kernel that prints something unexpected should not turn a unit's death into
/// a second failure.
pub fn oom_kills(dir: &Path) -> Result<Option<u64>, SyscallError> {
    let path = dir.join(MEMORY_EVENTS);
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(SyscallError::from_io("cgroup memory.events", &e)),
    };
    Ok(body.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        match (fields.next(), fields.next()) {
            (Some("oom_kill"), Some(value)) => value.parse().ok(),
            _ => None,
        }
    }))
}

/// Remove `dir`.
///
/// # Errors
/// A cgroup that still has processes fails this with `EBUSY` (and an ordinary
/// directory with `ENOTEMPTY`); the caller treats both as "keep the directory,
/// warn" rather than as a failed stop (plan §7.3's `rmdir` rule).
pub fn remove(dir: &Path) -> Result<(), SyscallError> {
    std::fs::remove_dir(dir).map_err(|e| SyscallError::from_io("cgroup rmdir", &e))
}

/// Write `value` to `path`, mapping the failure to `op`.
///
/// The file must already exist (see the module docs): a cgroup attribute is
/// either there because its controller is enabled, or absent because it is not,
/// and those two must not look alike. `write_all` returns only after the kernel
/// accepted the whole value; `File::flush` is a no-op and would not add a
/// guarantee.
fn write_file(path: &Path, value: &str, op: &'static str) -> Result<(), SyscallError> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|e| SyscallError::from_io(op, &e))?;
    // The attribute parser in the kernel reads the whole buffer; a partial write
    // would be a truncated number, so a short write is an error, not a retry.
    file.write_all(value.as_bytes())
        .map_err(|e| SyscallError::from_io(op, &e))
}

/// A throwaway directory for a test, removed on drop.
#[cfg(test)]
struct Scratch(PathBuf);

#[cfg(test)]
impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("rinit-cgroup-{}-{tag}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Scratch(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// A subdirectory of the scratch, created for the caller.
    fn subdir(&self, name: &str) -> PathBuf {
        let dir = self.0.join(name);
        std::fs::create_dir_all(&dir).expect("subdir");
        dir
    }
}

#[cfg(test)]
impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plan's availability test is the controllers file: an empty directory
    /// is not a v2 hierarchy, and one with the file is.
    #[test]
    fn the_controllers_file_is_what_makes_a_directory_a_v2_hierarchy() {
        let scratch = Scratch::new("is-v2");
        assert!(!is_v2(scratch.path()), "an empty directory is not a cgroup");

        std::fs::write(scratch.path().join(CONTROLLERS), "cpu memory pids\n").unwrap();
        assert!(is_v2(scratch.path()));

        // A directory that is not there at all is equally unusable.
        assert!(!is_v2(&scratch.path().join("nope")));
    }

    /// The controller list is read verbatim, so the pure intersection in
    /// `rinit-core` sees exactly what the kernel wrote.
    #[test]
    fn the_controller_list_is_read_back_verbatim() {
        let scratch = Scratch::new("controllers");
        std::fs::write(scratch.path().join(CONTROLLERS), "cpu memory pids\n").unwrap();
        assert_eq!(
            read_controllers(scratch.path()).unwrap(),
            "cpu memory pids\n"
        );
    }

    /// Enabling writes the caller's line into `cgroup.subtree_control`; the line
    /// itself is `rinit-core`'s business.
    #[test]
    fn enabling_writes_the_line_to_subtree_control() {
        let scratch = Scratch::new("enable");
        let dir = scratch.path();
        assert_eq!(
            enable_controllers(dir, "+cpu")
                .expect_err("no file yet")
                .errno(),
            libc::ENOENT,
            "a missing control file is reported, not created"
        );

        std::fs::write(dir.join(SUBTREE_CONTROL), "").unwrap();
        enable_controllers(dir, "+cpu +memory").expect("enable");
        assert_eq!(
            std::fs::read_to_string(dir.join(SUBTREE_CONTROL)).unwrap(),
            "+cpu +memory"
        );
    }

    /// A limit lands in the file named for it, as the kernel would read it.
    #[test]
    fn a_limit_is_written_to_its_own_file() {
        let scratch = Scratch::new("limit");
        let dir = scratch.subdir("rinit");
        std::fs::write(dir.join("memory.max"), "max").unwrap();

        write_limit(&dir, "memory.max", "33554432").expect("write");
        assert_eq!(
            std::fs::read_to_string(dir.join("memory.max")).unwrap(),
            "33554432"
        );
    }

    /// An attribute that is not there - the controller was never enabled - must
    /// stay absent and report `ENOENT`; creating a file that accepts the value
    /// and enforces nothing is the one outcome that must not happen.
    #[test]
    fn a_write_to_an_absent_attribute_is_enoent_and_creates_nothing() {
        let scratch = Scratch::new("absent");
        let dir = scratch.subdir("rinit");

        let err = write_limit(&dir, "pids.max", "64").expect_err("must fail");
        assert_eq!(err.errno(), libc::ENOENT);
        assert!(
            !dir.join("pids.max").exists(),
            "the write must not manufacture the attribute"
        );
    }

    /// A write into a cgroup that was never created must fail with the real
    /// errno: that is the difference between "the limit applied" and "the unit
    /// is running unbounded".
    #[test]
    fn a_limit_write_into_a_missing_cgroup_reports_enoent() {
        let scratch = Scratch::new("missing");
        let err = write_limit(&scratch.path().join("rinit/x.service"), "cpu.max", "1 2")
            .expect_err("must fail");
        assert_eq!(err.errno(), libc::ENOENT);
        assert_eq!(err.op(), "cgroup cpu.max");
    }

    /// Placement is a decimal pid, nothing else - the kernel parses this file as
    /// a number, and a trailing newline is not part of the value.
    #[test]
    fn placing_a_process_writes_the_pid_in_decimal() {
        let scratch = Scratch::new("place");
        let dir = scratch.subdir("rinit");
        std::fs::write(dir.join(PROCS), "").unwrap();

        place_process(&dir, 4321).expect("place");
        assert_eq!(std::fs::read_to_string(dir.join(PROCS)).unwrap(), "4321");
    }

    /// Reading the member list must survive the shapes a real file has: a
    /// trailing newline, blank lines, and (defensively) junk.
    #[test]
    fn the_process_list_is_parsed_leniently() {
        let scratch = Scratch::new("processes");
        let dir = scratch.subdir("rinit");

        std::fs::write(dir.join(PROCS), "101\n202\n").unwrap();
        assert_eq!(processes(&dir).unwrap(), vec![101, 202]);

        std::fs::write(dir.join(PROCS), "101\n\n 202 \nnot-a-pid\n").unwrap();
        assert_eq!(
            processes(&dir).unwrap(),
            vec![101, 202],
            "unparsable tokens are skipped, not fatal"
        );

        std::fs::write(dir.join(PROCS), "").unwrap();
        assert!(processes(&dir).unwrap().is_empty());
    }

    /// `populated` is the exit confirmation plan §7.3 asks for, so `1` and `0`
    /// must be distinguished and anything else must be an error rather than a
    /// guess.
    #[test]
    fn populated_is_read_from_the_events_file() {
        let scratch = Scratch::new("events");
        let dir = scratch.subdir("rinit");

        std::fs::write(dir.join(EVENTS), "populated 1\nfrozen 0\n").unwrap();
        assert!(is_populated(&dir).unwrap());

        std::fs::write(dir.join(EVENTS), "populated 0\n").unwrap();
        assert!(!is_populated(&dir).unwrap());

        std::fs::write(dir.join(EVENTS), "populated maybe\n").unwrap();
        assert_eq!(
            is_populated(&dir).expect_err("unparsable").errno(),
            libc::EINVAL
        );

        std::fs::write(dir.join(EVENTS), "frozen 0\n").unwrap();
        assert_eq!(
            is_populated(&dir).expect_err("no populated line").errno(),
            libc::EINVAL
        );

        std::fs::remove_file(dir.join(EVENTS)).unwrap();
        assert_eq!(
            is_populated(&dir).expect_err("missing").errno(),
            libc::ENOENT
        );
    }

    /// The OOM counter is the one thing that separates a cgroup OOM kill from
    /// any other `SIGKILL`, so it has to be read out of `memory.events` - and
    /// its absence has to be distinguishable from a `0`.
    #[test]
    fn an_oom_kill_is_read_out_of_the_memory_events_file() {
        let scratch = Scratch::new("oom-events");
        let dir = scratch.subdir("rinit");

        std::fs::write(
            dir.join(MEMORY_EVENTS),
            "low 0\nhigh 3\nmax 1\noom 1\noom_kill 2\n",
        )
        .unwrap();
        assert_eq!(oom_kills(&dir).unwrap(), Some(2));

        // A kernel without `CONFIG_MEMCG` has no file to read...
        std::fs::remove_file(dir.join(MEMORY_EVENTS)).unwrap();
        assert_eq!(oom_kills(&dir).unwrap(), None);

        // ...and one with the controller but a different kernel version may not
        // report this key. Neither may read as "no kills yet".
        std::fs::write(dir.join(MEMORY_EVENTS), "low 0\nhigh 0\nmax 0\n").unwrap();
        assert_eq!(oom_kills(&dir).unwrap(), None);

        // A counter is diagnostic: a value that does not parse is absent, not a
        // second failure on top of the unit's death.
        std::fs::write(dir.join(MEMORY_EVENTS), "oom_kill many\n").unwrap();
        assert_eq!(oom_kills(&dir).unwrap(), None);

        // A file that is there and unreadable *is* an error, so the caller can
        // tell "this kernel does not report it" from "I could not look".
        let io = scratch.subdir("io");
        std::fs::create_dir(io.join(MEMORY_EVENTS)).unwrap();
        assert!(oom_kills(&io).is_err());
    }

    /// Cleanup removes the directory, and fails on a directory that still has
    /// anything in it - the condition the caller must tolerate as a warning.
    #[test]
    fn removing_a_group_removes_it_and_refuses_a_full_one() {
        let scratch = Scratch::new("remove");
        let empty = scratch.subdir("empty");
        remove(&empty).expect("empty cgroup removes");
        assert!(!empty.exists());

        let full = scratch.subdir("full");
        std::fs::write(full.join(PROCS), "1").unwrap();
        let err = remove(&full).expect_err("a cgroup with members must not vanish");
        assert_eq!(
            err.errno(),
            libc::ENOTEMPTY,
            "an ordinary directory reports ENOTEMPTY where cgroupfs reports EBUSY"
        );
        assert!(
            full.exists(),
            "the directory is kept, as plan §7.3 requires"
        );
    }

    /// The two halves of placement, end to end: the child writes its own pid
    /// (`join_from_child`), and the list the parent reads sees it. This is
    /// plan §7.3's "把 fork 出的 PID 写入 cgroup.procs", with the writing done
    /// from the side that still owns the file.
    #[test]
    fn joining_from_the_child_writes_our_pid() {
        let scratch = Scratch::new("join");
        let dir = scratch.subdir("rinit");
        std::fs::write(dir.join(PROCS), "").unwrap();
        let procs = std::ffi::CString::new(dir.join(PROCS).to_str().unwrap()).unwrap();

        // SAFETY: `getpid` takes no arguments and never fails.
        let pid = unsafe { libc::getpid() };
        join_from_child(&procs, pid).expect("join");
        assert_eq!(processes(&dir).unwrap(), vec![pid]);
    }

    /// A failed join must come back as the real errno: a service that silently
    /// did not join would run unbounded while its unit file says otherwise.
    #[test]
    fn joining_from_the_child_reports_a_missing_attribute() {
        let scratch = Scratch::new("join-missing");
        let procs = scratch.path().join("rinit/x.service").join(PROCS);
        let procs = std::ffi::CString::new(procs.to_str().unwrap()).unwrap();

        assert_eq!(
            join_from_child(&procs, 1).expect_err("must fail"),
            libc::ENOENT
        );
    }

    /// The rendering has to be plain decimal with no sign, padding or newline,
    /// since the kernel parses this file as a number.
    #[test]
    fn pids_render_as_plain_decimal() {
        for (pid, expected) in [
            (0i32, "0"),
            (1, "1"),
            (9, "9"),
            (10, "10"),
            (4321, "4321"),
            (i32::MAX, "2147483647"),
        ] {
            let mut buf = [0u8; PID_TEXT_MAX];
            let len = decimal(pid, &mut buf);
            assert_eq!(
                std::str::from_utf8(&buf[..len]).unwrap(),
                expected,
                "pid {pid}"
            );
        }
    }
}
