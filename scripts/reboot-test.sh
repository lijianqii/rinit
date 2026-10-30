#!/usr/bin/env bash
# Reboot-persistence regression test for the ext4 device image.
#
# The source image is never booted: the script copies it, boots the copy, seeds
# a marker, /etc/hostname and a unit wanted by device.target, then:
#   1. reboots through rinit's SIGTERM path and verifies everything survived;
#   2. kills qemu with SIGKILL, boots again and verifies ext4 journal recovery.
#
# The image must already contain the public half of --key in
# /root/.ssh/authorized_keys (see "Persistent device image" in README.md).
#
# Usage:
#   scripts/reboot-test.sh --key PATH [options]
#
#   --arch aarch64|arm   qemu machine (default aarch64)
#   --image FILE         ext4 device image (default depends on --arch)
#   --kernel FILE        kernel Image/zImage (default depends on --arch)
#   --key FILE           SSH private key; required
#   --port N             host port forwarded to guest 22 (default 2222)
#   --tag NAME           names the log and working image (default reboot)
#   --log-dir DIR        serial log directory (default build/logs)
#   --keep-image         keep the working copy even when the test passes
#   -h, --help           show this help
#
# Exit codes: 0 pass, 1 check failure, 2 usage error, 3 setup/boot failure.
set -u -o pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
arch=aarch64
image=""
kernel=""
key=""
port=2222
tag=reboot
log_dir="$repo_root/build/logs"
keep_image=0

usage() {
    sed -n '2,/^set -u -o pipefail$/p' "$0" | sed '$d'
}

die() {
    echo "reboot-test: $*" >&2
    exit 2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch) arch="${2:?--arch needs a value}"; shift 2 ;;
        --image) image="${2:?--image needs a value}"; shift 2 ;;
        --kernel) kernel="${2:?--kernel needs a value}"; shift 2 ;;
        --key) key="${2:?--key needs a value}"; shift 2 ;;
        --port) port="${2:?--port needs a value}"; shift 2 ;;
        --tag) tag="${2:?--tag needs a value}"; shift 2 ;;
        --log-dir) log_dir="${2:?--log-dir needs a value}"; shift 2 ;;
        --keep-image) keep_image=1; shift ;;
        -h | --help) usage; exit 0 ;;
        *) die "unknown argument: $1" ;;
    esac
done

case "$arch" in
    aarch64)
        default_image="$repo_root/build/out/rinit-aarch64-unknown-linux-gnu.ext4"
        default_kernel="$repo_root/build/src/linux-6.6.158/arch/arm64/boot/Image"
        qemu_bin=qemu-system-aarch64
        ;;
    arm)
        default_image="$repo_root/build/arm32/out/rinit-armv7-unknown-linux-gnueabihf.ext4"
        default_kernel="$repo_root/build/arm32/src/linux-6.6.158/arch/arm/boot/zImage"
        qemu_bin=qemu-system-arm
        ;;
    *) die "--arch must be aarch64 or arm" ;;
esac

image="${image:-$default_image}"
kernel="${kernel:-$default_kernel}"
[[ -n "$key" ]] || die "--key is required"
[[ "$port" =~ ^[0-9]+$ ]] && ((port >= 1 && port <= 65535)) || die "--port must be 1..65535"
[[ "$tag" =~ ^[A-Za-z0-9._-]+$ ]] || die "--tag may contain only letters, digits, dot, underscore and dash"
[[ "$log_dir" = /* ]] || log_dir="$repo_root/$log_dir"

for f in "$image" "$kernel" "$key"; do
    [[ -e "$f" ]] || die "missing file: $f"
done
command -v "$qemu_bin" >/dev/null || die "$qemu_bin is not installed"

mkdir -p "$log_dir" "$repo_root/build/out"
log="$log_dir/reboot-$tag.log"
run_info="$log_dir/reboot-$tag-run.txt"
work_image="$repo_root/build/out/reboot-$tag.ext4"
src_real="$(realpath -m "$image")"
work_real="$(realpath -m "$work_image")"
[[ "$src_real" != "$work_real" ]] || die "--image must not be the working copy $work_image"

rm -f "$log" "$work_image"
cp --reflink=auto "$image" "$work_image" || die "cannot copy $image"
{
    printf 'started  %s\n' "$(date '+%F %T %Z')"
    printf 'commit   %s\n' "$(git -C "$repo_root" rev-parse HEAD 2>/dev/null || echo unknown)"
    printf 'arch     %s\n' "$arch"
    printf 'image    %s\n' "$image"
    printf 'image_sha256 %s\n' "$(sha256sum "$image" | cut -d' ' -f1)"
    printf 'kernel   %s\n' "$kernel"
    printf 'kernel_sha256 %s\n' "$(sha256sum "$kernel" | cut -d' ' -f1)"
    printf 'qemu     %s\n' "$("$qemu_bin" --version | head -1)"
    printf 'port     %s\n' "$port"
} >"$run_info"

marker="REBOOT-MARKER-$(date +%Y%m%d%H%M%S)-$RANDOM"
power_marker="REBOOT-POWERLOSS-$(date +%Y%m%d%H%M%S)-$RANDOM"
qemu_pid=""
boots=0
checks=0
fail=0

cleanup() {
    local status=$?
    trap - EXIT
    if [[ -n "$qemu_pid" ]] && kill -0 "$qemu_pid" 2>/dev/null; then
        kill "$qemu_pid" 2>/dev/null || true
    fi
    wait "$qemu_pid" 2>/dev/null || true
    if ((keep_image == 0)); then
        if ((status == 0)); then
            rm -f "$work_image"
        elif [[ -e "$work_image" ]]; then
            echo "reboot-test: kept failed image $work_image" >&2
        fi
    fi
    exit "$status"
}
trap cleanup EXIT

ssh_guest() {
    timeout 20 ssh -p "$port" -i "$key" -o BatchMode=yes \
        -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -o LogLevel=ERROR \
        -o ConnectTimeout=5 -o ServerAliveInterval=5 -o ServerAliveCountMax=2 \
        root@127.0.0.1 "$@"
}

log_count() {
    grep -cF -- "$1" "$log" 2>/dev/null || true
}

wait_log_count() {
    local pattern="$1" wanted="$2" timeout_s="$3"
    local deadline=$((SECONDS + timeout_s))
    while ((SECONDS < deadline)); do
        [[ "$(log_count "$pattern")" -ge "$wanted" ]] && return 0
        sleep 1
    done
    return 1
}

port_in_use() {
    command -v ss >/dev/null || return 1
    ss -ltn 2>/dev/null | grep -q "[:.]$port "
}

wait_port_free() {
    local deadline=$((SECONDS + 60))
    while ((SECONDS < deadline)); do
        port_in_use || return 0
        sleep 1
    done
    return 1
}

start_qemu() {
    if port_in_use; then
        echo "reboot-test: port $port is already in use" >&2
        return 1
    fi
    "$repo_root/scripts/run-qemu.sh" --arch "$arch" \
        --kernel "$kernel" --disk "$work_image" --root /dev/vda \
        --hostfwd "tcp::$port-:22" --kernel-args "-- --default device.target" \
        </dev/null >>"$log" 2>&1 &
    qemu_pid=$!
    sleep 1
    kill -0 "$qemu_pid" 2>/dev/null
}

wait_ssh() {
    local timeout_s="$1"
    local deadline=$((SECONDS + timeout_s))
    while ((SECONDS < deadline)); do
        if [[ -n "$qemu_pid" ]] && ! kill -0 "$qemu_pid" 2>/dev/null; then return 1; fi
        if ssh_guest 'true' >/dev/null 2>&1; then return 0; fi
        sleep 1
    done
    return 1
}

boot_first() {
    start_qemu || { echo "reboot-test: qemu did not start" >&2; exit 3; }
    if ! wait_ssh 180; then
        echo "reboot-test: first boot did not reach SSH" >&2
        tail -40 "$log" >&2
        exit 3
    fi
    boots=1
}

reboot_guest() {
    local want=$((boots + 1))
    timeout 5 ssh -p "$port" -i "$key" -o BatchMode=yes \
        -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -o LogLevel=ERROR -o ConnectTimeout=5 \
        root@127.0.0.1 'reboot' >/dev/null 2>&1 || true
    if ! wait_log_count 'rinit 0.1.0 starting (pid 1)' "$want" 180; then
        echo "reboot-test: boot $want did not start" >&2
        tail -40 "$log" >&2
        exit 3
    fi
    if ! wait_ssh 180; then
        echo "reboot-test: SSH did not return on boot $want" >&2
        tail -40 "$log" >&2
        exit 3
    fi
    boots=$want
}

check() {
    local name="$1" cmd="$2" result
    checks=$((checks + 1))
    if result=$(eval "$cmd" 2>&1); then
        echo "ok   $name"
    else
        echo "FAIL $name"
        printf '%s\n' "$result" | sed 's/^/     /'
        fail=$((fail + 1))
    fi
}

echo "reboot-test: tag=$tag marker=$marker"
boot_first

ssh_guest "cat > /etc/hostname" <<'EOF'
rinit-reboot
EOF
ssh_guest "printf '%s\\n' '$marker' > /root/reboot-marker" || exit 3
ssh_guest "cat > /etc/rinit/units/reboot-probe.service" <<'EOF'
[Unit]
Description=reboot persistence probe
Requires=rinit-init.target
After=rinit-init.target

[Service]
Type=oneshot
ExecStart=/bin/sh -c "echo boot >> /var/lib/reboot-probe-count"
EOF
ssh_guest "grep -q '^Wants=' /etc/rinit/units/device.target" || {
    echo "reboot-test: device.target has no Wants= line to extend" >&2
    exit 3
}
ssh_guest "sed -i 's/^Wants=.*/& reboot-probe.service/' /etc/rinit/units/device.target" || exit 3
ssh_guest 'mkdir -p /var/lib; rm -f /var/lib/reboot-probe-count; sync' || exit 3

echo "--- graceful reboot ---"
reboot_guest

check 'shutdown logged SIGTERM' "[[ \$(grep -cF 'received SIGTERM' '$log') -ge 1 ]]"
check 'shutdown logged rebooting' "[[ \$(grep -cF 'rebooting' '$log') -ge 1 ]]"
check 'persist marker survived' "[[ \"\$(ssh_guest cat /root/reboot-marker)\" == \"$marker\" ]]"
check 'hostname survived' "[[ \"\$(ssh_guest hostname)\" == 'rinit-reboot' ]]"
check 'probe ran once' "[[ \$(ssh_guest 'wc -l < /var/lib/reboot-probe-count' | tr -d ' ') == 1 ]]"
check 'probe is active' "ssh_guest 'rctl status --plain reboot-probe.service' | grep -q 'state: active'"
check 'device.target active' "ssh_guest 'rctl status --plain device.target' | grep -q 'state: active'"
check 'sshd active' "ssh_guest 'rctl status --plain sshd.service' | grep -q 'state: active'"
check 'mdev active' "ssh_guest 'rctl status --plain mdev.service' | grep -q 'state: active'"
check 'swap active' "ssh_guest 'cat /proc/swaps' | grep -q '/swapfile'"
check 'authorized key still works' "ssh_guest 'true'"
check 'no ext4 error after graceful reboot' "! ssh_guest 'dmesg' | grep -qiE 'EXT4-fs error|I/O error'"
check 'no panic after graceful reboot' "! grep -qF 'PANIC' '$log'"

echo "--- power loss ---"
ssh_guest "printf '%s\\n' '$power_marker' > /root/reboot-powerloss-marker; dd if=/dev/zero of=/var/lib/reboot-powerloss-data bs=1024 count=4096 2>/dev/null; sync" || exit 3
kill -9 "$qemu_pid" 2>/dev/null || true
wait "$qemu_pid" 2>/dev/null || true
qemu_pid=""
if ! wait_port_free; then
    echo "reboot-test: port $port stayed busy after power loss" >&2
    exit 3
fi
start_qemu || { echo "reboot-test: qemu did not restart after power loss" >&2; exit 3; }
if ! wait_log_count 'rinit 0.1.0 starting (pid 1)' 3 180; then
    echo "reboot-test: guest did not boot after power loss" >&2
    tail -40 "$log" >&2
    exit 3
fi
if ! wait_ssh 180; then
    echo "reboot-test: SSH did not return after power loss" >&2
    tail -40 "$log" >&2
    exit 3
fi
boots=3

check 'power-loss marker survived' "[[ \"\$(ssh_guest cat /root/reboot-powerloss-marker)\" == \"$power_marker\" ]]"
check 'original marker survived power loss' "[[ \"\$(ssh_guest cat /root/reboot-marker)\" == \"$marker\" ]]"
check 'hostname survived power loss' "[[ \"\$(ssh_guest hostname)\" == 'rinit-reboot' ]]"
check 'probe ran twice' "[[ \$(ssh_guest 'wc -l < /var/lib/reboot-probe-count' | tr -d ' ') == 2 ]]"
check 'device.target active after power loss' "ssh_guest 'rctl status --plain device.target' | grep -q 'state: active'"
check 'sshd active after power loss' "ssh_guest 'rctl status --plain sshd.service' | grep -q 'state: active'"
check 'mdev active after power loss' "ssh_guest 'rctl status --plain mdev.service' | grep -q 'state: active'"
check 'swap active after power loss' "ssh_guest 'cat /proc/swaps' | grep -q '/swapfile'"
check 'rctl list works after power loss' "ssh_guest 'rctl list' | grep -q 'device.target'"
check 'no ext4 error after power loss' "! ssh_guest 'dmesg' | grep -qiE 'EXT4-fs error|I/O error'"
check 'no panic after power loss' "! grep -qF 'PANIC' '$log'"

echo "--- journal messages ---"
ssh_guest 'dmesg' | grep -iE 'EXT4-fs|recovery|orphan' | tail -8 || true

if ((fail == 0)); then
    echo "REBOOT OK tag=$tag checks=$checks"
else
    echo "REBOOT FAIL tag=$tag checks=$checks failures=$fail"
fi
echo "log=$log"
echo "run=$run_info"
if ((keep_image)); then
    echo "image=$work_image"
fi
exit $((fail > 0))
