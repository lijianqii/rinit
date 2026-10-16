#!/usr/bin/env bash
# System test: boot rinit as PID 1 in qemu and assert on the serial console.
#
# Usage:
#   scripts/system-test.sh [--arch aarch64|x86_64] [--kernel IMAGE] [--initrd FILE]
#                          [--timeout SECONDS] [--log FILE] [--keep-log]
#
# What it does:
#   1. boots the guest from its unit directory (units/multi-user.target), which
#      starts getty.service and gives an interactive root login;
#   2. types at that login to prove a passwordless root shell is reachable,
#      following the console prompts rather than guessing at delays;
#   3. kills that console service and checks rinit restarts it (M2);
#   4. captures the serial console to a log file;
#   5. asserts the M0-M5 acceptance list on that log;
#   6. reports kernel panics / oopses as failures.
#
# `--keep-log` keeps the log even when every assertion passes (it is always
# kept on failure).
#
# Exit codes:
#   0  all assertions passed
#   1  an assertion failed (the log path is printed)
#   2  bad usage / missing artifacts
#   3  qemu itself could not be started
#
# This is the automated form of the manual verification in plan §4.3. It
# needs qemu-system-aarch64 and prebuilt artifacts (see the Makefile's
# `test-system` target, which builds them first when needed).
set -euo pipefail

arch="aarch64"
kernel=""
initrd=""
timeout_s=60
log_file=""
keep_log=0
# No --service by default: from M1 the guest boots the unit directory, and
# passing a service on the kernel cmdline would override it and skip exactly
# the behaviour this test exists to check.
service=""

usage() { sed -n '2,22p' "$0"; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch) arch="${2:?}"; shift 2 ;;
        --kernel) kernel="${2:?}"; shift 2 ;;
        --initrd) initrd="${2:?}"; shift 2 ;;
        --timeout) timeout_s="${2:?}"; shift 2 ;;
        --log) log_file="${2:?}"; shift 2 ;;
        --service) service="${2:?}"; shift 2 ;;
        --keep-log) keep_log=1; shift ;;
        -h | --help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

: "${kernel:?--kernel is required}"
: "${initrd:?--initrd is required}"

for f in "${kernel}" "${initrd}"; do
    if [[ ! -f "${f}" ]]; then
        echo "error: missing artifact ${f}" >&2
        echo "hint: run 'make initramfs kernel' to build it." >&2
        exit 2
    fi
done

if [[ -z "${log_file}" ]]; then
    mkdir -p "${repo_root}/build/logs"
    log_file="${repo_root}/build/logs/system-test-${arch}.log"
fi
mkdir -p "$(dirname "${log_file}")"

qemu_bin="qemu-system-${arch}"
if ! command -v "${qemu_bin}" >/dev/null 2>&1; then
    echo "error: ${qemu_bin} not found in PATH" >&2
    exit 3
fi

echo "== rinit system test (${arch}) =="
echo "   kernel: ${kernel}"
echo "   initrd: ${initrd}"
echo "   log:    ${log_file}"
echo "   booting (timeout ${timeout_s}s, the guest is killed once assertions are satisfiable)..."

# The console shell reads from the serial port, so the test has to *type* at
# it to prove a shell is actually reachable. A here-doc piped straight into
# qemu would be consumed long before the guest boots, so the commands are fed
# through a FIFO on a delay instead.
#
# The delay has to clear the whole boot (kernel + mounts + plan + units); 14s
# is comfortable on the aarch64 virt machine and short enough not to dominate
# the run. If a future image boots much slower this becomes a flake, so the
# final `exit` is what guarantees the guest releases the pipe.
fifo_dir="$(mktemp -d "${TMPDIR:-/tmp}/rinit-system-test.XXXXXX")"
fifo="${fifo_dir}/console"
mkfifo "${fifo}"
trap 'rm -rf "${fifo_dir}"' EXIT

# Wait until the console log matches a pattern, or give up after `timeout_s`.
#
# Driving the guest off fixed sleeps is what made the login assertion flaky: a
# boot that finished early still worked, but one that took a moment longer had
# the username typed before getty was listening, and the input was lost. Asking
# the log instead makes the feeder follow the guest rather than race it.
await_log() {
    local timeout_s="$1" pattern="$2"
    local deadline=$((SECONDS + timeout_s))
    while ((SECONDS < deadline)); do
        if grep -qE -- "${pattern}" "${log_file}"; then
            return 0
        fi
        sleep 0.2
    done
    return 1
}

# Hold the write end open for the life of qemu, then type as the guest prompts.
(
    exec 3>"${fifo}"

    # getty shows `login:`, so answer with the username. The password field in
    # /etc/passwd is empty, so an empty line must be accepted - that is the
    # property under test, and `id` afterwards proves it.
    await_log 40 'login:' || true
    printf 'root\n' >&3
    sleep 1
    printf '\n' >&3
    sleep 1
    printf 'id\n' >&3
    await_log 10 'uid=0\(root\)' || true

    # M3: drive rinit from the guest console. Each step waits for output that
    # only that command can produce, so a failure surfaces as a timeout rather
    # than as a race.
    printf 'rctl list\n' >&3
    await_log 10 'logdemo.service: inactive' || true
    printf 'rctl status getty.service\n' >&3
    await_log 10 'target: started' || true
    printf 'rctl start logdemo.service\n' >&3
    await_log 10 'started logdemo.service' || true
    # Give the oneshot a moment to run and its output to reach the buffer.
    sleep 1
    printf 'rctl log logdemo.service\n' >&3
    await_log 10 'rinit-log-marker' || true
    # A completed oneshot has nothing to restart: exit 4.
    printf 'rctl restart logdemo.service\n' >&3
    await_log 10 'no process to restart' || true
    printf 'rctl stop logdemo.service\n' >&3
    await_log 10 'stopped logdemo.service' || true
    printf 'rctl reset-failed fault.service\n' >&3
    await_log 10 'fault.service: cleared' || true

    # M5 §7.4: a unit that asks to be de-privileged must actually be so. The
    # command prints its uid, its gid and its own `NoNewPrivs` value, so this
    # one line is the whole claim - and it is read back through `rctl log`,
    # which also proves capture still works for a process that is not root.
    printf 'rctl start priv.service\n' >&3
    await_log 10 'started priv.service' || true
    sleep 1
    printf 'rctl log priv.service\n' >&3
    await_log 10 'uid=65534 gid=65534 nnp=1' || true

    # M5 §7.4: capabilities. `caps.service` stays root and empties its bounding
    # set; `ambient.service` runs as nobody and keeps exactly NET_BIND_SERVICE.
    # Both print the masks from their own /proc/self/status, so what is asserted
    # is the kernel's view of the service, not what rinit meant to do.
    printf 'rctl start caps.service\n' >&3
    await_log 10 'started caps.service' || true
    printf 'rctl start ambient.service\n' >&3
    await_log 10 'started ambient.service' || true
    sleep 1
    printf 'rctl log caps.service\n' >&3
    await_log 10 'bnd=0000000000000000' || true
    printf 'rctl log ambient.service\n' >&3
    await_log 10 'amb=0000000000000400' || true

    # M5 §7.3: cgroup v2 limits. `cglimit.service` reads its own placement and
    # its own limits back out of the kernel, so the assertions on its output are
    # about what the kernel did rather than what rinit meant to do.
    printf 'rctl start cglimit.service\n' >&3
    await_log 10 'started cglimit.service' || true
    sleep 1
    printf 'rctl log cglimit.service\n' >&3
    await_log 10 'cpu=50000 100000' || true

    # M5 §7.3: a *running* unit is inside its cgroup, and a stopped one leaves
    # nothing behind. `$(cat ...)` and `test -n` are how the console asks the
    # kernel directly: `cgroup.procs` is a live view (and st_size 0, so `test -s`
    # would be answered by `stat` rather than by the processes in it), and
    # `test -d` after the stop is the cleanup.
    printf 'rctl start cgrun.service\n' >&3
    await_log 10 'started cgrun.service' || true
    sleep 1
    printf 'p=$(cat /sys/fs/cgroup/rinit/cgrun.service/cgroup.procs); test -n "$p" && echo cgrun-populated || echo cgrun-empty\n' >&3
    await_log 10 'cgrun-populated' || true
    printf 'rctl stop cgrun.service\n' >&3
    await_log 10 'stopped cgrun.service' || true
    sleep 1
    printf 'test -d /sys/fs/cgroup/rinit/cgrun.service && echo cgrun-left || echo cgrun-gone\n' >&3
    await_log 10 'cgrun-gone' || true
    # The oneshot has exited by the time its log is read, so its cgroup has to
    # be gone too - the same cleanup, reached by the other exit path.
    printf 'test -d /sys/fs/cgroup/rinit/cglimit.service && echo cglimit-left || echo cglimit-gone\n' >&3
    await_log 10 'cglimit-gone' || true

    # M2: kill the console service and let rinit bring it back. `login` execs
    # the shell, so `$$` *is* the getty process tree; killing it makes rinit
    # reap the exit and, because getty is `Restart=always`, start it again -
    # the "自动重启" half of the milestone's acceptance test.
    printf 'kill -9 $$\n' >&3
    await_log 20 'getty.service will restart in' || true
    sleep 1
    exec 3>&-
) &
feeder_pid=$!

# The guest never exits on its own (rinit stays PID 1), so a bounded `timeout`
# is the expected outcome. `timeout` returns 124 when it had to stop qemu.
set +e
timeout -k 5 "${timeout_s}" \
    "${repo_root}/scripts/run-qemu.sh" \
    --arch "${arch}" --kernel "${kernel}" --initrd "${initrd}" \
    ${service:+--service "${service}"} <"${fifo}" >"${log_file}" 2>&1
qemu_rc=$?
wait "${feeder_pid}" 2>/dev/null || true
set -e

# 124 = we killed it (expected). Anything else means qemu died early.
if [[ ${qemu_rc} -ne 124 && ${qemu_rc} -ne 0 ]]; then
    echo "FAIL: qemu exited with ${qemu_rc} before the assertions could be checked" >&2
    tail -20 "${log_file}" >&2
    exit 3
fi

failures=0
pass() { printf '  ok   %s\n' "$1"; }
fail() {
    printf '  FAIL %s\n' "$1" >&2
    failures=$((failures + 1))
}

# expect <description> <fixed-string>
expect() {
    if grep -qF -- "$2" "${log_file}"; then
        pass "$1"
    else
        fail "$1 (missing: '$2')"
    fi
}

# The M0/M1 acceptance list: boots as PID 1, mounts the first wave, resolves
# the unit directory into a dependency-ordered schedule, and starts it.
expect "kernel executed rinit as init" "Run /sbin/rinit as init process"
expect "rinit started as PID 1" "starting (pid 1)"
expect "mounted /run" "rinit: mounted /run"
expect "mounted /proc" "rinit: mounted /proc"
expect "mounted /sys" "rinit: mounted /sys"
expect "mounted /dev" "rinit: mounted /dev"
expect "mounted /tmp" "rinit: mounted /tmp"

# M1: a real boot comes from the unit directory, not a hardcoded service.
expect "resolved a boot plan" "boot plan for 'multi-user.target'"
expect "the init anchor is scheduled" "plan 0: rinit-init.target"
expect "the console getty is scheduled" "getty.service"

# Boot must be ordered: mounts before any unit is started, and levels in
# dependency order.
line_of() { grep -n -m1 -F -- "$1" "${log_file}" | cut -d: -f1; }
require_order() {
    local desc="$1" first="$2" second="$3"
    local a b
    a="$(line_of "${first}")"
    b="$(line_of "${second}")"
    if [[ -n "${a}" && -n "${b}" && "${a}" -lt "${b}" ]]; then
        pass "${desc}"
    else
        fail "${desc} ('${first}' at ${a:-missing}, '${second}' at ${b:-missing})"
    fi
}
require_order "mounts complete before any unit starts" \
    "rinit: mounted /tmp" "starting level 0"
require_order "startup levels run in dependency order" \
    "starting level 0" "starting level 1"

# getty owns the console: it must actually be up and still running when the
# boot sequence finishes.
if grep -qF "started getty.service as pid" "${log_file}"; then
    pass "getty started as a process"
else
    fail "getty.service should have been started"
fi

# Both services in the closure must be started. Asserting the exact total would
# break every time a unit is added, so check the units themselves and only that
# the count is sane.
if grep -qF "started fault.service as pid" "${log_file}" \
    && grep -qF "started getty.service as pid" "${log_file}"; then
    pass "getty and the fault-injection unit were started at boot"
else
    fail "boot should have started getty.service and fault.service"
fi
if grep -qE "boot complete: [1-9][0-9]* unit\(s\) started" "${log_file}"; then
    pass "boot reported a positive number of started units"
else
    fail "boot should report the units it started"
fi

# Logging in must work, and with an empty password field there is nothing to
# type after the username. `uid=0(root)` is the proof that the whole path -
# getty, login, /etc/passwd - actually worked.
expect "getty presents a login prompt" "login:"
expect "login succeeded and gave a root shell" "uid=0(root)"

# M3 acceptance (plan §3): the guest's own `rctl` drives the running system and
# reads its logs back. `list`/`status`/`start`/`log`/`stop`/`reset-failed` are
# all exercised from the console above; the assertions check the output each one
# is the only possible source of.
expect "rctl list reached the control socket" "logdemo.service: inactive"
expect "rctl status described the console unit" "target: started"
expect "rctl started a unit on demand" "started logdemo.service"
expect "rctl log returned captured service output" "rinit-log-marker"
expect "rctl stop stopped a unit" "stopped logdemo.service"
expect "rctl reset-failed cleared the failed unit" "fault.service: cleared"

# M5 §7.4 acceptance: the privilege drop. `priv.service` runs
# `/bin/sh -c "echo uid=$(id -u) gid=$(id -g) nnp=..."` as `User=nobody`,
# `Group=nogroup`, with `NoNewPrivileges=true`, so the exact line proves three
# things at once: the process became nobody, its group became nogroup's, and
# the no-new-privileges flag was applied. rinit runs as root in the guest, so
# this is the case the host cannot test at all.
expect "a de-privileged service ran as nobody:nogroup with NoNewPrivs" \
    "uid=65534 gid=65534 nnp=1"

# M5 §7.4 acceptance: capabilities. This is the strictest setting in the plan
# (an empty `CapabilityBoundingSet=`) on the hardest subject: a service that is
# still **root**. A root process is normally handed every capability at `exec` -
# the kernel builds that set as `bset | inheritable` - so `eff=0` here is only
# possible because rinit dropped the bounding set before the exec.
expect "an empty CapabilityBoundingSet left a root service with no capabilities" \
    "uid=0 bnd=0000000000000000 eff=0000000000000000"

# The other half of the capability story: the one capability asked for is the
# one present, in all three sets. NET_BIND_SERVICE is bit 10, so 0x400.
# `amb`/`eff` are what a non-root service cannot get any other way, and `nobody`
# holding exactly one bit is the least-privilege shape §7.4 is for.
expect "ambient capabilities reached a de-privileged service" \
    "uid=65534 amb=0000000000000400 eff=0000000000000400 bnd=0000000000000400"

# M5 §7.3 acceptance: cgroup v2. The hierarchy is mounted by rinit's own first
# wave, then brought up (controllers enabled, the `rinit/` group created) before
# any unit starts, which is what makes a unit's attributes exist at all.
expect "rinit mounted the cgroup v2 hierarchy" "rinit: mounted /sys/fs/cgroup"
expect "the cgroup v2 hierarchy was brought up" "cgroup v2 ready at /sys/fs/cgroup"

# What the *kernel* says about a limited unit, read back by the unit itself.
# `self=` is the service process's own `/proc/<pid>/cgroup`; `0::` is the v2
# unified-hierarchy marker, and `/rinit/cglimit.service` is the placement §7.3
# asks for.
expect "a limited unit was placed in its own cgroup" \
    "self=0::/rinit/cglimit.service"
# A process the service forked is in the same cgroup, inherited by the kernel
# without rinit tracking anything.
expect "a process the service forked inherited its cgroup" \
    "child=0::/rinit/cglimit.service"
# `CgroupCPUMax=50%` against the kernel's 100000us period.
expect "the CPU quota was converted from a percentage" "cpu=50000 100000"
# Bytes, not the `32M` spelling the unit file uses.
expect "the memory hard and soft limits were written in bytes" \
    "mem=33554432 high=25165824"
expect "the task limit was applied to the cgroup" "pids=64"

# The live half: a running unit's cgroup holds a process, and after the stop the
# directory is gone. Both are read from the guest's own view of cgroupfs, and
# neither can be faked by a host test - an ordinary directory of stand-in
# attribute files is never empty enough to `rmdir`.
expect "a running unit was inside its cgroup" "cgrun-populated"
expect "a stopped unit's cgroup was removed" "cgrun-gone"
expect "a finished oneshot's cgroup was removed" "cglimit-gone"

# A completed oneshot has nothing to restart, so `restart` is a conflict
# (exit 4). `logdemo` proves it and that the unit is only started once.
if grep -qF "logdemo.service has no process to restart" "${log_file}"; then
    pass "rctl reported a restart conflict"
else
    fail "rctl restart of a completed oneshot should be a conflict"
fi
# An unrelated unit must not be pulled into the boot (plan §1.3(1)).
if [[ "$(grep -cF 'started logdemo.service as pid' "${log_file}")" -eq 1 ]]; then
    pass "logdemo.service only started because rctl asked"
else
    fail "logdemo.service must not be started at boot"
fi

# M2 acceptance (plan §3): kill a service and watch the manager restart it. The# console shell *is* the getty process tree, so `kill -9 $$` is a real crash of
# getty.service; a `Restart=always` unit must come back.
expect "a crashed service is scheduled for restart" "getty.service will restart in"
if [[ "$(grep -cF 'started getty.service as pid' "${log_file}")" -ge 2 ]]; then
    pass "the crashed console service was restarted"
else
    fail "getty.service must be restarted after it is killed"
fi
# A single crash is a restart, not a meltdown: the breaker must not have tripped.
if grep -qF "getty.service hit its start limit" "${log_file}"; then
    fail "a single crash must not trip getty's start limit"
else
    pass "one crash did not trip the breaker"
fi

# M2 acceptance: the breaker does trip when a service fails in a burst. The
# fault-injection unit fails immediately and twice, which exhausts its burst of
# two and parks it in Failed(StartLimitHit) instead of restarting forever.
expect "a repeatedly failing unit is retried at least once" "fault.service will restart in"
if grep -qE "error: fault.service hit its start limit \(2 failures\)" "${log_file}"; then
    pass "the breaker tripped on the failing unit"
else
    fail "fault.service should have tripped its start limit after two failures"
fi

# Plan §7.2: a target is an aggregation point with no process, so it must
# never appear in the "started ... as pid" stream.
if grep -qE "started (rinit-init|multi-user)\.target as pid" "${log_file}"; then
    fail "a target must never be exec'd (plan §7.2)"
else
    pass "no target was exec'd"
fi

# shell.service is the login-free alternative and must stay unstarted: it would
# fight getty for ttyAMA0.
if grep -qF "started shell.service" "${log_file}"; then
    fail "shell.service must not start while getty holds ttyAMA0"
else
    pass "shell.service correctly not started alongside getty"
fi

expect "boot completed" "boot complete"

# The panic hook must not have fired, and the kernel must not have died.
if grep -qF "rinit: PANIC" "${log_file}"; then
    fail "rinit reported a panic"
    grep -F "rinit: PANIC" "${log_file}" | head -5 >&2
else
    pass "no rinit panic"
fi

for pattern in "Kernel panic" "Internal error: Oops" "BUG: " "not syncing"; do
    if grep -qF -- "${pattern}" "${log_file}"; then
        fail "kernel log contains '${pattern}'"
        grep -F -- "${pattern}" "${log_file}" | head -3 >&2
    else
        pass "kernel log clean of '${pattern}'"
    fi
done

echo
if [[ ${failures} -eq 0 ]]; then
    echo "PASS: all assertions held on ${arch}"
    if [[ ${keep_log} -eq 1 ]]; then
        echo "log: ${log_file}"
    else
        # The log only matters when something failed; --keep-log asks for it
        # either way.
        rm -f "${log_file}"
    fi
    exit 0
fi

echo "FAIL: ${failures} assertion(s) failed on ${arch}" >&2
echo "full log: ${log_file}" >&2
exit 1
