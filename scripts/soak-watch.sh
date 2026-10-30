#!/usr/bin/env bash
# Host-side watchdog for a long soak. It never restarts qemu: on a failed check
# it stops the named units and exits non-zero so the run can be examined.
#
# Usage:
#   scripts/soak-watch.sh --serial FILE --unit UNIT [options]
#
#   --serial FILE            qemu serial log
#   --unit UNIT              systemd user unit to watch; repeatable
#   --feature FILE           feature-results.csv written by the guest suite
#   --tag NAME               names the health log (default soak-watch)
#   --log FILE               health log path (default build/soak-charts/watch-<tag>.log)
#   --interval SEC           check period (default 300)
#   --max-age SEC            maximum age of the last SOAK heartbeat (default 240)
#   --feature-max-age SEC    maximum age of feature-results.csv (default 5400)
#   --min-host-free-mb N     minimum free space on /home (default 20480)
#   --min-guest-free-kb N    minimum disk_free_kb in the serial heartbeat (default 8192)
#   --grace SEC              startup grace before the first check (default 300)
#   --once                   run one check and exit (for testing)
set -u

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
serial=""
feature=""
tag=soak-watch
log=""
interval=300
max_age=240
feature_max_age=5400
min_host_free_mb=20480
min_guest_free_kb=8192
grace=300
once=0
units=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --serial) serial="${2:?}"; shift 2 ;;
        --unit) units+=("${2:?}"); shift 2 ;;
        --feature) feature="${2:?}"; shift 2 ;;
        --tag) tag="${2:?}"; shift 2 ;;
        --log) log="${2:?}"; shift 2 ;;
        --interval) interval="${2:?}"; shift 2 ;;
        --max-age) max_age="${2:?}"; shift 2 ;;
        --feature-max-age) feature_max_age="${2:?}"; shift 2 ;;
        --min-host-free-mb) min_host_free_mb="${2:?}"; shift 2 ;;
        --min-guest-free-kb) min_guest_free_kb="${2:?}"; shift 2 ;;
        --grace) grace="${2:?}"; shift 2 ;;
        --once) once=1; shift ;;
        -h | --help) sed -n '2,/^set -u$/p' "$0" | sed '$d'; exit 0 ;;
        *) echo "soak-watch: unknown argument: $1" >&2; exit 2 ;;
    esac
done

[[ -n "$serial" ]] || { echo "soak-watch: --serial is required" >&2; exit 2; }
[[ -n "$log" ]] || log="$repo_root/build/soak-charts/watch-$tag.log"
mkdir -p "$(dirname "$log")"
start=$(date +%s)
last_status=""

say() {
    local line
    line="$(date '+%F %T %Z') $*"
    printf '%s\n' "$line" >>"$log"
    printf '%s\n' "$line"
}

stop_units() {
    local unit
    for unit in "${units[@]}"; do
        systemctl --user stop "$unit" >/dev/null 2>&1 || true
    done
}

fail() {
    say "FAIL $*"
    stop_units
    exit 1
}

check() {
    local now age disk_free host_free last
    now=$(date +%s)

    for unit in "${units[@]}"; do
        if ! systemctl --user is-active --quiet "$unit"; then
            fail "unit $unit is not active"
        fi
    done

    if [[ -e "$serial" ]]; then
        age=$((now - $(stat -c %Y "$serial")))
        ((age <= max_age)) || fail "serial log is ${age}s old (limit ${max_age}s)"
        last=$(grep 'SOAK elapsed=' "$serial" | tail -1 || true)
        [[ -n "$last" ]] || fail "serial log has no SOAK heartbeat"
        disk_free=$(grep -oE 'disk_free=[0-9]+' <<<"$last" | tail -1 | cut -d= -f2 || true)
        if [[ -n "$disk_free" ]] && ((disk_free < min_guest_free_kb)); then
            fail "guest disk_free=${disk_free}kB (limit ${min_guest_free_kb}kB)"
        fi
    else
        ((now - start >= grace)) && fail "serial log is missing"
    fi

    if [[ -n "$feature" ]]; then
        if [[ -e "$feature" ]]; then
            age=$((now - $(stat -c %Y "$feature")))
            ((age <= feature_max_age)) || fail "feature results are ${age}s old (limit ${feature_max_age}s)"
        else
            ((now - start >= feature_max_age)) && fail "feature results are missing"
        fi
    fi

    host_free=$(df -Pk /home | awk 'NR == 2 { print int($4 / 1024); exit }')
    if [[ -n "$host_free" ]] && ((host_free < min_host_free_mb)); then
        fail "host free space is ${host_free}MB (limit ${min_host_free_mb}MB)"
    fi

    last_status="ok serial_age=${age:-0}s host_free=${host_free:-?}MB guest_free=${disk_free:-?}kB"
    say "$last_status"
}

if ((once)); then
    check
    exit 0
fi

if ((grace > 0)); then
    say "waiting ${grace}s before the first check"
    sleep "$grace"
fi

while :; do
    check
    sleep "$interval"
done
