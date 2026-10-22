#!/usr/bin/env bash
# Boot rinit as PID 1 in qemu (plan §4.3).
#
# Usage:
#   scripts/run-qemu.sh --arch <aarch64|arm|x86_64> --kernel <Image|zImage|vmlinuz>
#                       (--initrd <cpio.gz> | --disk <img> --root <dev>)
#                       [--mem 128M] [--hostfwd tcp::2222-:22]
#                       [--service "<cmd> [args...]"]
#                       [--kernel-args "<tokens>"]
#
# --disk attaches a raw ext4 image as virtio-blk. With --root the kernel mounts
# that image itself and executes /sbin/rinit from it, so there is no initramfs:
# this is the persistent `make run-disk` path.
#
# --service appends the hardcoded service command to the kernel cmdline; the
# kernel passes those tokens to rinit as argv (M0 has no config parsing yet).
# Example: --service "/bin/sh" gives an interactive busybox shell.
#
# --kernel-args appends further tokens to the cmdline, which is how the system
# test tunes rinit (`--kernel-args "--watchdog-timeout 10"`). Bare tokens reach
# rinit as argv; a token containing `=` does not - the kernel hands those to
# init as *environment* entries (`init/main.c`'s `unknown_bootoption`), so
# rinit's flags go in their space-separated form here.
#
# Examples:
#   # embedded target
#   scripts/run-qemu.sh --arch aarch64 --kernel Image --initrd initramfs-aarch64-unknown-linux-gnu.cpio.gz
#   scripts/run-qemu.sh --arch arm --kernel zImage --initrd initramfs-armv7-unknown-linux-gnueabihf.cpio.gz
#   # host/qemu-x86 verification (uses an amd64 vmlinuz)
#   scripts/run-qemu.sh --arch x86_64 --kernel /boot/vmlinuz-6.12.111+deb13-amd64 \
#       --initrd initramfs-x86_64-unknown-linux-musl.cpio.gz
#
# The aarch64 and arm32 machines also get qemu's user-mode network: DHCP hands
# the guest 10.0.2.15/24, gateway 10.0.2.2 and DNS 10.0.2.3, and
# `units/network.service` configures the interface from the lease.
#
# Exit from the guest: send SIGTERM/SIGINT to PID 1 (rinit reboots), or Ctrl-A X
# in qemu to quit.
set -euo pipefail

arch=""
kernel=""
initrd=""
disk=""
root=""
hostfwd=""
mem="128M"
service=""
kernel_args=""
extra=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch) arch="${2:?}"; shift 2 ;;
        --kernel) kernel="${2:?}"; shift 2 ;;
        --initrd) initrd="${2:?}"; shift 2 ;;
        --disk) disk="${2:?}"; shift 2 ;;
        --root) root="${2:?}"; shift 2 ;;
        --hostfwd) hostfwd="${2:?}"; shift 2 ;;
        --mem) mem="${2:?}"; shift 2 ;;
        --service) service="${2:?}"; shift 2 ;;
        --kernel-args) kernel_args="${2:?}"; shift 2 ;;
        -h | --help) sed -n '2,/^set -euo pipefail$/p' "$0" | sed '$d'; exit 0 ;;
        --) shift; extra=("$@"); break ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

: "${arch:?--arch required}"
: "${kernel:?--kernel required}"
if [[ -z "${initrd}" && -z "${disk}" ]]; then
    echo "error: --initrd or --disk is required" >&2
    exit 2
fi
if [[ -n "${root}" && -z "${disk}" ]]; then
    echo "error: --root requires --disk" >&2
    exit 2
fi

boot_args=(-kernel "${kernel}")
[[ -n "${initrd}" ]] && boot_args+=(-initrd "${initrd}")
[[ -n "${disk}" ]] && boot_args+=(-drive "file=${disk},format=raw,if=virtio")

netdev="user,id=net0"
[[ -n "${hostfwd}" ]] && netdev="${netdev},hostfwd=${hostfwd}"

case "${arch}" in
    aarch64)
        append="console=ttyAMA0"
        if [[ -n "${root}" ]]; then
            append="${append} root=${root} rw rootfstype=ext4 init=/sbin/rinit"
        else
            append="${append} rdinit=/sbin/rinit"
        fi
        [[ -n "${service}" ]] && append="${append} ${service}"
        [[ -n "${kernel_args}" ]] && append="${append} ${kernel_args}"
        # Plan §7.5's watchdog. The arm64 `virt` machine has no watchdog of its
        # own, and the `i6300esb` driver is built into the kernel the Makefile
        # builds (`CONFIG_I6300ESB_WDT=y`), so this is what gives the guest a
        # real `/dev/watchdog` to arm, set a timeout on and feed. The x86_64 path
        # below boots the host's `vmlinuz`, whose watchdog driver is a module
        # that is not in the initramfs, so the device would have nothing behind
        # it and rinit would report nothing - correct, but not worth a device.
        # qemu's user-mode network: its DHCP server hands the guest 10.0.2.15/24,
        # gateway 10.0.2.2 and DNS 10.0.2.3. The kernel has virtio-net and the
        # mmio transport built in, both checked by `make kernel-config-check`.
        exec qemu-system-aarch64 \
            -machine virt -cpu cortex-a57 -m "${mem}" \
            "${boot_args[@]}" \
            -append "${append}" \
            -netdev "${netdev}" \
            -device virtio-net-device,netdev=net0 \
            -device i6300esb \
            -nographic "${extra[@]}"
        ;;
    arm)
        append="console=ttyAMA0"
        if [[ -n "${root}" ]]; then
            append="${append} root=${root} rw rootfstype=ext4 init=/sbin/rinit"
        else
            append="${append} rdinit=/sbin/rinit"
        fi
        [[ -n "${service}" ]] && append="${append} ${service}"
        [[ -n "${kernel_args}" ]] && append="${append} ${kernel_args}"
        # 32-bit ARM `virt`: the same virtio-mmio NIC and PCI i6300esb as the
        # aarch64 machine, with a PL011 console. The kernel is a zImage and
        # qemu supplies the device tree for this machine. `highmem=off` keeps
        # the PCI ECAM below 4G; without it the 32-bit kernel's
        # pci-host-generic probe fails with EOVERFLOW and there is no watchdog.
        exec qemu-system-arm \
            -machine virt,highmem=off -cpu cortex-a15 -m "${mem}" \
            "${boot_args[@]}" \
            -append "${append}" \
            -netdev "${netdev}" \
            -device virtio-net-device,netdev=net0 \
            -device i6300esb \
            -nographic "${extra[@]}"
        ;;
    x86_64)
        append="console=ttyS0"
        if [[ -n "${root}" ]]; then
            append="${append} root=${root} rw rootfstype=ext4 init=/sbin/rinit"
        else
            append="${append} rdinit=/sbin/rinit"
        fi
        [[ -n "${service}" ]] && append="${append} ${service}"
        [[ -n "${kernel_args}" ]] && append="${append} ${kernel_args}"
        exec qemu-system-x86_64 \
            -machine q35 -m "${mem}" \
            "${boot_args[@]}" \
            -append "${append}" \
            -nographic "${extra[@]}"
        ;;
    *)
        echo "unsupported arch: ${arch}" >&2
        exit 2
        ;;
esac
