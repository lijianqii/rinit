#!/usr/bin/env bash
# Regression tests for scripts/install-tools.sh. Uses the real aarch64 release
# binaries and a scratch rootfs; no root and no qemu required.
set -u

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bin_src="$repo_root/target/aarch64-unknown-linux-gnu/release"
installer="$repo_root/scripts/install-tools.sh"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/install-tools-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

[[ -x "$bin_src/rctl" ]] || {
    echo "install-tools-test: missing $bin_src/rctl (build the aarch64 target first)" >&2
    exit 2
}

pass=0
fail=0
check() {
    local name=$1
    shift
    if "$@"; then
        echo "ok   $name"
        pass=$((pass + 1))
    else
        echo "FAIL $name"
        fail=$((fail + 1))
    fi
}

new_case() {
    root="$tmp/$1/root"
    bin="$tmp/$1/bin"
    mkdir -p "$root/bin" "$bin"
    touch "$root/bin/busybox"
    cp "$bin_src/rctl" "$bin/mount"
    cat >"$tmp/$1.list" <<EOF
mount mount /bin/mount /bin/mount 1024 crates/rinit-tools/src/bin/mount.rs
EOF
}

# 1. replace-system installs a real file and removes the BusyBox link.
new_case success
ln -s /bin/busybox "$root/bin/mount"
"$installer" --rootfs "$root" --bin-dir "$bin" \
    --manifest "$tmp/success.list" --arch aarch64 >/dev/null
check 'replace-system installs a real file' test -f "$root/bin/mount"
check 'replace-system removes the BusyBox link' test ! -L "$root/bin/mount"
check 'replace-system leaves /bin/busybox' test -f "$root/bin/busybox"

# 2. A non-BusyBox symlink is refused.
new_case wronglink
ln -s /bin/other "$root/bin/mount"
"$installer" --rootfs "$root" --bin-dir "$bin" \
    --manifest "$tmp/wronglink.list" --arch aarch64 >/dev/null 2>&1
rc=$?
check 'non-BusyBox symlink is refused' test "$rc" -ne 0
check 'refused link is left intact' test -L "$root/bin/mount"

# 3. Missing binary is fatal unless --allow-missing is explicit.
new_case missing
rm -f "$bin/mount"
"$installer" --rootfs "$root" --bin-dir "$bin" \
    --manifest "$tmp/missing.list" --arch aarch64 >/dev/null 2>&1
rc=$?
check 'missing binary is fatal' test "$rc" -ne 0
"$installer" --rootfs "$root" --bin-dir "$bin" \
    --manifest "$tmp/missing.list" --arch aarch64 --allow-missing >/dev/null 2>&1
check '--allow-missing skips an unbuilt tool' test "$?" -eq 0

# 4. A wrong interpreter is rejected before the BusyBox link is touched.
new_case wrongelf
cp /bin/true "$bin/mount"
ln -s /bin/busybox "$root/bin/mount"
"$installer" --rootfs "$root" --bin-dir "$bin" \
    --manifest "$tmp/wrongelf.list" --arch aarch64 >/dev/null 2>&1
rc=$?
check 'wrong interpreter is rejected' test "$rc" -ne 0
check 'failed ELF check leaves the BusyBox link' test -L "$root/bin/mount"

# 5. Size budget is enforced before removal too.
new_case toobig
sed -i 's/ 1024 / 1 /' "$tmp/toobig.list"
ln -s /bin/busybox "$root/bin/mount"
"$installer" --rootfs "$root" --bin-dir "$bin" \
    --manifest "$tmp/toobig.list" --arch aarch64 >/dev/null 2>&1
rc=$?
check 'size budget is enforced' test "$rc" -ne 0
check 'failed size check leaves the BusyBox link' test -L "$root/bin/mount"

# 5b. The whole manifest has a total budget, checked before any install.
new_case total
ln -s /bin/busybox "$root/bin/mount"
TOOLS_TOTAL_BUDGET_KIB=1 "$installer" --rootfs "$root" --bin-dir "$bin" \
    --manifest "$tmp/total.list" --arch aarch64 >/dev/null 2>&1
rc=$?
check 'total size budget is enforced' test "$rc" -ne 0
check 'total size failure leaves the BusyBox link' test -L "$root/bin/mount"

# 6. coexist keeps BusyBox and uses an rinit- prefix.
new_case coexist
ln -s /bin/busybox "$root/bin/mount"
"$installer" --rootfs "$root" --bin-dir "$bin" \
    --manifest "$tmp/coexist.list" --arch aarch64 --profile coexist >/dev/null
check 'coexist installs rinit-mount' test -f "$root/bin/rinit-mount"
check 'coexist keeps /bin/mount as BusyBox' test -L "$root/bin/mount"

# 7. A tool whose install path is not the BusyBox applet does not need a
# prefix: /bin/login stays BusyBox, /bin/rlogin is the new binary, and the
# shipped getty.service can keep calling /sbin/rgetty.
different="$tmp/different"
mkdir -p "$different/root/bin" "$different/bin"
touch "$different/root/bin/busybox"
ln -s /bin/busybox "$different/root/bin/login"
cp "$bin_src/rctl" "$different/bin/rlogin"
cat >"$different.list" <<EOF
rlogin rlogin /bin/rlogin /bin/login 1024 crates/rinit-tools/src/bin/rlogin/main.rs
EOF
"$installer" --rootfs "$different/root" --bin-dir "$different/bin" \
    --manifest "$different.list" --arch aarch64 --profile coexist >/dev/null
check 'coexist installs /bin/rlogin at its own path' test -f "$different/root/bin/rlogin"
check 'coexist keeps /bin/login as BusyBox' test -L "$different/root/bin/login"

echo "install-tools-test: $pass passed, $fail failed"
exit $((fail > 0))
