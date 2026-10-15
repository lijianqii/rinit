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
for bin in rinit rinit-testservice; do
    if [[ ! -x "${bin_dir}/${bin}" ]]; then
        echo "error: ${bin_dir}/${bin} not found; run 'cargo build --release --target ${triple}'" >&2
        exit 1
    fi
done

work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

# Directory layout (plan §4.3). No /init symlink: we boot with rdinit=.
install -d "${work}"/{sbin,bin,etc/rinit/units,proc,sys,dev,run,tmp,root}

install -m 0755 "${bin_dir}/rinit" "${work}/sbin/rinit"
install -m 0755 "${bin_dir}/rinit-testservice" "${work}/sbin/rinit-testservice"

# Example unit files (M0 does not parse them yet; included for layout parity).
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
    make -C "${busybox_src}" CONFIG_PREFIX="${work}" install >/dev/null
    printf 'root:x:0:0:root:/root:/bin/sh\n' >"${work}/etc/passwd"
    printf 'root:x:0:\n' >"${work}/etc/group"
fi

"${here}/mkinitramfs.sh" "${work}" "${output}"
