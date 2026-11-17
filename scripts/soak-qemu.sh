#!/usr/bin/env bash
# Host-side driver for a guest-side run-disk soak.
#
# The guest sampler is started once over SSH, then runs independently of the
# host connection. This avoids making the whole 24 h run depend on 1,440 SSH
# sessions; the serial console still carries a compact heartbeat if SSH fails.
#
# Usage: scripts/soak-qemu.sh --key PATH [--arch aarch64|arm] [--seconds N]
#                             [--interval N] [--tag NAME] [--kernel FILE]
#                             [--disk FILE] [--port N] [--checkpoint-interval N]
#                             [--helper FILE --feature-suite FILE]
#                             [--feature-args STRING] [--seed N]
set -u

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
rinit_version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$repo_root/Cargo.toml" | head -1)"
kernel_version="${KERNEL_VERSION:-$(sed -n 's/^KERNEL_VERSION[[:space:]]*?=[[:space:]]*//p' "$repo_root/Makefile" | head -1)}"
[[ -n "$rinit_version" ]] || { echo "error: cannot read the rinit version from Cargo.toml" >&2; exit 2; }
[[ -n "$kernel_version" ]] || { echo "error: cannot read KERNEL_VERSION from the Makefile" >&2; exit 2; }
arch=aarch64
kernel=""
disk=""
key=""
seconds=3600
interval=60
port=2222
checkpoint_interval=3600
tag="qemu-soak"
log_dir="$repo_root/build/soak-charts"
helper=""
feature_suite=""
feature_args=""
seed=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch) arch="${2:?}"; shift 2 ;;
        --kernel) kernel="${2:?}"; shift 2 ;;
        --disk) disk="${2:?}"; shift 2 ;;
        --key) key="${2:?}"; shift 2 ;;
        --seconds) seconds="${2:?}"; shift 2 ;;
        --interval) interval="${2:?}"; shift 2 ;;
        --port) port="${2:?}"; shift 2 ;;
        --checkpoint-interval) checkpoint_interval="${2:?}"; shift 2 ;;
        --helper) helper="${2:?}"; shift 2 ;;
        --feature-suite) feature_suite="${2:?}"; shift 2 ;;
        --feature-args) feature_args="${2:?}"; shift 2 ;;
        --seed) seed="${2:?}"; shift 2 ;;
        --tag) tag="${2:?}"; shift 2 ;;
        -h | --help) sed -n '2,/^set -u$/p' "$0" | sed '$d'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

[[ "$tag" =~ ^[A-Za-z0-9._-]+$ ]] ||
    { echo "error: --tag must match [A-Za-z0-9._-]+" >&2; exit 2; }
[[ "$port" =~ ^[0-9]+$ ]] && ((10#$port >= 1 && 10#$port <= 65535)) ||
    { echo "error: --port must be 1..65535" >&2; exit 2; }
[[ "$checkpoint_interval" =~ ^[0-9]+$ ]] ||
    { echo "error: --checkpoint-interval must be a non-negative integer" >&2; exit 2; }

case "$arch" in
    aarch64)
        : "${kernel:=$repo_root/build/src/linux-${kernel_version}/arch/arm64/boot/Image}"
        : "${disk:=$repo_root/build/out/rinit-aarch64-unknown-linux-gnu.ext4}"
        qemu_bin=qemu-system-aarch64
        ;;
    arm)
        : "${kernel:=$repo_root/build/arm32/src/linux-${kernel_version}/arch/arm/boot/zImage}"
        : "${disk:=$repo_root/build/arm32/out/rinit-armv7-unknown-linux-gnueabihf.ext4}"
        qemu_bin=qemu-system-arm
        ;;
    *) echo "error: --arch must be aarch64 or arm" >&2; exit 2 ;;
esac

[[ -n "$key" ]] || { echo "error: --key is required" >&2; exit 2; }
for f in "$kernel" "$disk" "$key"; do
    [[ -e "$f" ]] || { echo "error: missing $f" >&2; exit 2; }
done
if [[ -n "$feature_suite" ]]; then
    [[ -n "$helper" ]] || { echo "error: --feature-suite requires --helper" >&2; exit 2; }
    [[ -e "$helper" ]] || { echo "error: missing $helper" >&2; exit 2; }
    [[ -e "$feature_suite" ]] || { echo "error: missing $feature_suite" >&2; exit 2; }
fi
seed="${seed:-$(date +%s)}"
mkdir -p "$log_dir"
serial="$log_dir/soak-qemu-${tag}.log"
guest_csv="$log_dir/soak-qemu-${tag}-guest.csv"
run_info="$log_dir/soak-qemu-${tag}-run.txt"
checkpoint_dir="$log_dir/soak-qemu-${tag}-checkpoints"
rm -f "$serial" "$guest_csv" "$run_info"
rm -rf "$checkpoint_dir"
mkdir -p "$checkpoint_dir"
printf 'checkpoint log for %s\n' "$tag" >"$checkpoint_dir/log"

work=$(mktemp -d "${TMPDIR:-/tmp}/soak-qemu.XXXXXX")
qemu_pid=""
cleanup() {
    if [[ -n "$qemu_pid" ]] && kill -0 "$qemu_pid" 2>/dev/null; then
        kill "$qemu_pid" 2>/dev/null || true
    fi
    wait "$qemu_pid" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

cat >"$work/steady.service" <<'EOF'
[Unit]
Description=stays up
[Service]
Type=simple
ExecStart=/bin/sleep 2592000
Restart=no
TimeoutStopSec=5
EOF
cat >"$work/driven.service" <<'EOF'
[Unit]
Description=driven by rctl
[Service]
Type=simple
ExecStart=/bin/sleep 2592000
Restart=no
StartLimitBurst=1000000
StartLimitIntervalSec=0
RestartSec=0
TimeoutStopSec=5
EOF
cat >"$work/churn.service" <<'EOF'
[Unit]
Description=restarts carefully, no output
[Service]
Type=simple
ExecStart=/bin/true
Restart=always
RestartSec=5
StartLimitBurst=1000000
StartLimitIntervalSec=0
EOF

ssh_guest() {
    timeout 30 ssh -p "$port" -i "$key" -o BatchMode=yes \
        -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -o LogLevel=ERROR -o ConnectTimeout=5 \
        -o ServerAliveInterval=10 -o ServerAliveCountMax=3 \
        root@127.0.0.1 "$@"
}

{
    printf 'started  %s\n' "$(date '+%F %T %Z')"
    printf 'commit   %s\n' "$(git -C "$repo_root" rev-parse HEAD)"
    printf 'arch     %s\n' "$arch"
    printf 'image    %s\n' "$(sha256sum "$disk" | cut -d' ' -f1)"
    printf 'kernel   %s\n' "$(sha256sum "$kernel" | cut -d' ' -f1)"
    printf 'qemu     %s\n' "$("$qemu_bin" --version | head -1)"
    printf 'port     %s\n' "$port"
    printf 'seed     %s\n' "$seed"
    printf 'feature  %s\n' "${feature_suite:-none}"
    printf 'duration %ss interval %ss checkpoint %ss\n' \
        "$seconds" "$interval" "$checkpoint_interval"
} >"$run_info"

# A long soak must not be reset by the hardware watchdog when the *host* is
# starved or suspended for more than the watchdog interval. The reboot test
# exercises the watchdog; the soak needs one uninterrupted guest. Pointing
# --watchdog at /dev/null leaves the device unarmed while keeping rinit's
# command line in the same shape.
timeout -k 5 $((seconds + 1800)) "$repo_root/scripts/run-qemu.sh" --arch "$arch" \
    --kernel "$kernel" --disk "$disk" --root /dev/vda --mem 128M \
    --hostfwd "tcp:127.0.0.1:$port-:22" \
    --kernel-args "-- --default device.target --watchdog /dev/null" \
    </dev/null >"$serial" 2>&1 &
qemu_pid=$!

ssh_ready=0
for _ in $(seq 1 120); do
    if ssh_guest 'true' >/dev/null 2>&1; then ssh_ready=1; break; fi
    sleep 1
done
if ((ssh_ready == 0)); then
    echo "FAIL: SSH did not come up" >&2
    tail -30 "$serial" >&2
    exit 3
fi

for unit in steady driven churn; do
    ssh_guest "cat > /etc/rinit/units/$unit.service" <"$work/$unit.service" || {
        echo "FAIL: could not install $unit.service" >&2
        exit 3
    }
done
ssh_guest 'cat > /root/soak-guest.sh' <"$repo_root/scripts/soak-guest.sh" || {
    echo "FAIL: could not install guest sampler" >&2
    exit 3
}
ssh_guest 'chmod 700 /root/soak-guest.sh' >/dev/null || exit 3

if [[ -n "$feature_suite" ]]; then
    tar -C "$repo_root/units/demo" -cf - . |
        ssh_guest 'tar -C /etc/rinit/units -xf -' || {
        echo "FAIL: could not install demo units" >&2
        exit 3
    }
    tar -C "$repo_root/tests/soak/units" -cf - . |
        ssh_guest 'tar -C /etc/rinit/units -xf -' || {
        echo "FAIL: could not install soak units" >&2
        exit 3
    }
    ssh_guest 'cat > /usr/local/bin/rinit-soak-helper' <"$helper" || {
        echo "FAIL: could not install helper" >&2
        exit 3
    }
    ssh_guest 'cat > /root/feature-suite.sh' <"$feature_suite" || {
        echo "FAIL: could not install feature suite" >&2
        exit 3
    }
    ssh_guest "chmod 700 /root/feature-suite.sh; chmod 755 /usr/local/bin/rinit-soak-helper; rctl reload" >/dev/null || {
        echo "FAIL: could not reload feature units" >&2
        exit 3
    }
    # SocketActivate sockets are bound during boot, not by a reload. Reboot
    # once before starting the soak so the feature units are part of that boot.
    ssh_guest 'reboot' >/dev/null 2>&1 || true
    boot_deadline=$((SECONDS + 180))
    while ((SECONDS < boot_deadline)); do
        [[ "$(grep -cF "rinit ${rinit_version} starting (pid 1)" "$serial")" -ge 2 ]] && break
        sleep 1
    done
    for _ in $(seq 1 180); do
        ssh_guest 'true' >/dev/null 2>&1 && break
        sleep 1
    done
    ssh_guest 'true' >/dev/null 2>&1 || {
        echo "FAIL: SSH did not return after the feature-unit reboot" >&2
        tail -30 "$serial" >&2
        exit 3
    }
fi

ssh_guest 'rctl reload' >/dev/null || {
    echo "FAIL: could not reload soak units" >&2
    exit 3
}
ssh_guest "rctl start steady.service; rctl start driven.service; rctl start churn.service" >/dev/null || {
    echo "FAIL: could not start soak units" >&2
    exit 3
}
ssh_guest "nohup sh /root/soak-guest.sh --seconds $seconds --interval $interval >/var/log/rinit/soak-guest.log 2>&1 &" || {
    echo "FAIL: could not start guest sampler" >&2
    exit 3
}
if [[ -n "$feature_suite" ]]; then
    ssh_guest "nohup sh /root/feature-suite.sh --seconds $seconds --seed $seed $feature_args >/var/log/rinit/feature-suite.log 2>&1 &" || {
        echo "FAIL: could not start feature suite" >&2
        exit 3
    }
fi

deadline=$((SECONDS + seconds + 1800))
last_checkpoint=$SECONDS
last_guest_elapsed=0
guest_rebooted=0
while ((SECONDS < deadline)); do
    if grep -q 'SOAK_DONE' "$serial"; then break; fi
    if ! kill -0 "$qemu_pid" 2>/dev/null; then break; fi
    # A second kernel boot means the guest reset: the soak is no longer
    # measuring one continuous uptime, so stop early instead of burning hours.
    if [[ "$(grep -cF 'Booting Linux on physical CPU' "$serial")" -gt 1 ]]; then
        printf 'FAIL %s guest rebooted during soak\n' "$(date '+%F %T %Z')" >>"$checkpoint_dir/log"
        guest_rebooted=1
        break
    fi
    if ((checkpoint_interval > 0 && SECONDS - last_checkpoint >= checkpoint_interval)); then
        stamp="$(date +%Y%m%d%H%M%S)"
        tmp="$checkpoint_dir/.guest-$stamp.csv"
        checkpoint_ok=0
        for _attempt in 1 2 3; do
            if ssh_guest 'cat /var/log/rinit/soak-guest.csv' >"$tmp" 2>/dev/null && [[ -s "$tmp" ]]; then
                mv "$tmp" "$checkpoint_dir/guest-$stamp.csv"
                printf 'ok   %s guest-%s.csv\n' "$(date '+%F %T %Z')" "$stamp" >>"$checkpoint_dir/log"
                checkpoint_ok=1
                break
            fi
            rm -f "$tmp"
            sleep 3
        done
        if ((checkpoint_ok == 0)); then
            printf 'FAIL %s ssh/checkpoint unavailable after retries\n' "$(date '+%F %T %Z')" >>"$checkpoint_dir/log"
        else
            # A stale CSV is worse than a missing one: after a guest reboot the
            # sampler is gone, but its old file is still readable. Require the
            # last elapsed value to move forward before calling this checkpoint
            # healthy.
            last_row="$(tail -1 "$checkpoint_dir/guest-$stamp.csv" 2>/dev/null || true)"
            elapsed="${last_row%%,*}"
            if [[ "$elapsed" =~ ^[0-9]+$ ]] && ((elapsed > last_guest_elapsed)); then
                last_guest_elapsed=$elapsed
            else
                printf 'FAIL %s guest CSV did not advance (elapsed=%s)\n' \
                    "$(date '+%F %T %Z')" "${elapsed:-missing}" >>"$checkpoint_dir/log"
            fi
        fi
        last_checkpoint=$SECONDS
    fi
    sleep 20
done

if [[ -n "$feature_suite" ]]; then
    feature_deadline=$((SECONDS + 900))
    while ((SECONDS < feature_deadline)); do
        grep -q 'FEATURE_DONE' "$serial" && break
        kill -0 "$qemu_pid" 2>/dev/null || break
        sleep 10
    done
fi

ssh_final=0
if ssh_guest 'true' >/dev/null 2>&1; then ssh_final=1; fi
csv_ok=0
if ((ssh_final)); then
    expected=$((seconds / interval))
    tmp="$guest_csv.partial"
    rm -f "$tmp"
    for attempt in 1 2 3; do
        if ssh_guest 'cat /var/log/rinit/soak-guest.csv' >"$tmp" 2>/dev/null; then
            rows=$(( $(wc -l <"$tmp") - 1 ))
            header="$(head -1 "$tmp")"
            if [[ "$header" == elapsed,* ]] && ((rows >= expected)); then
                mv "$tmp" "$guest_csv"
                csv_ok=1
                break
            fi
        fi
        sleep 2
    done
    if ((csv_ok == 0)); then
        [[ -e "$tmp" ]] && mv "$tmp" "$guest_csv.partial"
        echo "warning: final guest CSV is incomplete; kept as $guest_csv.partial" >&2
    fi
    ssh_guest 'cat /var/log/rinit/soak-guest.status' 2>/dev/null || true
fi

feature_ok=1
if [[ -n "$feature_suite" ]]; then
    feature_csv="$log_dir/soak-qemu-${tag}-features.csv"
    ssh_guest 'cat /var/log/rinit/feature-results.csv' >"$feature_csv" 2>/dev/null || true
    ssh_guest 'cat /var/log/rinit/feature-suite.status' 2>/dev/null || true
    feature_status=$(grep 'FEATURE_DONE' "$serial" | tail -1 || true)
    printf '%s\n' "$feature_status"
    [[ "$feature_status" == *'status=OK'* ]] || feature_ok=0
fi

serial_status=$(grep 'SOAK_DONE' "$serial" | tail -1 || true)
printf '%s\n' "$serial_status"
if ((ssh_final)); then
    echo 'SSH final: reachable'
else
    echo 'SSH final: unreachable'
fi
if ((csv_ok)); then
    echo "CSV final: $rows rows"
else
    echo 'CSV final: incomplete or unavailable'
fi

result=FAIL
if [[ "$serial_status" == *'status=OK'* ]] && ((ssh_final)) && ((csv_ok)) && ((feature_ok)); then
    result=OK
fi
if ((guest_rebooted)); then
    echo "SOAK FAIL tag=${tag} reason=guest-reboot samples=${guest_csv} serial=${serial}"
    exit 1
fi
echo "SOAK ${result} tag=${tag} samples=${guest_csv} serial=${serial}"
if [[ "$result" == OK ]]; then exit 0; fi
exit 1
