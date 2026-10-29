#!/bin/sh
# Guest-side soak sampler for the run-disk device image.
#
# This runs as root inside the guest and assumes rinit is PID 1. It deliberately
# does not depend on SSH: the host starts it once, then reads the CSV from the
# ext4 image or the SOAK heartbeats on the serial console if SSH is unavailable.
set -u

seconds=86400
interval=60
out=/var/log/rinit/soak-guest.csv
status=/var/log/rinit/soak-guest.status
console=/dev/console

while [ $# -gt 0 ]; do
    case "$1" in
        --seconds) seconds="${2:?}"; shift 2 ;;
        --interval) interval="${2:?}"; shift 2 ;;
        --out) out="${2:?}"; shift 2 ;;
        --status) status="${2:?}"; shift 2 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

mkdir -p /var/log/rinit
: >"$out"
printf 'elapsed,rss_kb,fds,threads,zombies,control_ok,control_fail,ip,route,udhcpc,dropbear,network,sshd\n' >"$out"

start=$(date +%s)
count=$((seconds / interval))
i=1
control_ok=0
control_fail=0
health_fail=0

sample_zombies() {
    z=0
    for f in /proc/[0-9]*/status; do
        [ -r "$f" ] || continue
        awk -v p=1 '$1 == "State:" { s = $2 } $1 == "PPid:" { if (s == "Z" && $2 == p) exit 0 } END { exit 1 }' "$f" && z=$((z + 1))
    done
    echo "$z"
}

while [ "$i" -le "$count" ]; do
    rss=$(awk '/VmRSS:/{print $2}' /proc/1/status 2>/dev/null)
    fds=$(ls /proc/1/fd 2>/dev/null | wc -l | tr -d ' ')
    threads=$(awk '/Threads:/{print $2}' /proc/1/status 2>/dev/null)
    zombies=$(sample_zombies)

    ctl_ok=0
    ctl_fail=0
    rctl status --plain steady.service >/dev/null 2>&1
    if [ $? -eq 0 ]; then ctl_ok=$((ctl_ok + 1)); else ctl_fail=$((ctl_fail + 1)); fi
    rctl restart driven.service >/dev/null 2>&1
    if [ $? -eq 0 ]; then ctl_ok=$((ctl_ok + 1)); else ctl_fail=$((ctl_fail + 1)); fi
    control_ok=$((control_ok + ctl_ok))
    control_fail=$((control_fail + ctl_fail))

    ip4=$(ip addr show dev eth0 2>/dev/null | awk '/inet /{print $2; exit}')
    route=$(ip route 2>/dev/null | awk '$1 == "default" { print $3; exit }')
    udhcpc=$(ps 2>/dev/null | grep -c '[u]dhcpc')
    dropbear=$(ps 2>/dev/null | grep -c '[d]ropbear')
    network=$(rctl status --plain network.service 2>/dev/null | awk '/^state:/{print $2; exit}')
    sshd=$(rctl status --plain sshd.service 2>/dev/null | awk '/^state:/{print $2; exit}')
    [ "$network" = "active" ] || health_fail=$((health_fail + 1))
    [ "$sshd" = "active" ] || health_fail=$((health_fail + 1))
    [ -n "$ip4" ] || health_fail=$((health_fail + 1))
    [ -n "$route" ] || health_fail=$((health_fail + 1))
    [ "$udhcpc" -gt 0 ] || health_fail=$((health_fail + 1))
    [ "$dropbear" -gt 0 ] || health_fail=$((health_fail + 1))

    elapsed=$(( $(date +%s) - start ))
    printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
        "$elapsed" "$rss" "$fds" "$threads" "$zombies" "$ctl_ok" "$ctl_fail" \
        "$ip4" "$route" "$udhcpc" "$dropbear" "$network" "$sshd" >>"$out"
    printf 'SOAK elapsed=%s rss=%s fds=%s threads=%s zombies=%s control=%s/%s network=%s sshd=%s\n' \
        "$elapsed" "$rss" "$fds" "$threads" "$zombies" "$ctl_ok" "$((ctl_ok + ctl_fail))" "$network" "$sshd" >"$console"

    i=$((i + 1))
    if [ "$i" -le "$count" ]; then
        target=$((start + (i - 1) * interval))
        now=$(date +%s)
        [ "$target" -gt "$now" ] && sleep $((target - now))
    fi
done

rctl stop churn.service >/dev/null 2>&1 || true
rctl stop driven.service >/dev/null 2>&1 || true
rctl stop steady.service >/dev/null 2>&1 || true
sleep 2
quiescent=$(sample_zombies)
rctl status --plain device.target >/dev/null 2>&1 || health_fail=$((health_fail + 1))
rctl status --plain sshd.service >/dev/null 2>&1 || health_fail=$((health_fail + 1))
rctl status --plain mdev.service >/dev/null 2>&1 || health_fail=$((health_fail + 1))

if [ "$control_fail" -eq 0 ] && [ "$quiescent" -eq 0 ] && [ "$health_fail" -eq 0 ]; then
    result=OK
else
    result=FAIL
fi
summary="SOAK_DONE status=$result samples=$count control_ok=$control_ok control_fail=$control_fail quiescent=$quiescent health_fail=$health_fail"
printf '%s\n' "$summary" >"$status"
printf '%s\n' "$summary" >"$console"
[ "$result" = "OK" ] && exit 0
exit 1
