#!/usr/bin/env bash
# Boot rinit as PID 1 in qemu (plan §4.3).
#
# Usage:
#   scripts/run-qemu.sh --arch <aarch64|x86_64> --kernel <Image|vmlinuz> --initrd <cpio.gz>
#                       [--mem 128M] [--service "<cmd> [args...]"]
#
# --service appends the hardcoded service command to the kernel cmdline; the
# kernel passes those tokens to rinit as argv (M0 has no config parsing yet).
# Example: --service "/bin/sh" gives an interactive busybox shell.
#
# Examples:
#   # embedded target
#   scripts/run-qemu.sh --arch aarch64 --kernel Image --initrd initramfs-aarch64-unknown-linux-gnu.cpio.gz
#   # host/qemu-x86 verification (uses an amd64 vmlinuz)
#   scripts/run-qemu.sh --arch x86_64 --kernel /boot/vmlinuz-6.12.111+deb13-amd64 \
#       --initrd initramfs-x86_64-unknown-linux-musl.cpio.gz
#
# Exit from the guest: send SIGTERM/SIGINT to PID 1 (rinit reboots), or Ctrl-A X
# in qemu to quit.
set -euo pipefail

arch=""
kernel=""
initrd=""
mem="128M"
service=""
extra=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch) arch="${2:?}"; shift 2 ;;
        --kernel) kernel="${2:?}"; shift 2 ;;
        --initrd) initrd="${2:?}"; shift 2 ;;
        --mem) mem="${2:?}"; shift 2 ;;
        --service) service="${2:?}"; shift 2 ;;
        -h | --help) sed -n '2,20p' "$0"; exit 0 ;;
        --) shift; extra=("$@"); break ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

: "${arch:?--arch required}"
: "${kernel:?--kernel required}"
: "${initrd:?--initrd required}"

case "${arch}" in
    aarch64)
        append="console=ttyAMA0 rdinit=/sbin/rinit"
        [[ -n "${service}" ]] && append="${append} ${service}"
        exec qemu-system-aarch64 \
            -machine virt -cpu cortex-a57 -m "${mem}" \
            -kernel "${kernel}" -initrd "${initrd}" \
            -append "${append}" \
            -nographic "${extra[@]}"
        ;;
    x86_64)
        append="console=ttyS0 rdinit=/sbin/rinit"
        [[ -n "${service}" ]] && append="${append} ${service}"
        exec qemu-system-x86_64 \
            -machine q35 -m "${mem}" \
            -kernel "${kernel}" -initrd "${initrd}" \
            -append "${append}" \
            -nographic "${extra[@]}"
        ;;
    *)
        echo "unsupported arch: ${arch}" >&2
        exit 2
        ;;
esac
