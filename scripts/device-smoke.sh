#!/usr/bin/env bash
# Boot a run-disk device image and assert that its device services came up.
#
# This is the short gate used by CI: ext4 root, devpts, swap, mdev, sshd,
# syslog/klogd, device.target and rctl, plus the absence of panics or ext4
# errors. It does not modify the image beyond normal boot writes.
#
# The image must already contain the public half of --key in
# /root/.ssh/authorized_keys. `make disk-image AUTHORIZED_KEY=<file.pub>` does
# that; the key is generated per CI run and never committed.
#
# Usage:
#   scripts/device-smoke.sh --key PATH [options]
#
#   --arch aarch64|arm   qemu machine (default aarch64)
#   --image FILE         ext4 device image (default depends on --arch)
#   --kernel FILE        kernel Image/zImage (default depends on --arch)
#   --key FILE           SSH private key; required
#   --port N             host port forwarded to guest 22 (default 2222)
#   --tag NAME           names the log (default device-smoke)
#   --log-dir DIR        serial log directory (default build/logs)
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
tag=device-smoke
log_dir="$repo_root/build/logs"

usage() {
    sed -n '2,/^set -u -o pipefail$/p' "$0" | sed '$d'
}

die() {
    echo "device-smoke: $*" >&2
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
[[ "$port" =~ ^[0-9]+$ ]] && ((10#$port >= 1 && 10#$port <= 65535)) || die "--port must be 1..65535"
[[ "$tag" =~ ^[A-Za-z0-9._-]+$ ]] || die "--tag may contain only letters, digits, dot, underscore and dash"
[[ "$log_dir" = /* ]] || log_dir="$repo_root/$log_dir"

for f in "$image" "$kernel" "$key"; do
    [[ -e "$f" ]] || die "missing file: $f"
done
command -v "$qemu_bin" >/dev/null || die "$qemu_bin is not installed"

mkdir -p "$log_dir"
log="$log_dir/device-$tag.log"
run_info="$log_dir/device-$tag-run.txt"
rm -f "$log"
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

qemu_pid=""
checks=0
fail=0

cleanup() {
    local status=$?
    trap - EXIT
    if [[ -n "$qemu_pid" ]] && kill -0 "$qemu_pid" 2>/dev/null; then
        kill "$qemu_pid" 2>/dev/null || true
    fi
    wait "$qemu_pid" 2>/dev/null || true
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

port_in_use() {
    command -v ss >/dev/null || return 1
    ss -ltn 2>/dev/null | grep -q "[:.]$port "
}

kill_qemu() {
    if [[ -n "$qemu_pid" ]] && kill -0 "$qemu_pid" 2>/dev/null; then
        kill "$qemu_pid" 2>/dev/null || true
    fi
    wait "$qemu_pid" 2>/dev/null || true
    qemu_pid=""
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

if port_in_use; then
    die "port $port is already in use"
fi

"$repo_root/scripts/run-qemu.sh" --arch "$arch" \
    --kernel "$kernel" --disk "$image" --root /dev/vda \
    --hostfwd "tcp::$port-:22" --kernel-args "-- --default device.target" \
    </dev/null >"$log" 2>&1 &
qemu_pid=$!

deadline=$((SECONDS + 180))
while ((SECONDS < deadline)); do
    if ! kill -0 "$qemu_pid" 2>/dev/null; then
        echo "device-smoke: qemu exited before SSH" >&2
        tail -40 "$log" >&2
        exit 3
    fi
    if ssh_guest 'true' >/dev/null 2>&1; then break; fi
    sleep 1
done
if ! ssh_guest 'true' >/dev/null 2>&1; then
    echo "device-smoke: SSH did not come up" >&2
    tail -40 "$log" >&2
    exit 3
fi
echo "device-smoke: SSH ready"

check 'root is ext4 and writable' "ssh_guest 'grep \" / \" /proc/mounts' | grep -q 'ext4 rw'"
check 'devpts is mounted' "ssh_guest 'grep \" /dev/pts \" /proc/mounts'"
check 'swapfile is active' "ssh_guest 'cat /proc/swaps' | grep -q '/swapfile'"
check 'device.target is active' "ssh_guest 'rctl status --plain device.target' | grep -q 'state: active'"
check 'sshd is active' "ssh_guest 'rctl status --plain sshd.service' | grep -q 'state: active'"
check 'mdev is active' "ssh_guest 'rctl status --plain mdev.service' | grep -q 'state: active'"
check 'syslog is active' "ssh_guest 'rctl status --plain syslog.service' | grep -q 'state: active'"
check 'klogd is active' "ssh_guest 'rctl status --plain klogd.service' | grep -q 'state: active'"
check 'network-online is active' "ssh_guest 'rctl status --plain network-online.service' | grep -q 'state: active'"
check '/dev/vda exists' "ssh_guest 'test -e /dev/vda'"
check '/dev/ptmx exists' "ssh_guest 'test -e /dev/ptmx'"
check 'rctl list includes the target' "ssh_guest 'rctl list' | grep -q 'device.target'"
check 'mount is a real ELF path' "ssh_guest 'test -x /bin/mount && ! test -L /bin/mount'"
check 'hostname is a real ELF path' "ssh_guest 'test -x /bin/hostname && ! test -L /bin/hostname'"
check 'reboot is a real ELF path' "ssh_guest 'test -x /sbin/reboot && ! test -L /sbin/reboot'"
check 'ps runs' "ssh_guest 'ps >/dev/null'"
check 'free runs' "ssh_guest 'free >/dev/null'"
check 'uptime runs' "ssh_guest 'uptime >/dev/null'"
check 'df runs' "ssh_guest 'df >/dev/null'"
check 'top batch snapshot runs' "ssh_guest 'top -b -n 1 >/dev/null'"
check 'BusyBox remains available' "ssh_guest 'test -x /bin/busybox && test -L /bin/sh'"

kill_qemu

check 'rinit booted exactly once' "[[ \$(grep -cF 'starting (pid 1)' '$log') -eq 1 ]]"
check 'no kernel panic or oops' "! grep -qiE 'rinit: PANIC|Kernel panic|Oops' '$log'"
check 'no ext4 or I/O errors' "! grep -qiE 'EXT4-fs error|I/O error' '$log'"

if ((fail == 0)); then
    echo "DEVICE SMOKE OK tag=$tag checks=$checks"
else
    echo "DEVICE SMOKE FAIL tag=$tag checks=$checks failures=$fail"
fi
echo "log=$log"
echo "run=$run_info"
exit $((fail > 0))
