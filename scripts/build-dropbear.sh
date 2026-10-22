#!/usr/bin/env bash
# Build a static Dropbear (server + key generator) for one rinit target.
#
# Usage:
#   scripts/build-dropbear.sh <target-triple> <out-dir>
#
# The source tarball is cached in build/dl/. `dropbear` and `dropbearkey` are
# installed into <out-dir>; the image builder copies them into /usr/sbin and
# /usr/bin. dbclient is deliberately not shipped: the device is a server, and
# the host's ssh client is the client.
set -euo pipefail

triple="${1:?usage: build-dropbear.sh <target-triple> <out-dir>}"
out="${2:?usage: build-dropbear.sh <target-triple> <out-dir>}"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${here}/.." && pwd)"
version="${DROPBEAR_VERSION:-2026.94}"
tarball="${repo_root}/build/dl/dropbear-${version}.tar.bz2"
src_root="${repo_root}/build/src"
case "${triple}" in
    aarch64-*)
        src_root="${repo_root}/build/src"
        cross="aarch64-linux-gnu-"
        host="aarch64-linux-gnu"
        ;;
    armv7-*)
        src_root="${repo_root}/build/arm32/src"
        cross="arm-linux-gnueabihf-"
        host="arm-linux-gnueabihf"
        ;;
    *)
        echo "unsupported target for dropbear: ${triple}" >&2
        exit 2
        ;;
esac
src="${src_root}/dropbear-${version}"

mkdir -p "${repo_root}/build/dl" "${src_root}" "${out}"
out="$(cd "${out}" && pwd)"
if [[ ! -f "${tarball}" ]]; then
    echo "downloading dropbear ${version}..." >&2
    curl -fsSL --retry 3 --max-time 180 \
        "https://matt.ucc.asn.au/dropbear/releases/dropbear-${version}.tar.bz2" \
        -o "${tarball}" || {
        echo "error: could not download dropbear; put dropbear-${version}.tar.bz2 in build/dl/" >&2
        exit 1
    }
fi
if [[ ! -d "${src}" ]]; then
    tar -C "${src_root}" -xf "${tarball}"
fi

cd "${src}"
# No zlib/PAM/NSS helpers: the image is static and has no shared NSS modules.
# Password auth is compiled out (`-s` at runtime is not enough: the ARM
# toolchain has no static libcrypt and the default config then refuses to
# compile). Logs go to stderr, which rinit captures into its ring/file.
make clean >/dev/null 2>&1 || true
CFLAGS="-O2 -DDROPBEAR_SVR_PASSWORD_AUTH=0" \
./configure --host="${host}" \
    --disable-zlib --disable-pam --disable-utmp --disable-wtmp \
    --disable-lastlog --disable-syslog --disable-shadow --disable-harden >/dev/null
make -j"${JOBS:-$(nproc 2>/dev/null || echo 2)}" \
    PROGRAMS="dropbear dropbearkey" LDFLAGS="-static" >/dev/null

install -m 0755 dropbear dropbearkey "${out}/"
for bin in dropbear dropbearkey; do
    file "${out}/${bin}" | grep -q 'statically linked' || {
        echo "error: ${bin} is not static" >&2
        exit 1
    }
done
echo "build-dropbear: ${out}/dropbear"
