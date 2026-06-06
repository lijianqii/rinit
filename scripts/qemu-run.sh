#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."

INITRAMFS="target/initramfs/initramfs.cpio.gz"
KERNEL="/boot/vmlinuz-$(uname -r)"

if [ ! -f "$INITRAMFS" ]; then
    echo "Error: initramfs not found. Run mkinitramfs.sh first."
    exit 1
fi

echo "=== rinit QEMU Test Environment ==="
echo "Kernel:     $KERNEL"
echo "Initramfs:  $INITRAMFS"
echo "=================================="
echo ""
echo "When booted, rinit will start as PID 1."
echo "Log output goes to console (ttyS0)."
echo "Press Ctrl+A then X to quit QEMU."
echo ""

qemu-system-x86_64 \
    -kernel "$KERNEL" \
    -initrd "$INITRAMFS" \
    -append "console=ttyS0 quiet rinit.log_level=debug" \
    -nographic \
    -m 128M \
    -smp 2 \
    -no-reboot \
    "$@"
