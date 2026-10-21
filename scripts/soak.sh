#!/usr/bin/env bash
# Soak test: run rinit under sustained churn and watch its memory, descriptors
# and children for growth (plan §3 M4 "长稳 soak + 内存泄漏检测", §4.4).
#
# Usage:
#   scripts/soak.sh [--seconds N] [--interval SECONDS] [--rinit PATH]
#                   [--warmup N] [--max-rss-kb N] [--min-free-kb N]
#                   [--max-samples N] [--keep]
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
#   3  the harness itself could not keep going: the work filesystem ran out of
#      room, or the console reader died. Nothing about rinit is implied. A 72-hour
#      run must not be cut short by the machine it runs on, so both are checked
#      every sample and reported as themselves instead of turning into an
#      unexplained "rinit died" an hour later.
set -euo pipefail

seconds=60
interval=1
rinit=""
warmup=5
max_rss_kb=1024
keep=0
# The work directory itself needs almost nothing (the console is streamed, not
# kept), but the machine is shared: another process filling the filesystem would
# otherwise kill the run for a reason it never reports.
min_free_kb=65536
# 0 means "until `--seconds` is up"; a positive value is a hard limit on the
# number of samples, which is what makes the window check below exercisable.
max_samples=0

usage() { sed -n '2,/^set -euo pipefail$/p' "$0" | sed '$d'; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --seconds) seconds="${2:?}"; shift 2 ;;
        --interval) interval="${2:?}"; shift 2 ;;
        --rinit) rinit="${2:?}"; shift 2 ;;
        --warmup) warmup="${2:?}"; shift 2 ;;
        --max-rss-kb) max_rss_kb="${2:?}"; shift 2 ;;
        --min-free-kb) min_free_kb="${2:?}"; shift 2 ;;
        --max-samples) max_samples="${2:?}"; shift 2 ;;
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
counts="$work/counts"
# rinit's console is the one unbounded thing in this script. The churn unit is
# deliberately unthrottled (`RestartSec=0`, breaker off) so rinit logs every
# restart, on the order of a thousand lines a second: a 72h run wrote 955 MB in
# five hours, filled the 978M /tmp tmpfs, and the soak died of ENOSPC rather than
# of anything it was measuring.
#
# So the console goes into a fifo and a reader instead of into a file. The reader
# is the only thing that has to keep up, and it keeps almost nothing: a running
# count of the two lines the assertions below need - written out as it goes, so
# they are readable *before* the shutdown - and a rolling tail of the last
# `log_lines` lines for a post-mortem. The run is unchanged in every way but how
# much disk it uses.
log_lines=2000
mkfifo "$work/console"
# (`snapshot`, not `log`: gawk has a `log` builtin and refuses the name.)
awk -v counts="$counts" -v snapshot="$log" -v keep="$log_lines" '
    /churn\.service will restart in/ { r++; if (r % 200 == 0) { print "R " r >> counts; close(counts) } }
    /control: .* ok/                 { c++; if (c % 50 == 0)  { print "C " c >> counts; close(counts) } }
    { ring[NR % keep] = $0 }
    END {
        print "R " r >> counts
        print "C " c >> counts
        close(counts)
        for (i = NR - keep + 1; i <= NR; i++) if ((i % keep) in ring) print ring[i % keep] > snapshot
        close(snapshot)
    }
' <"$work/console" &
console_pid=$!
"$rinit" --dev --units "$units" --default soak.target >"$work/console" 2>&1 &
rinit_pid=$!

cleanup() {
    # `rinit_pid` is cleared once rinit has been waited for: `kill 0` would
    # signal *this* process group, which is how a soak ends up killing the
    # shell that started it.
    if [[ -n "$rinit_pid" ]] && kill -0 "$rinit_pid" 2>/dev/null; then
        kill -TERM "$rinit_pid" 2>/dev/null || true
        wait "$rinit_pid" 2>/dev/null || true
    fi
    # rinit's exit closes the fifo, so the reader is at EOF; waiting for it
    # guarantees the log is complete before anyone is told to read it.
    flush_log
    if [[ $keep -eq 1 ]]; then
        echo "kept: $work (rinit log: $log)"
    else
        rm -rf "$work"
    fi
}
trap cleanup EXIT

# Wait for the console reader, which is at EOF once rinit's end of the fifo is
# closed - so it has written the final counts and the tail. Called before
# anything reads them, so the numbers and the diagnostics are complete.
flush_log() {
    [[ -n "$console_pid" ]] && wait "$console_pid" 2>/dev/null || true
    return 0
}

# One of the reader's running counts, or 0 when it has not written that one yet
# (it writes every 200 restarts and every 50 control commands, so early in a run
# the file may hold only the other).
counted() {
    awk -v want="$1" '$1 == want { n = $2 } END { print n + 0 }' "$counts" 2>/dev/null || echo 0
}

# Free space in kB on the filesystem the work directory lives on, or empty when
# the question cannot be answered - an unanswerable question must not stop the
# soak, so this never fails.
free_kb() { df -Pk "$work" 2>/dev/null | awk 'NR == 2 { print $4 }' || true; }
work_fs() { df -Pk "$work" 2>/dev/null | awk 'NR == 2 { print $6 }' || true; }

# Sample one number out of /proc, or 0 when the field is missing.
proc_field() {
    awk -v want="$2" '$1 == want { print $2; found = 1 } END { if (!found) print 0 }' \
        "/proc/$1/status" 2>/dev/null || echo 0
}

rss_of() { proc_field "$1" "VmRSS:"; }
threads_of() { proc_field "$1" "Threads:"; }
fds_of() { find "/proc/$1/fd" -mindepth 1 2>/dev/null | wc -l | tr -d ' '; }

# Zombies are children of rinit specifically: PID 1 must have none, ever.
#
# One awk over every status file rather than an awk per file: the per-file
# version forked twice per process on the machine (167 processes here, ~0.8s a
# sample), which at `--interval 1` cost more than the interval itself and made
# the default 60s gate sample only a third of the window it asked for. `State:`
# precedes `PPid:` in /proc/<pid>/status, so a single pass sees both.
zombies_of() {
    awk -v parent="$1" '
        FNR == 1        { zombie = 0 }
        $1 == "State:"  { zombie = ($2 == "Z") }
        $1 == "PPid:"   { if (zombie && $2 == parent) count++ }
        END             { print count + 0 }
    ' /proc/[0-9]*/status 2>/dev/null || echo 0
}

# Wait for the control socket so the churn loop does not race the boot.
for _ in $(seq 1 50); do
    [[ -S "$RINIT_CONTROL_DIR/control.sock" ]] && break
    kill -0 "$rinit_pid" 2>/dev/null || {
        echo "rinit exited during boot:" >&2
        flush_log
        cat "$log" >&2
        exit 2
    }
    sleep 0.1
done

echo "soaking for ${seconds}s (interval ${interval}s, warmup ${warmup} samples)..."
echo "sample  when             rss_kb  fds  threads  zombies"
curve="$work/curve.txt"
: >"$curve"

sample=0
started=$SECONDS
end=$((started + seconds))
next=$started
while ((SECONDS < end)); do
    # An `if`, not `&& break`: under `set -e` a false `(( ))` as the last command
    # of a list is an error, which would abort the run on the first sample.
    if ((max_samples > 0)) && ((sample >= max_samples)); then
        break
    fi
    # Sleep until this sample's absolute slot. Sleeping a whole `interval` after
    # the work instead lets the work add to every period: at `--interval 1` a
    # sample costs ~70ms, so a 60s window yielded 56 samples and failed the
    # coverage check at the bottom for arithmetic rather than for anything it
    # measured. Sleeping until `next` keeps the cadence while the slots are
    # reachable; if one sample costs more than `interval`, the loop runs back to
    # back and the check still sees the shortfall.
    now=$SECONDS
    if ((next > now)); then
        if ((next >= end)); then
            break
        fi
        sleep $((next - now))
    fi
    sample=$((sample + 1))
    rss="$(rss_of "$rinit_pid")"
    fds="$(fds_of "$rinit_pid")"
    threads="$(threads_of "$rinit_pid")"
    zombies="$(zombies_of "$rinit_pid")"
    # A timestamp per sample, so a run that stops at hour 60 says *when* it
    # stopped: the curve itself is numbers only, and a wall-clock time is the one
    # thing a post-mortem needs that the numbers cannot carry.
    printf '%6d  %-14s  %6d  %3d  %7d  %7d\n' \
        "$sample" "$(date '+%m-%d %H:%M')" "$rss" "$fds" "$threads" "$zombies"
    printf '%d %d %d %d\n' "$rss" "$fds" "$threads" "$zombies" >>"$curve"

    # The two ways the harness itself can stop, checked rather than assumed. The
    # reader dying would freeze the counts and the assertions would then fail for
    # a reason that has nothing to do with rinit; a full filesystem would abort
    # the appends (`set -e`) and look like a crash.
    kill -0 "$console_pid" 2>/dev/null || {
        echo "the console reader (pid ${console_pid}) is gone: the restart and control counts would stop, so this run cannot assert anything" >&2
        exit 3
    }
    free="$(free_kb)"
    if [[ -n "$free" ]] && ((free < min_free_kb)); then
        echo "${free}kB left on $(work_fs) (floor ${min_free_kb}kB): the work directory cannot be written, so the run is stopping here rather than aborting mid-sample" >&2
        exit 3
    fi

    # Drive the control socket: a start/stop cycle plus a log read, every
    # sample. Each one is a fresh connection, a fresh request and a reply.
    "$rctl" restart driven.service >/dev/null 2>&1 || true
    "$rctl" log --tail=5 >/dev/null 2>&1 || true

    kill -0 "$rinit_pid" 2>/dev/null || {
        echo "rinit died mid-soak:" >&2
        flush_log
        tail -40 "$log" >&2
        exit 1
    }
    next=$((next + interval))
done

# Steady state is everything after the warmup: boot, the first allocations and
# the allocator's own arena growth all belong to the samples before it.
steady="$(awk -v w="$warmup" 'NR > w' "$curve")"
if [[ -z "$steady" ]] || (($(echo "$steady" | wc -l) < 2)); then
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
# The lowest value in a half: what the descriptor check is actually about. The
# count moves with the churn - a log pipe, an accepted connection, a spawned
# child - so a *peak* per half is a coin flip over 72 hours (one sample catching
# the wrong instant fails the run), while the trough is the state everything is
# supposed to come back to. A leak raises the trough; a spike leaves it alone.
trough() { echo "$1" | awk -v column="$2" 'NR == 1 { m = $column } { if ($column < m) m = $column } END { print m + 0 }'; }

first_rss="$(peak "$first_half" 1)"
last_rss="$(peak "$second_half" 1)"
first_fds="$(trough "$first_half" 2)"
last_fds="$(trough "$second_half" 2)"
fds_peak="$(peak "$steady" 2)"
max_zombies="$(peak "$steady" 4)"

fail=0
rss_delta=$((last_rss - first_rss))
if ((rss_delta > max_rss_kb)); then
    echo "FAIL: RSS kept climbing: ${first_rss}kB peak in the first half, ${last_rss}kB in the second (allowed +${max_rss_kb}kB)" >&2
    fail=1
fi
if ((last_fds > first_fds + 2)); then
    echo "FAIL: descriptors kept climbing: their lowest was ${first_fds} in the first half and ${last_fds} in the second (peak ${fds_peak}): something is not closing" >&2
    fail=1
fi
if ((max_zombies > 0)); then
    echo "FAIL: ${max_zombies} zombie(s) parented to rinit: the reaper is missing exits" >&2
    fail=1
fi

# A clock step, a signal or any other early exit must not be able to turn a
# short window into a pass: the samples have to cover what was asked for.
# `--max-samples` shortens the window on purpose, so it lowers the expectation
# with it; otherwise every debugging run capped below `seconds/interval` fails
# for arithmetic rather than for anything it measured.
expected=$((seconds / interval))
if ((max_samples > 0)) && ((expected > max_samples)); then
    expected=$max_samples
fi
if ((sample < expected * 95 / 100)); then
    echo "FAIL: only ${sample} samples for a ${seconds}s window at ${interval}s (expected ~${expected}): the window is too short to assert anything" >&2
    fail=1
fi

# Shutdown is part of the soak: a manager that leaks may still stop cleanly, but
# one that cannot stop has definitely broken. Stopping first is also what makes
# the counts below complete: the reader writes them as it goes and once more at
# end of input, so a count read while the reader is still behind is an
# undercount. `R` is only written every 200 restarts, so a short run could read
# 0 after churning all along.
kill -TERM "$rinit_pid"
if ! wait "$rinit_pid"; then
    echo "FAIL: rinit did not exit 0 on SIGTERM" >&2
    flush_log
    tail -20 "$log" >&2
    fail=1
fi
rinit_pid=""
flush_log

# The restart loop has to have actually run, or the soak proves nothing.
restarts="$(counted R)"
if ((restarts < 10)); then
    echo "FAIL: only ${restarts} churn restarts in ${seconds}s: the churn workload is not churning" >&2
    fail=1
fi

# The control socket has to have answered, or the request/reply path - the
# thing that owns a connection, a job and a log query per call - went untested.
control_ok="$(counted C)"
if ((control_ok < sample)); then
    echo "FAIL: only ${control_ok} control command(s) succeeded for ${sample} samples" >&2
    fail=1
fi

echo
echo "steady state: rss ${first_rss}kB -> ${last_rss}kB peak per half (delta ${rss_delta}kB), fds ${first_fds} -> ${last_fds} at their lowest (peak ${fds_peak}), max zombies ${max_zombies}"
echo "churn: ${restarts} restarts, ${control_ok} control command(s) answered"

if ((fail)); then
    echo "SOAK FAILED" >&2
    exit 1
fi
echo "SOAK OK ($sample samples)"
