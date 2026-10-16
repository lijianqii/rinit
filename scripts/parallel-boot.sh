#!/usr/bin/env bash
# Parallel-vs-serial startup measurement (plan §6 acceptance: "20 个服务依赖并行
# 启动,耗时与串行比有明显提升").
#
# Usage:
#   scripts/parallel-boot.sh [--count N] [--seconds S] [--rinit PATH] [--keep]
#
# What it measures is rinit's *scheduling*, which is why it runs on the host in
# `--dev` mode: the boot plan, the level walk, the spawn path and the wait for
# `oneshot` units are the same code the guest runs, so the ratio below is about
# the scheduler rather than about qemu. The guest asserts the *derived* property
# instead - that levels run in dependency order - because a guest run cannot see
# a ratio without two boots.
#
# Two runs over the same N services, each a `oneshot` that sleeps `--seconds`:
#
#   parallel: every service depends only on `rinit-init.target`, so all N land in
#             one level and are spawned together;
#   serial:   each service is `After=` the previous one, so every level holds one
#             service and they start one after another.
#
# Both runs are wall-clock from spawning rinit to its "boot complete" line, which
# for a directory of `oneshot` services is exactly when the last one has exited
# (plan §1.3(3)). A parallel boot should therefore cost about one sleep and a
# serial boot about N of them.
#
# Exit codes: 0 the speedup is there, 1 it is not (or a run failed), 2 usage.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
count=20
seconds=1
rinit="${repo_root}/target/debug/rinit"
keep=0
# What "明显提升" is taken to mean: a parallel boot has to be at least this many
# times faster than the serial one. The floor is not 1 - it is well below the
# ratio an N-level chain should give, so the check survives scheduling noise
# while still failing if the levels stop being parallel at all.
min_speedup=4

while [[ $# -gt 0 ]]; do
    case "$1" in
        --count) count="${2:?}"; shift 2 ;;
        --seconds) seconds="${2:?}"; shift 2 ;;
        --rinit) rinit="${2:?}"; shift 2 ;;
        --keep) keep=1; shift ;;
        -h | --help) sed -n '2,27p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

if [[ ! -x "${rinit}" ]]; then
    echo "error: no rinit binary at ${rinit}; run 'cargo build' first" >&2
    exit 2
fi

work="$(mktemp -d "${TMPDIR:-/tmp}/rinit-parallel-XXXXXX")"
cleanup() {
    if ((keep)); then
        echo "kept: ${work}"
    else
        rm -rf "${work}"
    fi
}
trap cleanup EXIT

# A directory of `count` services plus the target that pulls them in, laid out
# for one of the two shapes. `order` is what differs: nothing, or "wait for the
# previous service".
write_units() {
    local dir="$1" shape="$2"
    mkdir -p "${dir}"
    printf '[Unit]\nType=target\nRequires=%s\n' \
        "$(services_required | tr '\n' ' ')" >"${dir}/load.target"

    local index=0 previous=""
    while ((index < count)); do
        local name
        name="$(printf 'svc%02d.service' "${index}")"
        {
            printf '[Unit]\n'
            if [[ "${shape}" == "serial" && -n "${previous}" ]]; then
                printf 'Requires=%s\nAfter=%s\n' "${previous}" "${previous}"
            fi
            printf '[Service]\nType=oneshot\nExecStart=/bin/sleep %s\n' "${seconds}"
        } >"${dir}/${name}"
        previous="${name}"
        index=$((index + 1))
    done
}

services_required() {
    local index=0
    while ((index < count)); do
        printf 'svc%02d.service\n' "${index}"
        index=$((index + 1))
    done
}

# Boot `dir` and print the milliseconds from spawn to "boot complete".
measure() {
    local dir="$1" log="$2"
    local started finished
    started="$(date +%s%N)"
    "${rinit}" --dev --units "${dir}" --default load.target >"${log}" 2>&1 &
    local pid=$!
    # Wait for the boot to finish, then stop rinit. The wait is bounded so a
    # scheduler that never gets there fails the measurement instead of hanging.
    # `SECONDS` rather than an accumulator: bash arithmetic is integer-only, and
    # a float budget silently becomes a syntax error in the loop condition.
    local started_at=${SECONDS}
    while ! grep -q 'boot complete' "${log}" 2>/dev/null; do
        if ! kill -0 "${pid}" 2>/dev/null; then
            echo "error: rinit exited before the boot completed; see ${log}" >&2
            return 1
        fi
        if ((SECONDS - started_at > 120)); then
            echo "error: the boot did not complete within 120s; see ${log}" >&2
            kill -TERM "${pid}" 2>/dev/null || true
            return 1
        fi
        sleep 0.05
    done
    finished="$(date +%s%N)"
    kill -TERM "${pid}"
    wait "${pid}" || true
    # Milliseconds, integer.
    awk -v a="${started}" -v b="${finished}" 'BEGIN { printf "%d\n", (b - a) / 1000000 }'
}

echo "rinit: ${rinit}"
echo "services: ${count} x 'Type=oneshot, ExecStart=/bin/sleep ${seconds}'"

parallel_dir="${work}/parallel"
serial_dir="${work}/serial"
write_units "${parallel_dir}" parallel
write_units "${serial_dir}" serial

# Warm up first, and throw the number away: the very first boot of a freshly
# written binary pays for ones the later ones do not (page cache, dynamic link,
# first-touch allocations), and it is worth several hundred milliseconds - which
# is more than the whole parallel boot costs. Measuring that would compare a cold
# start against a warm one.
measure "${parallel_dir}" "${work}/warmup.log" >/dev/null

parallel_ms="$(measure "${parallel_dir}" "${work}/parallel.log")"
serial_ms="$(measure "${serial_dir}" "${work}/serial.log")"

# The scheduler has to have actually started them: a boot that started nothing
# would be fast for the wrong reason.
started="$(grep -c 'completed (Active(Exited))' "${work}/parallel.log" || true)"
if ((started != count)); then
    echo "FAIL: the parallel run completed ${started} of ${count} services" >&2
    tail -20 "${work}/parallel.log" >&2
    exit 1
fi
levels="$(grep -c 'starting level' "${work}/parallel.log" || true)"
if ((levels > 3)); then
    echo "FAIL: the parallel run used ${levels} levels; the services are not sharing one" >&2
    exit 1
fi

speedup="$(awk -v s="${serial_ms}" -v p="${parallel_ms}" 'BEGIN { if (p > 0) printf "%.1f", s / p; else print "inf" }')"

echo
echo "parallel: ${parallel_ms} ms (${levels} level(s) of ${count})"
echo "serial:   ${serial_ms} ms (${count} level(s) of 1)"
echo "speedup:  ${speedup}x (floor ${min_speedup}x)"

if awk -v s="${speedup}" -v m="${min_speedup}" 'BEGIN { exit !(s >= m) }'; then
    echo "PARALLEL OK"
    exit 0
fi
echo "FAIL: ${speedup}x is below the ${min_speedup}x floor: the levels are not starting in parallel" >&2
exit 1
