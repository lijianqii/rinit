#!/usr/bin/env bash
# System test: boot rinit as PID 1 in qemu and assert on the serial console.
#
# Usage:
#   scripts/system-test.sh [--arch aarch64|x86_64] [--kernel IMAGE] [--initrd FILE]
#                          [--timeout SECONDS] [--log FILE] [--keep-log]
#
# What it does:
#   1. boots the guest with a bounded test service (`--ticks 6`), so the
#      service exits by itself and rinit must reap it;
#   2. captures the serial console to a log file;
#   3. asserts the M0 acceptance list on that log;
#   4. reports kernel panics / oopses as failures.
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
        echo "hint: run 'make run' (without SKIP_BUILD=1) to build it." >&2
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

# The guest never exits on its own (rinit stays PID 1), so a bounded `timeout`
# is the expected outcome. `timeout` returns 124 when it had to stop qemu.
set +e
timeout -k 5 "${timeout_s}" \
    "${repo_root}/scripts/run-qemu.sh" \
    --arch "${arch}" --kernel "${kernel}" --initrd "${initrd}" \
    ${service:+--service "${service}"} </dev/null >"${log_file}" 2>&1
qemu_rc=$?
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
expect "getty is scheduled" "getty.service"

# Boot must be ordered: mounts before any unit is started.
if [[ "$(grep -n 'rinit: mounted /tmp' "${log_file}" | head -1 | cut -d: -f1)" -lt \
      "$(grep -n 'starting level 0' "${log_file}" | head -1 | cut -d: -f1)" ]]; then
    pass "mounts complete before any unit starts"
else
    fail "mounts must complete before any unit starts"
fi

# Levels must be entered in dependency order: nothing may start before every
# earlier level has been reached.
first_line() { grep -n -m1 -F "$1" "${log_file}" | cut -d: -f1; }
l0="$(first_line 'starting level 0')"
l1="$(first_line 'starting level 1')"
if [[ -n "${l0}" && -n "${l1}" && "${l0}" -lt "${l1}" ]]; then
    pass "startup levels run in dependency order"
else
    fail "startup levels must run in dependency order (level0=${l0} level1=${l1})"
fi

# getty is Type=simple, so it is ready at exec and must be running afterwards.
if grep -qF "started getty.service as pid" "${log_file}"; then
    pass "getty started as a process"
else
    fail "getty.service should have been started"
fi

# Plan §7.2: a target is an aggregation point with no process, so it must
# never appear in the "started ... as pid" stream.
if grep -qE "started (rinit-init|multi-user)\.target as pid" "${log_file}"; then
    fail "a target must never be exec'd (plan §7.2)"
else
    pass "no target was exec'd"
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
    [[ ${keep_log} -eq 1 ]] || true
    exit 0
fi

echo "FAIL: ${failures} assertion(s) failed on ${arch}" >&2
echo "full log: ${log_file}" >&2
exit 1
