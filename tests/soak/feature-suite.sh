#!/bin/sh
# Guest-side functional probe suite for the long soak.
#
# It shares the guest with scripts/soak-guest.sh: the sampler records
# feature_active while a probe runs, so the RSS/fd trend can exclude these
# windows. Probes are deliberately bounded and clean up after themselves.
set -u

seconds=604800
seed=""
tier=""
only=""
out=/var/log/rinit/feature-results.csv
status=/var/log/rinit/feature-suite.status
console=/dev/console
helper=/usr/local/bin/rinit-soak-helper
light_min=1080
light_max=2520
medium_min=16200
medium_max=27000
heavy_min=72000
heavy_max=93600
first_heavy=21600

while [ $# -gt 0 ]; do
    case "$1" in
        --seconds) seconds="${2:?}"; shift 2 ;;
        --seed) seed="${2:?}"; shift 2 ;;
        --tier) tier="${2:?}"; shift 2 ;;
        --scenario) only="${2:?}"; shift 2 ;;
        --light-min) light_min="${2:?}"; shift 2 ;;
        --light-max) light_max="${2:?}"; shift 2 ;;
        --medium-min) medium_min="${2:?}"; shift 2 ;;
        --medium-max) medium_max="${2:?}"; shift 2 ;;
        --heavy-min) heavy_min="${2:?}"; shift 2 ;;
        --heavy-max) heavy_max="${2:?}"; shift 2 ;;
        --first-heavy) first_heavy="${2:?}"; shift 2 ;;
        *) echo "feature-suite: unknown argument: $1" >&2; exit 2 ;;
    esac
done

[ -n "$seed" ] || seed="$(date +%s)"
[ "$seed" -gt 0 ] 2>/dev/null || seed=1
state=$seed
scenarios=0
failures=0
NEXT=0

mkdir -p /var/log/rinit
: >"$out"
printf 'ts,seed,scenario,tier,result,expected_fail,duration_ms,detail\n' >"$out"
printf 'feature suite seed=%s\n' "$seed" >"$status"

trap 'rm -rf /run/rinit-feature.lock; rm -f /run/rinit-feature-active' EXIT
if ! mkdir /run/rinit-feature.lock 2>/dev/null; then
    echo "feature-suite: already running" >&2
    exit 0
fi

rand_between() {
    state=$(( (state * 48271) % 2147483647 ))
    [ "$state" -gt 0 ] || state=1
    NEXT=$(( $1 + state % ($2 - $1 + 1) ))
}

feature_start() {
    touch /run/rinit-feature-active
}

feature_end() {
    rm -f /run/rinit-feature-active
}

record() {
    local name=$1
    local level=$2
    local result=$3
    local expected=$4
    local detail=$5
    printf '%s,%s,%s,%s,%s,%s,%s,%s\n' \
        "$(date +%s)" "$seed" "$name" "$level" "$result" "$expected" \
        "$duration" "$detail" >>"$out"
    printf 'FEATURE %s %s expected=%s %s\n' "$name" "$result" "$expected" "$detail" >"$console"
}

run_scenario() {
    local name=$1
    local level=$2
    local expected=$3
    local fn=$4
    local started result detail duration
    started=$(date +%s)
    feature_start
    if "$fn"; then
        result=pass
        detail=ok
    else
        result=fail
        detail="probe returned failure"
    fi
    feature_end
    duration=$(( ($(date +%s) - started) * 1000 ))
    scenarios=$((scenarios + 1))
    [ "$result" = pass ] || failures=$((failures + 1))
    record "$name" "$level" "$result" "$expected" "$detail"
}

unit_state() {
    rctl status --plain "$1" 2>/dev/null | awk '/^state:/{print $2; exit}'
}

unit_restarts() {
    rctl status --plain "$1" 2>/dev/null | awk '/^restarts:/{print $2; exit}'
}

wait_state() {
    local name=$1
    local wanted=$2
    local limit=$3
    local i=0
    while [ "$i" -lt "$limit" ]; do
        [ "$(unit_state "$name")" = "$wanted" ] && return 0
        sleep 1
        i=$((i + 1))
    done
    return 1
}

wait_file() {
    path=$1
    limit=$2
    i=0
    while [ "$i" -lt "$limit" ]; do
        [ -e "$path" ] && return 0
        sleep 1
        i=$((i + 1))
    done
    return 1
}

log_has() {
    rctl log "$1" 2>/dev/null | grep -q -- "$2"
}

start_unit() {
    rctl start "$1" >/dev/null 2>&1
}

stop_unit() {
    rctl stop "$1" >/dev/null 2>&1 || true
    rctl reset-failed "$1" >/dev/null 2>&1 || true
}

cleanup_names() {
    local unit
    for unit in "$@"; do
        stop_unit "$unit"
    done
}

# ---------------------------------------------------------------------------
# light probes

sc_rctl_formats() {
    fail=0
    for fmt in table plain short json; do
        rctl list --format "$fmt" >/tmp/fs-out 2>&1 || fail=1
        [ -s /tmp/fs-out ] || fail=1
    done
    for fmt in systemd plain short json card; do
        rctl status steady.service --format "$fmt" >/tmp/fs-out 2>&1 || fail=1
        [ -s /tmp/fs-out ] || fail=1
    done
    return "$fail"
}

sc_rctl_log() {
    start_unit logdemo.service || return 1
    sleep 1
    log_has logdemo.service rinit-log-marker || { stop_unit logdemo.service; return 1; }
    stop_unit logdemo.service
    return 0
}

sc_processes() {
    processes >/tmp/fs-out 2>&1 || return 1
    grep -q 'PPID' /tmp/fs-out || return 1
    processes --tree >/tmp/fs-out 2>&1 || return 1
    processes --limit=3 >/tmp/fs-out 2>&1 || return 1
    return 0
}

sc_dependency() {
    rm -f /tmp/fs-order
    start_unit fs-dep.target || return 1
    sleep 1
    [ "$(cat /tmp/fs-order 2>/dev/null)" = "$(printf 'b\na')" ] || {
        cleanup_names fs-dep-a.service fs-dep-b.service fs-dep.target
        return 1
    }
    cleanup_names fs-dep-a.service fs-dep-b.service fs-dep.target
    return 0
}

sc_wants_soft() {
    fail=0
    start_unit fs-wants.target || fail=1
    [ "$(unit_state fs-wants.target)" = active ] || fail=1
    [ "$(unit_state fs-wants-fail.service)" = failed ] || fail=1
    cleanup_names fs-wants.target fs-wants-fail.service
    return "$fail"
}

sc_requires_hard() {
    fail=0
    start_unit fs-requires.target && fail=1
    [ "$(unit_state fs-requires.target)" != active ] || fail=1
    [ "$(unit_state fs-requires-fail.service)" = failed ] || fail=1
    cleanup_names fs-requires.target fs-requires-fail.service
    return "$fail"
}

sc_template() {
    start_unit worker@light.service || return 1
    sleep 1
    log_has worker@light.service 'worker-instance=light' || {
        stop_unit worker@light.service
        return 1
    }
    stop_unit worker@light.service
    return 0
}

sc_environment() {
    printf 'FS_TWO=two\n' >/etc/fs-env.conf
    start_unit fs-env.service || return 1
    sleep 1
    log_has fs-env.service 'env=one file=two pwd=/tmp' || {
        stop_unit fs-env.service
        return 1
    }
    stop_unit fs-env.service
    return 0
}

sc_oneshot_lifecycle() {
    rm -f /tmp/fs-pre
    start_unit fs-pre.service || return 1
    sleep 1
    [ "$(cat /tmp/fs-pre 2>/dev/null)" = "$(printf 'pre\nmain\npost')" ] || {
        stop_unit fs-pre.service
        return 1
    }
    stop_unit fs-pre.service
    return 0
}

sc_conflicts() {
    fail=0
    start_unit fs-conflict-a.service || fail=1
    sleep 1
    start_unit fs-conflict-b.service || fail=1
    sleep 1
    [ "$(unit_state fs-conflict-a.service)" != active ] || fail=1
    cleanup_names fs-conflict-a.service fs-conflict-b.service
    return "$fail"
}

sc_restart_policies() {
    fail=0
    start_unit fs-restart-fail.service || fail=1
    sleep 2
    [ "$(unit_restarts fs-restart-fail.service)" -gt 0 ] 2>/dev/null || fail=1
    stop_unit fs-restart-fail.service

    start_unit fs-restart-success.service || fail=1
    sleep 2
    [ "$(unit_restarts fs-restart-success.service)" -gt 0 ] 2>/dev/null || fail=1
    stop_unit fs-restart-success.service
    return "$fail"
}

sc_log_ring() {
    start_unit fs-log-flood.service || return 1
    sleep 1
    log_has fs-log-flood.service 'flood-199' || {
        stop_unit fs-log-flood.service
        return 1
    }
    stop_unit fs-log-flood.service
    return 0
}

sc_device_services() {
    local svc
    fail=0
    for svc in device.target sshd.service mdev.service network-online.service; do
        [ "$(unit_state "$svc")" = active ] || fail=1
    done
    return "$fail"
}

# ---------------------------------------------------------------------------
# medium probes

sc_cgroup_limits() {
    start_unit cglimit.service || return 1
    sleep 1
    fail=0
    log_has cglimit.service 'cpu=50000 100000' || fail=1
    log_has cglimit.service 'mem=33554432 high=25165824' || fail=1
    log_has cglimit.service 'pids=64' || fail=1
    stop_unit cglimit.service
    return "$fail"
}

sc_cgroup_cleanup() {
    start_unit cgrun.service || return 1
    sleep 1
    dir=/sys/fs/cgroup/rinit/cgrun.service
    [ -n "$(cat "$dir/cgroup.procs" 2>/dev/null)" ] || { stop_unit cgrun.service; return 1; }
    stop_unit cgrun.service
    sleep 1
    [ ! -d "$dir" ]
}

sc_sandbox() {
    fail=0
    start_unit priv.service || fail=1
    sleep 1
    log_has priv.service 'uid=65534 gid=65534 nnp=1' || fail=1
    stop_unit priv.service

    start_unit caps.service || fail=1
    sleep 1
    log_has caps.service 'uid=0 bnd=0000000000000000 eff=0000000000000000' || fail=1
    stop_unit caps.service

    start_unit ambient.service || fail=1
    sleep 1
    log_has ambient.service 'uid=65534 amb=0000000000000400 eff=0000000000000400 bnd=0000000000000400' || fail=1
    stop_unit ambient.service
    return "$fail"
}

sc_forking() {
    start_unit forking.service || return 1
    sleep 1
    pid=$(cat /run/forking.pid 2>/dev/null)
    [ -n "$pid" ] && [ -d "/proc/$pid" ] || { stop_unit forking.service; return 1; }
    stop_unit forking.service
    sleep 1
    [ ! -e "/proc/$pid" ]
}

sc_sockets() {
    start_unit sockd.service || return 1
    sleep 2
    start_unit sockc.service || { stop_unit sockd.service; return 1; }
    sleep 1
    log_has sockc.service 'socket-present' || {
        cleanup_names sockc.service sockd.service
        return 1
    }
    cleanup_names sockc.service sockd.service
    return 0
}

sc_socket_connect() {
    stop_unit api.service
    "$helper" socket-connect /run/api.sock || return 1
    sleep 1
    log_has api.service 'activated-fd-3' || {
        stop_unit api.service
        return 1
    }
    stop_unit api.service
    return 0
}

sc_socket_idle() {
    "$helper" socket-connect /run/fs-idle.sock || return 1
    wait_state fs-idle.service active 5 || return 1
    sleep 4
    [ "$(unit_state fs-idle.service)" != active ] || {
        stop_unit fs-idle.service
        return 1
    }
    stop_unit fs-idle.service
    return 0
}

sc_health() {
    touch /run/fs-health-ok
    start_unit fs-health.service || { rm -f /run/fs-health-ok; return 1; }
    sleep 2
    rm -f /run/fs-health-ok
    sleep 4
    restarts=$(unit_restarts fs-health.service)
    [ "${restarts:-0}" -gt 0 ] 2>/dev/null || {
        stop_unit fs-health.service
        return 1
    }
    stop_unit fs-health.service
    return 0
}

sc_stop_timeout() {
    start_unit fs-timeout.service || return 1
    sleep 1
    start=$(date +%s)
    stop_unit fs-timeout.service
    sleep 3
    elapsed=$(( $(date +%s) - start ))
    [ "$elapsed" -ge 2 ] || return 1
    [ "$(unit_state fs-timeout.service)" != active ]
}

sc_start_timeout() {
    start_unit fs-start-timeout.service || true
    wait_state fs-start-timeout.service failed 5 || {
        stop_unit fs-start-timeout.service
        return 1
    }
    stop_unit fs-start-timeout.service
    return 0
}

sc_reload_noop() {
    rctl reload >/dev/null 2>&1
}

sc_reload_add() {
    cat >/etc/rinit/units/fs-reload.service <<'EOF'
[Unit]
Description=reload probe
[Service]
Type=oneshot
ExecStart=/bin/sh -c "echo reloaded-service"
EOF
    rctl reload >/dev/null 2>&1 || return 1
    start_unit fs-reload.service || return 1
    sleep 1
    log_has fs-reload.service 'reloaded-service' || {
        rm -f /etc/rinit/units/fs-reload.service
        rctl reload >/dev/null 2>&1
        return 1
    }
    rm -f /etc/rinit/units/fs-reload.service
    rctl reload >/dev/null 2>&1
    start_unit fs-reload.service && return 1
    return 0
}

sc_reload_reject() {
    cat >/etc/rinit/units/fs-bad.service <<'EOF'
[Service]
ExecStart="/bin/true
EOF
    fail=0
    rctl reload >/dev/null 2>&1 && fail=1
    [ "$(unit_state device.target)" = active ] || fail=1
    rm -f /etc/rinit/units/fs-bad.service
    rctl reload >/dev/null 2>&1
    return "$fail"
}

sc_notify_ready() {
    start_unit fs-notify.service || return 1
    sleep 1
    [ "$(unit_state fs-notify.service)" = active ] || {
        stop_unit fs-notify.service
        return 1
    }
    stop_unit fs-notify.service
    return 0
}

sc_notify_timeout() {
    start_unit fs-notify-timeout.service || true
    wait_state fs-notify-timeout.service failed 5 || {
        stop_unit fs-notify-timeout.service
        return 1
    }
    stop_unit fs-notify-timeout.service
    return 0
}

sc_tty_success() {
    rm -f /run/fs-tty /tmp/fs-tty-result
    "$helper" pty-hold /run/fs-tty &
    helper_pid=$!
    wait_file /run/fs-tty 5 || { kill "$helper_pid" 2>/dev/null; return 1; }
    start_unit fs-tty.service || { kill "$helper_pid" 2>/dev/null; return 1; }
    sleep 1
    [ "$(cat /tmp/fs-tty-result 2>/dev/null)" = 'tty-ok' ] || {
        stop_unit fs-tty.service
        kill "$helper_pid" 2>/dev/null
        return 1
    }
    stop_unit fs-tty.service
    kill "$helper_pid" 2>/dev/null
    rm -f /run/fs-tty /tmp/fs-tty-result
    return 0
}

sc_tty_bad() {
    fail=0
    start_unit fs-tty-bad.service && fail=1
    [ "$(unit_state fs-tty-bad.service)" = failed ] || fail=1
    stop_unit fs-tty-bad.service
    return "$fail"
}

# ---------------------------------------------------------------------------
# heavy probes

sc_oom() {
    start_unit fs-oom.service || return 1
    i=0
    while [ "$i" -lt 15 ]; do
        dmesg 2>/dev/null | grep -qi 'Memory cgroup out of memory' && break
        sleep 1
        i=$((i + 1))
    done
    fail=0
    dmesg 2>/dev/null | grep -qi 'Memory cgroup out of memory' || fail=1
    [ "$(unit_restarts fs-oom.service)" -gt 0 ] 2>/dev/null || fail=1
    stop_unit fs-oom.service
    return "$fail"
}

sc_breaker() {
    start_unit fault.service || return 1
    sleep 3
    fail=0
    rctl status --plain fault.service 2>/dev/null | grep -q 'start-limit' || fail=1
    stop_unit fault.service
    return "$fail"
}

sc_disk_audit() {
    free=$(df -k / | awk 'NR==2 {print $4; exit}')
    [ -n "$free" ] && [ "$free" -ge 8192 ]
}

sc_control_recovery() {
    rctl restart mdev.service >/dev/null 2>&1 || return 1
    wait_state mdev.service active 10
}

# ---------------------------------------------------------------------------
# tier runners

run_light() {
    rand_between 1 13
    case "$NEXT" in
        1) run_scenario light-rctl-formats light 0 sc_rctl_formats ;;
        2) run_scenario light-rctl-log light 0 sc_rctl_log ;;
        3) run_scenario light-processes light 0 sc_processes ;;
        4) run_scenario light-dependency light 0 sc_dependency ;;
        5) run_scenario light-wants light 1 sc_wants_soft ;;
        6) run_scenario light-requires light 1 sc_requires_hard ;;
        7) run_scenario light-template light 0 sc_template ;;
        8) run_scenario light-environment light 0 sc_environment ;;
        9) run_scenario light-oneshot light 0 sc_oneshot_lifecycle ;;
        10) run_scenario light-conflicts light 0 sc_conflicts ;;
        11) run_scenario light-restart light 0 sc_restart_policies ;;
        12) run_scenario light-log-ring light 0 sc_log_ring ;;
        13) run_scenario light-device light 0 sc_device_services ;;
    esac
}

run_light_all() {
    run_scenario light-rctl-formats light 0 sc_rctl_formats
    run_scenario light-rctl-log light 0 sc_rctl_log
    run_scenario light-processes light 0 sc_processes
    run_scenario light-dependency light 0 sc_dependency
    run_scenario light-wants light 1 sc_wants_soft
    run_scenario light-requires light 1 sc_requires_hard
    run_scenario light-template light 0 sc_template
    run_scenario light-environment light 0 sc_environment
    run_scenario light-oneshot light 0 sc_oneshot_lifecycle
    run_scenario light-conflicts light 0 sc_conflicts
    run_scenario light-restart light 0 sc_restart_policies
    run_scenario light-log-ring light 0 sc_log_ring
    run_scenario light-device light 0 sc_device_services
}

run_medium() {
    run_scenario medium-cgroup-limits medium 0 sc_cgroup_limits
    run_scenario medium-cgroup-cleanup medium 0 sc_cgroup_cleanup
    run_scenario medium-sandbox medium 0 sc_sandbox
    run_scenario medium-forking medium 0 sc_forking
    run_scenario medium-sockets medium 0 sc_sockets
    run_scenario medium-socket-connect medium 0 sc_socket_connect
    run_scenario medium-socket-idle medium 0 sc_socket_idle
    run_scenario medium-health medium 0 sc_health
    run_scenario medium-stop-timeout medium 0 sc_stop_timeout
    run_scenario medium-start-timeout medium 1 sc_start_timeout
    run_scenario medium-reload-noop medium 0 sc_reload_noop
    run_scenario medium-reload-add medium 0 sc_reload_add
    run_scenario medium-reload-reject medium 1 sc_reload_reject
    run_scenario medium-notify medium 0 sc_notify_ready
    run_scenario medium-notify-timeout medium 1 sc_notify_timeout
    run_scenario medium-tty medium 0 sc_tty_success
    run_scenario medium-tty-bad medium 1 sc_tty_bad
}

run_heavy() {
    run_scenario heavy-oom heavy 1 sc_oom
    run_scenario heavy-breaker heavy 1 sc_breaker
    run_scenario heavy-disk heavy 0 sc_disk_audit
    run_scenario heavy-control heavy 0 sc_control_recovery
}

finish() {
    if [ "$failures" -eq 0 ]; then
        result=OK
    else
        result=FAIL
    fi
    sync
    summary="FEATURE_DONE status=$result scenarios=$scenarios failures=$failures seed=$seed"
    printf '%s\n' "$summary" >"$status"
    printf '%s\n' "$summary" >"$console"
    [ "$result" = OK ] && exit 0
    exit 1
}

if [ -n "$only" ]; then
    run_scenario "manual-$only" manual 0 "sc_$only"
    finish
fi

if [ -n "$tier" ]; then
    case "$tier" in
        light) run_light ;;
        light-all) run_light_all ;;
        medium) run_medium ;;
        heavy) run_heavy ;;
        *) echo "feature-suite: unknown tier $tier" >&2; exit 2 ;;
    esac
    finish
fi

start=$(date +%s)
end=$((start + seconds))
next_light=$((start + 120))
next_medium=$((start + 1800))
next_heavy=$((start + first_heavy))

while :; do
    now=$(date +%s)
    [ "$now" -ge "$end" ] && break
    if [ "$now" -ge "$next_heavy" ]; then
        run_heavy
        rand_between "$heavy_min" "$heavy_max"
        next_heavy=$((now + NEXT))
    elif [ "$now" -ge "$next_medium" ]; then
        run_medium
        rand_between "$medium_min" "$medium_max"
        next_medium=$((now + NEXT))
    elif [ "$now" -ge "$next_light" ]; then
        run_light
        rand_between "$light_min" "$light_max"
        next_light=$((now + NEXT))
    else
        sleep 20
    fi
done

finish
