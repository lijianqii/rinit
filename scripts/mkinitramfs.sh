#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

RINIT_BIN="target/x86_64-unknown-linux-musl/release/rinit"
if [ ! -f "$RINIT_BIN" ]; then
    echo "Error: rinit not built. Run build-static.sh first."
    exit 1
fi

WORKDIR="/tmp/rinit-initramfs-$$"
echo "[1/5] Creating workdir: $WORKDIR"
rm -rf "$WORKDIR"
mkdir -p "$WORKDIR"/{bin,sbin,dev,proc,sys,run,etc/rinit/units}

echo "[2/5] Installing rinit as /init (kernel entry point)..."
cp "$RINIT_BIN" "$WORKDIR/init"
chmod +x "$WORKDIR/init"

echo "[3/5] Copying busybox for debugging..."
cp /usr/bin/busybox "$WORKDIR/bin/busybox"
chmod +x "$WORKDIR/bin/busybox"
for cmd in sh ls cat echo mount mkdir mknod sleep ps dmesg kill; do
    ln -sf /bin/busybox "$WORKDIR/bin/$cmd"
done

echo "[4/5] Creating /bin/init-fallback (busybox rescue shell)..."
cat > "$WORKDIR/bin/init-fallback" << 'INITEOF'
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
INITEOF
chmod +x "$WORKDIR/bin/init-fallback"

echo "[5/5] Packaging initramfs.cpio.gz..."
OUTDIR="target/initramfs"
mkdir -p "$OUTDIR"
OUTFILE="$OUTDIR/initramfs.cpio.gz"
( cd "$WORKDIR" && find . | cpio -o -H newc 2>/dev/null | gzip ) > "$OUTFILE"
echo "Done: $OUTFILE ($(du -h "$OUTFILE" | cut -f1))"
rm -rf "$WORKDIR"
