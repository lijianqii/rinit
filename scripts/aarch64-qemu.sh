#!/bin/bash
# =============================================================================
# rinit — aarch64 (ARM64) cross-build, initramfs, and QEMU boot
# =============================================================================
#
# Usage:
#   ./scripts/aarch64-qemu.sh /path/to/aarch64-kernel-Image
#
# Examples:
#   ./scripts/aarch64-qemu.sh ~/kernels/Image-6.12-arm64
#   ./scripts/aarch64-qemu.sh /boot/vmlinuz-6.12-arm64
#
# Prerequisites (install once):
#   sudo apt install -y gcc-aarch64-linux-gnu qemu-system-arm
#   rustup target add aarch64-unknown-linux-musl
# =============================================================================
set -euo pipefail
cd "$(dirname "$0")/.."

# -- constants --
BUSYBOX_VERSION="1.36.1"
BUSYBOX_URL="https://busybox.net/downloads/busybox-${BUSYBOX_VERSION}.tar.bz2"
BUSYBOX_DIR="target/busybox-${BUSYBOX_VERSION}"
RINIT_BIN="target/aarch64-unknown-linux-musl/release/rinit"
INITRAMFS="target/initramfs-aarch64/initramfs.cpio.gz"
CROSS_PREFIX="aarch64-linux-gnu-"
QEMU_MACHINE="virt"
QEMU_CPU="cortex-a57"
QEMU_MEM="128M"

banner() { echo "=== [$1]"; }
ok()    { echo "[OK] $1"; }
warn()  { echo "[WARN] $1"; }
die()   { echo "[FATAL] $1"; exit 1; }

# =========================== ARGUMENT PARSING ===============================

KERNEL="${1:-}"
if [ -z "$KERNEL" ]; then
    echo "Usage: $0 <kernel-image-path>"
    echo ""
    echo "  Provide the path to an aarch64 kernel Image (or vmlinux)."
    echo ""
    echo "  Examples:"
    echo "    $0 ~/kernels/linux-6.12/arch/arm64/boot/Image"
    echo "    $0 /boot/vmlinuz-6.12-arm64"
    exit 1
fi

if [ ! -f "$KERNEL" ]; then
    die "kernel not found: $KERNEL"
fi
ok "Kernel: $KERNEL"

# =========================== PREREQ CHECKS ==================================

banner "Checking prerequisites"

command -v "${CROSS_PREFIX}gcc" >/dev/null 2>&1 || \
    die "missing ${CROSS_PREFIX}gcc. Install: sudo apt install gcc-aarch64-linux-gnu"

command -v qemu-system-aarch64 >/dev/null 2>&1 || \
    die "missing qemu-system-aarch64. Install: sudo apt install qemu-system-arm"

ok "aarch64 cross-compiler found"
ok "qemu-system-aarch64 found"

# =========================== BUILD RINIT ====================================

banner "Building rinit for aarch64"

rustup target add aarch64-unknown-linux-musl 2>/dev/null || true

cargo build --release --target aarch64-unknown-linux-musl
aarch64-linux-gnu-strip "$RINIT_BIN" 2>/dev/null || strip "$RINIT_BIN" 2>/dev/null || true

ls -lh "$RINIT_BIN"
file "$RINIT_BIN"
ok "rinit built"

# =========================== BUILD BUSYBOX ==================================

banner "Building busybox ${BUSYBOX_VERSION} for aarch64"

WORKDIR="/tmp/rinit-aarch64-$$"
rm -rf "$WORKDIR"
mkdir -p "$WORKDIR"

if [ ! -f "$BUSYBOX_DIR/.built" ]; then
    if [ ! -d "$BUSYBOX_DIR" ]; then
        if [ ! -f "target/busybox-${BUSYBOX_VERSION}.tar.bz2" ]; then
            echo "  Downloading busybox ${BUSYBOX_VERSION}..."
            curl -sL "$BUSYBOX_URL" -o "target/busybox-${BUSYBOX_VERSION}.tar.bz2"
            ok "busybox tarball downloaded"
        fi
        echo "  Extracting..."
        mkdir -p target
        tar -xjf "target/busybox-${BUSYBOX_VERSION}.tar.bz2" -C target/
    fi

    echo "  Configuring (static, minimal)..."
    cd "$BUSYBOX_DIR"
    export ARCH=arm64
    export CROSS_COMPILE="$CROSS_PREFIX"
    make defconfig >/dev/null || die "busybox defconfig failed"
    sed -i 's/^# CONFIG_STATIC is not set/CONFIG_STATIC=y/' .config
    sed -i "s|^CONFIG_CROSS_COMPILER_PREFIX=.*|CONFIG_CROSS_COMPILER_PREFIX=\"${CROSS_PREFIX}\"|" .config
    # Disable tc (traffic control) — TCA_CBQ_* removed from kernel 6.12+ uapi headers
    sed -i 's/^CONFIG_TC=y$/# CONFIG_TC is not set/' .config
    # Re-sync dependencies after config changes
    make oldconfig >/dev/null 2>&1 || true
    echo "  Compiling busybox (this takes ~30s)..."
    if ! make -j"$(nproc)"; then
        cd ../..
        die "busybox compilation failed — see output above"
    fi
    touch .built
    cd ../..
    ok "busybox compiled"
else
    ok "busybox already built (cached)"
fi

# =========================== BUILD INITRAMFS ================================

banner "Creating initramfs"

mkdir -p "$WORKDIR"/{bin,sbin,dev,proc,sys,run,etc/rinit/units}

# rinit as /init
cp "$RINIT_BIN" "$WORKDIR/init"
chmod +x "$WORKDIR/init"

# busybox + symlinks
cp "$BUSYBOX_DIR/busybox" "$WORKDIR/bin/busybox"
chmod +x "$WORKDIR/bin/busybox"
for cmd in sh ls cat echo mount mkdir mknod sleep ps dmesg kill getty login; do
    ln -sf busybox "$WORKDIR/bin/$cmd"
done

# rescue shell
cat > "$WORKDIR/bin/init-fallback" << 'INIT_FALLBACK_EOF'
#!/bin/busybox sh
echo "rinit fallback: mounting proc/sys/dev..."
/bin/busybox --install -s
mount -t proc  proc  /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mount -t tmpfs  tmpfs /run
mkdir -p /dev/pts && mount -t devpts devpts /dev/pts
echo "Dropping to shell. Run /init to try rinit again."
exec /bin/sh
INIT_FALLBACK_EOF
chmod +x "$WORKDIR/bin/init-fallback"

# copy unit files into initramfs
cp config/getty.service.toml "$WORKDIR/etc/rinit/units/"
cp config/default.target.toml "$WORKDIR/etc/rinit/units/"

# package
mkdir -p "$(dirname "$INITRAMFS")"
( cd "$WORKDIR" && find . | cpio -o -H newc 2>/dev/null | gzip ) > "$INITRAMFS"

ok "initramfs: $INITRAMFS ($(du -h "$INITRAMFS" | cut -f1))"
rm -rf "$WORKDIR"

# =========================== BOOT WITH QEMU =================================

banner "Booting rinit on aarch64 QEMU"
echo ""
echo "  Machine:    ${QEMU_MACHINE}"
echo "  CPU:        ${QEMU_CPU}"
echo "  Memory:     ${QEMU_MEM}"
echo "  Kernel:     ${KERNEL}"
echo "  Initramfs:  ${INITRAMFS}"
echo ""
echo "  Press Ctrl+A then X to quit."
echo ""

qemu-system-aarch64 \
    -M "$QEMU_MACHINE" \
    -cpu "$QEMU_CPU" \
    -m "$QEMU_MEM" \
    -kernel "$KERNEL" \
    -initrd "$INITRAMFS" \
    -append "console=ttyAMA0 earlycon rinit.log_level=debug" \
    -nographic \
    -no-reboot \
    "$@"
