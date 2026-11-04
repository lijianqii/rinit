#!/usr/bin/env bash
# Boot a replace-system device image and exercise the installed tools.
#
# This is the tool-level counterpart to device-smoke.sh: it checks that the
# standard paths are real ELFs, that BusyBox still owns the remaining applets,
# and that the commands produce usable output.  A separate --power-cycle mode
# additionally checks `reboot` and `poweroff` through PID 1.
#
# Usage:
#   scripts/tools-test.sh --key PATH [--arch aarch64|arm] [options]
#
#   --image FILE     device image (default depends on --arch)
#   --kernel FILE    kernel (default depends on --arch)
#   --key FILE       SSH private key; required
#   --port N         host port (default 2222)
#   --tag NAME       log tag (default tools-test)
#   --log-dir DIR    log directory (default build/logs)
#   --power-cycle    test reboot then poweroff (ends the VM)
#   -h, --help       show this help
#
# Exit codes: 0 pass, 1 check failure, 2 usage, 3 boot/setup failure.
set -u

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
arch=aarch64
image=""
kernel=""
key=""
port=2222
tag=tools-test
log_dir="$repo_root/build/logs"
power_cycle=0

die() { echo "tools-test: $*" >&2; exit 2; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch) arch="${2:?}"; shift 2 ;;
        --image) image="${2:?}"; shift 2 ;;
        --kernel) kernel="${2:?}"; shift 2 ;;
        --key) key="${2:?}"; shift 2 ;;
        --port) port="${2:?}"; shift 2 ;;
        --tag) tag="${2:?}"; shift 2 ;;
        --log-dir) log_dir="${2:?}"; shift 2 ;;
        --power-cycle) power_cycle=1; shift ;;
        -h | --help) sed -n '2,/^set -u$/p' "$0" | sed '$d'; exit 0 ;;
        *) die "unknown argument: $1" ;;
    esac
done

case "$arch" in
    aarch64)
        : "${image:=$repo_root/build/out/rinit-aarch64-unknown-linux-gnu.ext4}"
        : "${kernel:=$repo_root/build/src/linux-6.6.158/arch/arm64/boot/Image}"
        ;;
    arm)
        : "${image:=$repo_root/build/arm32/out/rinit-armv7-unknown-linux-gnueabihf.ext4}"
        : "${kernel:=$repo_root/build/arm32/src/linux-6.6.158/arch/arm/boot/zImage}"
        ;;
    *) die "--arch must be aarch64 or arm" ;;
esac
[[ -n "$key" ]] || die "--key is required"
[[ -d "$log_dir" ]] || mkdir -p "$log_dir"
[[ -e "$image" && -e "$kernel" && -e "$key" ]] || die "image, kernel or key is missing"

serial="$log_dir/tools-$tag.log"
rm -f "$serial"
qemu_pid=""
cleanup() {
    [[ -n "$qemu_pid" ]] && kill "$qemu_pid" 2>/dev/null || true
    wait "$qemu_pid" 2>/dev/null || true
}
trap cleanup EXIT

ssh_guest() {
    timeout 30 ssh -p "$port" -i "$key" -o BatchMode=yes \
        -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -o LogLevel=ERROR -o ConnectTimeout=5 \
        root@127.0.0.1 "$@"
}

wait_ssh() {
    local deadline=$((SECONDS + 150))
    while ((SECONDS < deadline)); do
        ssh_guest true >/dev/null 2>&1 && return 0
        sleep 1
    done
    return 1
}

checks=0
fail=0
check() {
    local name="$1" cmd="$2" out
    checks=$((checks + 1))
    if out=$(eval "$cmd" 2>&1); then
        echo "ok   $name"
    else
        echo "FAIL $name"
        printf '%s\n' "$out" | sed 's/^/     /'
        fail=$((fail + 1))
    fi
}

"$repo_root/scripts/run-qemu.sh" --arch "$arch" \
    --kernel "$kernel" --disk "$image" --root /dev/vda \
    --hostfwd "tcp:127.0.0.1:$port-:22" --kernel-args "-- --default device.target" \
    </dev/null >"$serial" 2>&1 &
qemu_pid=$!

wait_ssh || { echo "FAIL: SSH did not come up" >&2; tail -30 "$serial" >&2; exit 3; }

for path in mount umount swapon swapoff hostname ps free uptime dmesg df sysctl top reboot poweroff \
    processes ls cat cp mv rm mkdir ln touch echo sleep uname mktemp sync stat rlogin \
    id whoami env which dirname basename realpath readlink rgetty; do
    case "$path" in
        mount | umount | hostname | ps | dmesg | df | processes | ls | cat | cp | mv | rm | mkdir | ln | touch | echo | sleep | uname | mktemp | sync | stat | rlogin) full="/bin/$path" ;;
        free | uptime | top | id | whoami | env | which | dirname | basename | realpath | readlink) full="/usr/bin/$path" ;;
        sysctl | reboot | poweroff | rgetty | swapon | swapoff) full="/sbin/$path" ;;
    esac
    check "$full is a real executable" "ssh_guest 'test -x $full && ! test -L $full'"
done
check 'BusyBox remains' "ssh_guest 'test -x /bin/busybox && test -L /bin/sh && busybox ls / >/dev/null'"

check 'hostname reads' "ssh_guest 'hostname' | grep -q ."
check 'hostname -F sets the name' "ssh_guest 'printf \"tools-test\\n\" >/tmp/h; hostname -F /tmp/h; test \"\$(hostname)\" = tools-test'"
check 'hostname restores' "ssh_guest 'hostname rinit'"
check 'ps prints a table' "ssh_guest 'ps' | grep -q 'PID.*USER.*COMMAND'"
check 'free prints memory' "ssh_guest 'free' | grep -q 'Mem:'"
check 'uptime prints load' "ssh_guest 'uptime' | grep -q 'load average:'"
check 'df prints a filesystem' "ssh_guest 'df' | grep -q '/dev/root'"
check 'dmesg reads the ring' "ssh_guest 'dmesg' | grep -q 'Booting Linux'"
check 'sysctl reads a key' "ssh_guest 'sysctl kernel.hostname' | grep -q 'kernel.hostname ='"
check 'sysctl -w writes a key' "ssh_guest 'sysctl -w kernel.hostname=tools-sysctl >/dev/null; test \"\$(hostname)\" = tools-sysctl; sysctl -w kernel.hostname=rinit >/dev/null'"
check 'top batch snapshot' "ssh_guest 'top -b -n 1' | grep -q 'Tasks:'"
check 'mount/umount tmpfs' "ssh_guest 'mkdir -p /tmp/toolmnt; mount -t tmpfs tmpfs /tmp/toolmnt; grep -q \" /tmp/toolmnt \" /proc/mounts; umount /tmp/toolmnt; ! grep -q \" /tmp/toolmnt \" /proc/mounts'"
check 'devpts is mounted from fstab' "ssh_guest 'grep -q \" /dev/pts \" /proc/mounts'"
check 'mount -a reads a temporary fstab' "ssh_guest 'printf \"tmpfs /tmp/toolmnt-a tmpfs rw 0 0\\n\" >/tmp/toolmnt-a.fstab; mount -o bind /tmp/toolmnt-a.fstab /etc/fstab; mkdir -p /tmp/toolmnt-a; mount -a; ok=\$?; grep -q \" /tmp/toolmnt-a \" /proc/mounts || ok=1; umount /tmp/toolmnt-a || ok=1; umount /etc/fstab || ok=1; rm -f /tmp/toolmnt-a.fstab; exit \$ok'"
check 'mount -o ro is applied' "ssh_guest 'mkdir -p /tmp/toolmnt-ro; mount -t tmpfs -o ro tmpfs /tmp/toolmnt-ro; grep -q \" /tmp/toolmnt-ro tmpfs ro\" /proc/mounts; umount /tmp/toolmnt-ro'"
check 'mount fails on a missing target' "! ssh_guest 'mount -t tmpfs tmpfs /no/such/tool-target' >/dev/null 2>&1"
check 'umount fails on an unknown target' "! ssh_guest 'umount /no/such/tool-target' >/dev/null 2>&1"
check 'BusyBox mount fallback' "ssh_guest 'busybox mount' | grep -q ' / '"
check 'swapon -s reports the boot swap' "ssh_guest 'swapon -s' | grep -q '/swapfile'"
check 'swapoff and swapon handle one device' "ssh_guest 'swapoff /swapfile && ! grep -q /swapfile /proc/swaps && swapon /swapfile && grep -q /swapfile /proc/swaps'"
check 'swapoff -a and swapon -a round trip' "ssh_guest 'swapoff -a && ! grep -q /swapfile /proc/swaps && swapon -a && grep -q /swapfile /proc/swaps'"

check 'ls lists a file' "ssh_guest 'ls /etc/hostname' | grep -q hostname"
check 'cat reads a file' "ssh_guest 'cat /etc/hostname' | grep -q rinit"
check 'cp copies a file' "ssh_guest 'cp /etc/hostname /tmp/tool-cp && cmp -s /etc/hostname /tmp/tool-cp'"
check 'mv moves a file' "ssh_guest 'mv /tmp/tool-cp /tmp/tool-mv && test -f /tmp/tool-mv && ! test -e /tmp/tool-cp'"
check 'rm removes a file' "ssh_guest 'rm /tmp/tool-mv && ! test -e /tmp/tool-mv'"
check 'mkdir creates parents' "ssh_guest 'mkdir -p /tmp/tool-dir/a/b && test -d /tmp/tool-dir/a/b'"
check 'ln creates a symlink' "ssh_guest 'ln -s /etc/hostname /tmp/tool-link && test -L /tmp/tool-link'"
check 'touch creates a file' "ssh_guest 'touch /tmp/tool-touch && test -f /tmp/tool-touch'"
check 'cp refuses a directory without -r' "! ssh_guest 'cp /etc /tmp/tool-cp-dir' >/dev/null 2>&1"
check 'rm refuses a directory without -r' "! ssh_guest 'rm /etc' >/dev/null 2>&1"

check '/bin/echo prints its arguments' "ssh_guest '/bin/echo hello' | grep -q hello"
check 'sleep accepts zero' "ssh_guest 'sleep 0'"
check 'uname reports Linux' "ssh_guest 'uname -s' | grep -q '^Linux$'"
check 'id prints a numeric uid' "ssh_guest 'id -u' | grep -qE '^[0-9]+\$'"
check 'whoami prints a name' "ssh_guest 'whoami' | grep -q ."
check 'env prints PATH' "ssh_guest 'env' | grep -q '^PATH='"
check 'which finds sh' "ssh_guest 'which sh' | grep -q sh"
check 'dirname strips the last component' "ssh_guest 'test \"\$(dirname /a/b)\" = /a'"
check 'basename strips directory and suffix' "ssh_guest 'test \"\$(basename /a/b.txt .txt)\" = b'"
check 'realpath resolves an existing path' "ssh_guest 'realpath /etc/hostname' | grep -q '^/etc/hostname$'"
check 'readlink prints a symlink target' "ssh_guest 'ln -s /etc/hostname /tmp/tool-readlink && test \"\$(readlink /tmp/tool-readlink)\" = /etc/hostname'"
check 'mktemp creates a file' "ssh_guest 'p=\$(mktemp /tmp/tool-tmp.XXXXXX) && test -f \"\$p\" && rm -f \"\$p\"'"
check 'sync succeeds' "ssh_guest 'sync'"
check 'stat prints a file name and size' "ssh_guest 'stat -c \"%n %s\" /etc/hostname' | grep -q hostname"
check 'rlogin --help' "ssh_guest '/bin/rlogin --help' | grep -q Usage"
check 'rgetty --help' "ssh_guest '/sbin/rgetty --help' | grep -q Usage"
check 'old login/getty links are gone' "ssh_guest '! test -e /bin/login && ! test -e /sbin/getty'"
ssh_guest 'rm -rf /tmp/tool-dir /tmp/tool-link /tmp/tool-touch /tmp/tool-cp-dir /tmp/tool-readlink' >/dev/null 2>&1 || true

if ((power_cycle)); then
    boot_count=$(grep -cF 'starting (pid 1)' "$serial" || true)
    check 'reboot asks PID 1' "ssh_guest 'reboot'"
    deadline=$((SECONDS + 180))
    while ((SECONDS < deadline)); do
        [[ "$(grep -cF 'starting (pid 1)' "$serial" 2>/dev/null)" -gt "$boot_count" ]] && break
        sleep 1
    done
    check 'reboot started a new boot' "[[ \$(grep -cF 'starting (pid 1)' '$serial') -gt $boot_count ]]"
    wait_ssh || { echo "FAIL: SSH after reboot" >&2; exit 3; }
    check 'poweroff exits qemu' "ssh_guest 'poweroff'"
    for _ in $(seq 1 60); do
        kill -0 "$qemu_pid" 2>/dev/null || break
        sleep 1
    done
    check 'qemu stopped after poweroff' "! kill -0 $qemu_pid 2>/dev/null"
    wait "$qemu_pid" 2>/dev/null || true
    qemu_pid=""
    check 'poweroff reached the shutdown path' "grep -qF 'powering off' '$serial'"
fi

echo "TOOLS $( ((fail == 0)) && echo OK || echo FAIL) checks=$checks failures=$fail"
echo "log=$serial"
exit $((fail > 0))
