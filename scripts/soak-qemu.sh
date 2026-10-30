#!/usr/bin/env bash
# Host-side driver for a guest-side run-disk soak.
#
# The guest sampler is started once over SSH, then runs independently of the
# host connection. This avoids making the whole 24 h run depend on 1,440 SSH
# sessions; the serial console still carries a compact heartbeat if SSH fails.
#
# Usage: scripts/soak-qemu.sh --key PATH [--arch aarch64|arm] [--seconds N]
#                             [--interval N] [--tag NAME] [--kernel FILE]
#                             [--disk FILE]
set -u

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
arch=aarch64
kernel=""
disk=""
key=""
seconds=3600
interval=60
tag="qemu-soak"
log_dir="$repo_root/build/soak-charts"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch) arch="${2:?}"; shift 2 ;;
        --kernel) kernel="${2:?}"; shift 2 ;;
        --disk) disk="${2:?}"; shift 2 ;;
        --key) key="${2:?}"; shift 2 ;;
        --seconds) seconds="${2:?}"; shift 2 ;;
        --interval) interval="${2:?}"; shift 2 ;;
        --tag) tag="${2:?}"; shift 2 ;;
        -h | --help) sed -n '2,/^set -u$/p' "$0" | sed '$d'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

case "$arch" in
    aarch64)
        : "${kernel:=$repo_root/build/src/linux-6.6.158/arch/arm64/boot/Image}"
        : "${disk:=$repo_root/build/out/rinit-aarch64-unknown-linux-gnu.ext4}"
        qemu_bin=qemu-system-aarch64
        ;;
    arm)
        : "${kernel:=$repo_root/build/arm32/src/linux-6.6.158/arch/arm/boot/zImage}"
        : "${disk:=$repo_root/build/arm32/out/rinit-armv7-unknown-linux-gnueabihf.ext4}"
        qemu_bin=qemu-system-arm
        ;;
    *) echo "error: --arch must be aarch64 or arm" >&2; exit 2 ;;
esac

[[ -n "$key" ]] || { echo "error: --key is required" >&2; exit 2; }
for f in "$kernel" "$disk" "$key"; do
    [[ -e "$f" ]] || { echo "error: missing $f" >&2; exit 2; }
done
mkdir -p "$log_dir"
serial="$log_dir/soak-qemu-${tag}.log"
guest_csv="$log_dir/soak-qemu-${tag}-guest.csv"
run_info="$log_dir/soak-qemu-${tag}-run.txt"
rm -f "$serial" "$guest_csv" "$run_info"

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
    ssh -p 2222 -i "$key" -o BatchMode=yes -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 \
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
    printf 'duration %ss interval %ss\n' "$seconds" "$interval"
} >"$run_info"

timeout -k 5 $((seconds + 1800)) "$repo_root/scripts/run-qemu.sh" --arch "$arch" \
    --kernel "$kernel" --disk "$disk" --root /dev/vda \
    --hostfwd tcp::2222-:22 --kernel-args "-- --default device.target" \
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
ssh_guest "chmod 700 /root/soak-guest.sh; rctl reload; rctl start steady.service; rctl start driven.service; rctl start churn.service" >/dev/null || {
    echo "FAIL: could not start soak units" >&2
    exit 3
}
ssh_guest "nohup sh /root/soak-guest.sh --seconds $seconds --interval $interval >/var/log/rinit/soak-guest.log 2>&1 &" || {
    echo "FAIL: could not start guest sampler" >&2
    exit 3
}

deadline=$((SECONDS + seconds + 1800))
while ((SECONDS < deadline)); do
    if grep -q 'SOAK_DONE' "$serial"; then break; fi
    if ! kill -0 "$qemu_pid" 2>/dev/null; then break; fi
    sleep 20
done

ssh_final=0
if ssh_guest 'true' >/dev/null 2>&1; then ssh_final=1; fi
if ((ssh_final)); then
    ssh_guest 'cat /var/log/rinit/soak-guest.csv' >"$guest_csv" 2>/dev/null || true
    ssh_guest 'cat /var/log/rinit/soak-guest.status' 2>/dev/null || true
fi

serial_status=$(grep 'SOAK_DONE' "$serial" | tail -1 || true)
printf '%s\n' "$serial_status"
if ((ssh_final)); then
    echo 'SSH final: reachable'
else
    echo 'SSH final: unreachable'
fi

result=FAIL
if [[ "$serial_status" == *'status=OK'* ]] && ((ssh_final)); then
    result=OK
fi
echo "SOAK ${result} tag=${tag} samples=${guest_csv} serial=${serial}"
if [[ "$result" == OK ]]; then exit 0; fi
exit 1
