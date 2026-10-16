#!/usr/bin/env bash
# Soak test: run rinit under sustained churn and watch its memory, descriptors
# and children for growth (plan §3 M4 "长稳 soak + 内存泄漏检测", §4.4).
#
# Usage:
#   scripts/soak.sh [--seconds N] [--interval SECONDS] [--rinit PATH]
#                   [--warmup N] [--max-rss-kb N] [--keep]
#
# What it does:
#   1. builds a scratch unit directory whose services churn: one crash-restarts
#      with `Restart=always` and prints to stdout (so every restart goes through
#      spawn -> log pipe -> reap -> restart), one stays up as a steady baseline,
#      and one exists only to be driven by `rctl`;
#   2. runs rinit in `--dev` mode against it, with the control socket in a
#      scratch directory (`--dev` skips mounts, so this needs no privileges);
#   3. every `--interval` samples the rinit process: RSS, open descriptors,
#      thread count and zombie children;
#   4. drives the control socket in a loop (`rctl status/log/restart`), because
#      the socket, the job queue and the log ring are all per-request state;
#   5. after the first `--warmup` samples (boot + first-touch allocations), the
#      steady-state curve must stay flat: RSS may not grow by more than
#      `--max-rss-kb`, descriptors must come back to the baseline, and no child
#      may be left as a zombie;
#   6. stops rinit with SIGTERM and checks it exits 0 (a soak that cannot shut
#      down cleanly has found something too).
#
# The default 60 seconds is a commit-time regression check, not the acceptance
# run. §3 M4 asks for 72 hours with no leak: that is the same script with
# `--seconds 259200` on the target, and the assertion thresholds below are what
# the long run has to satisfy.
#
# Exit codes:
#   0  every assertion held
#   1  an assertion failed (the sampled curve is printed)
#   2  bad usage / rinit could not be built or started
set -euo pipefail

seconds=60
interval=1
rinit=""
warmup=5
max_rss_kb=1024
keep=0

usage() { sed -n '2,32p' "$0"; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --seconds) seconds="${2:?}"; shift 2 ;;
        --interval) interval="${2:?}"; shift 2 ;;
        --rinit) rinit="${2:?}"; shift 2 ;;
        --warmup) warmup="${2:?}"; shift 2 ;;
        --max-rss-kb) max_rss_kb="${2:?}"; shift 2 ;;
        --keep) keep=1; shift ;;
        -h | --help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if [[ -z "$rinit" ]]; then
    echo "building rinit + rctl (host, debug)..."
    cargo build --offline --quiet
    rinit="$repo_root/target/debug/rinit"
fi
rctl="$repo_root/target/debug/rctl"
for bin in "$rinit" "$rctl"; do
    [[ -x "$bin" ]] || {
        echo "missing binary: $bin" >&2
        exit 2
    }
done

work="$(mktemp -d "${TMPDIR:-/tmp}/rinit-soak-XXXXXX")"
units="$work/units"
mkdir -p "$units" "$work/control" "$work/notify"

# The churn unit: it exits successfully, so `Restart=always` brings it straight
# back up - each cycle is a spawn, a log pipe, an exit and a reap, with no
# backoff and no breaker interference (a clean exit resets both counters). That
# is the loop that would leak a descriptor or a buffer if anything did.
#
# `StartLimitIntervalSec=0` is what makes the churn unbounded: it disables the
# breaker (plan §1.1), so the restart loop never parks. It is also the
# configuration that used to leak - the breaker's sliding window recorded one
# timestamp per restart when it had no interval to expire them with - which is
# why the soak uses it rather than the defaults.
cat >"$units/churn.service" <<'EOF'
[Unit]
Description=restarts forever, printing on every cycle
[Service]
Type=simple
ExecStart=/bin/sh -c "echo churn cycle; exit 0"
Restart=always
RestartSec=0
StartLimitBurst=1000000
StartLimitIntervalSec=0
EOF

# A steady unit keeps the "normal" side of the manager exercised alongside the
# churn: one long-lived process that is never restarted.
cat >"$units/steady.service" <<'EOF'
[Unit]
Description=stays up
[Service]
Type=simple
ExecStart=/bin/sleep 86400
Restart=no
TimeoutStopSec=5
EOF

# Started and stopped by `rctl` in the soak loop: the only unit that is driven
# by the control socket rather than by the boot.
cat >"$units/driven.service" <<'EOF'
[Unit]
Description=driven by rctl
[Service]
Type=simple
ExecStart=/bin/sleep 86400
Restart=no
TimeoutStopSec=5
EOF

cat >"$units/soak.target" <<'EOF'
[Unit]
Description=the soak target
Type=target
Requires=churn.service steady.service
EOF

export RINIT_CONTROL_DIR="$work/control"
export RINIT_NOTIFY_DIR="$work/notify"

log="$work/rinit.log"
"$rinit" --dev --units "$units" --default soak.target >"$log" 2>&1 &
rinit_pid=$!

cleanup() {
    # `rinit_pid` is cleared once rinit has been waited for: `kill 0` would
    # signal *this* process group, which is how a soak ends up killing the
    # shell that started it.
    if [[ -n "$rinit_pid" ]] && kill -0 "$rinit_pid" 2>/dev/null; then
        kill -TERM "$rinit_pid" 2>/dev/null || true
        wait "$rinit_pid" 2>/dev/null || true
    fi
    if [[ $keep -eq 1 ]]; then
        echo "kept: $work (rinit log: $log)"
    else
        rm -rf "$work"
    fi
}
trap cleanup EXIT

# Sample one number out of /proc, or 0 when the field is missing.
proc_field() {
    awk -v want="$2" '$1 == want { print $2; found = 1 } END { if (!found) print 0 }' \
        "/proc/$1/status" 2>/dev/null || echo 0
}

rss_of() { proc_field "$1" "VmRSS:"; }
threads_of() { proc_field "$1" "Threads:"; }
fds_of() { find "/proc/$1/fd" -mindepth 1 2>/dev/null | wc -l | tr -d ' '; }

# Zombies are children of rinit specifically: PID 1 must have none, ever.
zombies_of() {
    local count=0 pid state ppid
    for pid in /proc/[0-9]*; do
        state="$(awk '$1 == "State:" { print $2 }' "$pid/status" 2>/dev/null || true)"
        [[ "$state" == "Z" ]] || continue
        ppid="$(awk '$1 == "PPid:" { print $2 }' "$pid/status" 2>/dev/null || true)"
        [[ "$ppid" == "$1" ]] && count=$((count + 1))
    done
    echo "$count"
}

# Wait for the control socket so the churn loop does not race the boot.
for _ in $(seq 1 50); do
    [[ -S "$RINIT_CONTROL_DIR/control.sock" ]] && break
    kill -0 "$rinit_pid" 2>/dev/null || {
        echo "rinit exited during boot:" >&2
        cat "$log" >&2
        exit 2
    }
    sleep 0.1
done

echo "soaking for ${seconds}s (interval ${interval}s, warmup ${warmup} samples)..."
echo "sample  rss_kb  fds  threads  zombies"
curve="$work/curve.txt"
: >"$curve"

sample=0
end=$((SECONDS + seconds))
while ((SECONDS < end)); do
    sample=$((sample + 1))
    rss="$(rss_of "$rinit_pid")"
    fds="$(fds_of "$rinit_pid")"
    threads="$(threads_of "$rinit_pid")"
    zombies="$(zombies_of "$rinit_pid")"
    printf '%6d  %6d  %3d  %7d  %7d\n' "$sample" "$rss" "$fds" "$threads" "$zombies"
    printf '%d %d %d %d\n' "$rss" "$fds" "$threads" "$zombies" >>"$curve"

    # Drive the control socket: a start/stop cycle plus a log read, every
    # sample. Each one is a fresh connection, a fresh request and a reply.
    "$rctl" restart driven.service >/dev/null 2>&1 || true
    "$rctl" log --tail=5 >/dev/null 2>&1 || true

    kill -0 "$rinit_pid" 2>/dev/null || {
        echo "rinit died mid-soak:" >&2
        tail -40 "$log" >&2
        exit 1
    }
    sleep "$interval"
done

# Steady state is everything after the warmup: boot, the first allocations and
# the allocator's own arena growth all belong to the samples before it.
steady="$(awk -v w="$warmup" 'NR > w' "$curve")"
if [[ -z "$steady" ]]; then
    echo "soak too short to reach steady state (warmup $warmup of $sample samples)" >&2
    exit 2
fi

# Comparing the two halves of the window, by maximum, is what makes this a leak
# check rather than a snapshot: memory that grows once and then stops - a ring
# buffer filling, a table sized lazily - passes, while anything that keeps
# climbing, which is what a leak is, does not. Taking a per-half maximum also
# absorbs the descriptors that come and go with every restart.
half=$(($(echo "$steady" | wc -l) / 2))
first_half="$(echo "$steady" | head -n "$half")"
second_half="$(echo "$steady" | tail -n "$half")"
peak() { echo "$1" | awk -v column="$2" '{ if ($column > m) m = $column } END { print m + 0 }'; }

first_rss="$(peak "$first_half" 1)"
last_rss="$(peak "$second_half" 1)"
first_fds="$(peak "$first_half" 2)"
last_fds="$(peak "$second_half" 2)"
max_zombies="$(peak "$steady" 4)"

fail=0
rss_delta=$((last_rss - first_rss))
if ((rss_delta > max_rss_kb)); then
    echo "FAIL: RSS kept climbing: ${first_rss}kB peak in the first half, ${last_rss}kB in the second (allowed +${max_rss_kb}kB)" >&2
    fail=1
fi
if ((last_fds > first_fds + 2)); then
    echo "FAIL: descriptors kept climbing: ${first_fds} peak in the first half, ${last_fds} in the second: something is not closing" >&2
    fail=1
fi
if ((max_zombies > 0)); then
    echo "FAIL: ${max_zombies} zombie(s) parented to rinit: the reaper is missing exits" >&2
    fail=1
fi

# The restart loop has to have actually run, or the soak proves nothing.
restarts="$(grep -c 'churn.service will restart in' "$log" || true)"
if ((restarts < 10)); then
    echo "FAIL: only ${restarts} churn restarts in ${seconds}s: the churn workload is not churning" >&2
    fail=1
fi

# Shutdown is part of the soak: a manager that leaks may still stop cleanly, but
# one that cannot stop has definitely broken.
kill -TERM "$rinit_pid"
if ! wait "$rinit_pid"; then
    echo "FAIL: rinit did not exit 0 on SIGTERM" >&2
    tail -20 "$log" >&2
    fail=1
fi
rinit_pid=""

# The control socket has to have answered, or the request/reply path - the
# thing that owns a connection, a job and a log query per call - went untested.
control_ok="$(grep -c 'control: .* ok' "$log" || true)"
if ((control_ok < sample)); then
    echo "FAIL: only ${control_ok} control command(s) succeeded for ${sample} samples" >&2
    fail=1
fi

echo
echo "steady state (peak per half): rss ${first_rss}kB -> ${last_rss}kB (delta ${rss_delta}kB), fds ${first_fds} -> ${last_fds}, max zombies ${max_zombies}"
echo "churn: ${restarts} restarts, ${control_ok} control command(s) answered"

if ((fail)); then
    echo "SOAK FAILED" >&2
    exit 1
fi
echo "SOAK OK ($sample samples)"
