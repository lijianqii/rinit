#!/usr/bin/env bash
# Assemble a gzipped cpio initramfs containing rinit (plan §4.3).
#
# Usage:
#   scripts/build-initramfs.sh <target-triple> [--busybox-src DIR] [--out FILE]
#
# Options:
#   --busybox-src DIR  install the given *built* busybox source tree
#                      (runs `make CONFIG_PREFIX=<root> install`) so a full
#                      applet set (sh/getty/login/mount/ps/...) is available.
#   --out FILE         output path (default: build/out/initramfs-<triple>.cpio.gz)
#
# Examples:
#   scripts/build-initramfs.sh aarch64-unknown-linux-gnu --busybox-src build/src/busybox-1.36.1
#   scripts/build-initramfs.sh x86_64-unknown-linux-musl         # rinit only
#
# The rinit binaries must be built first:
#   cargo build --release --target <triple>
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${here}/.." && pwd)"

triple=""
busybox_src=""
output=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --busybox-src) busybox_src="${2:?}"; shift 2 ;;
        --out) output="${2:?}"; shift 2 ;;
        -h | --help) sed -n '2,20p' "$0"; exit 0 ;;
        -*) echo "unknown option: $1" >&2; exit 2 ;;
        *) triple="$1"; shift ;;
    esac
done

: "${triple:?usage: build-initramfs.sh <target-triple> [--busybox-src DIR] [--out FILE]}"
[[ -n "${output}" ]] || output="${repo_root}/build/out/initramfs-${triple}.cpio.gz"

bin_dir="${repo_root}/target/${triple}/release"
if [[ ! -x "${bin_dir}/rinit" ]]; then
    echo "error: ${bin_dir}/rinit not found; run 'cargo build --release --target ${triple}'" >&2
    exit 1
fi
if [[ ! -x "${bin_dir}/rctl" ]]; then
    echo "error: ${bin_dir}/rctl not found; run 'cargo build --release --target ${triple}'" >&2
    exit 1
fi

work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

# The work directory *is* the image's root directory, and `mktemp -d` creates it
# as `0700`. That is invisible while everything in the guest runs as root - root
# has `CAP_DAC_OVERRIDE` and never notices - but it is the whole image's
# traversal bit: a service that drops privileges (plan §7.4) cannot so much as
# `execve("/bin/sh")` through a root directory it may not search, and fails with
# `EACCES` that looks like a missing binary. Everything else in the layout is
# already world-readable; this is the one entry that is not.
chmod 0755 "${work}"

# Directory layout (plan §4.3). No /init symlink: we boot with rdinit=.
install -d "${work}"/{sbin,bin,etc/rinit/units,proc,sys,dev,run,tmp,root}

install -m 0755 "${bin_dir}/rinit" "${work}/sbin/rinit"
# The control client goes where a user can type it. It is useless without the
# socket rinit creates at boot (plan §1.3(10)), which is why the two ship
# together.
install -m 0755 "${bin_dir}/rctl" "${work}/bin/rctl"

# rinit-testservice is deliberately NOT packed. It is a M0 bring-up helper used
# to prove fork/exec/reap, and it is still built into target/ for the host
# smoke test; shipping a ~1 MB binary that no unit refers to would only bloat
# the image. The guest's services come from units/ and busybox.

# Plan §1.3(9)'s second wave: the mount table rinit reads after the first wave.
# It ships in the image because the second wave runs at *boot* - a table written
# at run time would be too late - and it is the repo's `fstab` verbatim so the
# guest's evidence points at a file instead of at the packing script.
install -m 0644 "${repo_root}/fstab" "${work}/etc/fstab"

# The unit directory, which is what the guest actually boots.
for glob in "${repo_root}"/units/*.service "${repo_root}"/units/*.target; do
    [[ -e "${glob}" ]] || continue
    install -m 0644 "${glob}" "${work}/etc/rinit/units/"
done

# Optional busybox: install the full built tree (creates applet symlinks).
if [[ -n "${busybox_src}" ]]; then
    if [[ ! -x "${busybox_src}/busybox" ]]; then
        echo "error: ${busybox_src}/busybox not built" >&2
        exit 1
    fi
    # busybox's Makefile only reads the cross prefix from the command line or
    # CONFIG_CROSS_COMPILER_PREFIX in .config (defconfig leaves it empty).
    # Without passing it here the install step silently recompiles busybox with
    # the HOST compiler and overwrites the cross-built binary.
    cross=""
    bbox_arch=""
    case "${triple}" in
        aarch64-*) cross="aarch64-linux-gnu-"; bbox_arch="arm64" ;;
    esac
    if [[ -n "${cross}" ]]; then
        make -C "${busybox_src}" CONFIG_PREFIX="${work}" CROSS_COMPILE="${cross}" ARCH="${bbox_arch}" install >/dev/null
    else
        make -C "${busybox_src}" CONFIG_PREFIX="${work}" install >/dev/null
    fi
    # An *empty* password field, not `x`.
    #
    # `x` means "the password lives in /etc/shadow", which this initramfs does
    # not have - busybox login then refuses every password, including the empty
    # one, and the console sits at a prompt nobody can get past. Leaving the
    # field empty means "no password required", which is what makes the console
    # usable with `make run`.
    #
    # This is safe only because the serial port is the sole access path and the
    # image is a development initramfs; anyone who can reach it already has the
    # machine. See units/shell.service.
    #
    # `nobody`/`nogroup` are here for the §7.4 acceptance test: a unit that sets
    # `User=`/`Group=` has to be seen dropping to a user that is *not* root, and
    # 65534 is the conventional id for that user. It is a name rather than a
    # number so the test exercises the `/etc/passwd`/`/etc/group` lookup too.
    printf 'root::0:0:root:/root:/bin/sh\nnobody:x:65534:65534:nobody:/nonexistent:/sbin/nologin\n' >"${work}/etc/passwd"
    printf 'root:x:0:\nnogroup:x:65534:\n' >"${work}/etc/group"
fi

"${here}/mkinitramfs.sh" "${work}" "${output}"
