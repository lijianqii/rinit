#!/usr/bin/env bash
# Pack a prepared root directory into an ext4 filesystem image.
#
# Usage: scripts/mkdisk.sh <root-dir> <output.img>
#
# `mke2fs -d` populates the image without mounting it, so this works without
# root. The image is sized from the tree plus a 25% + 16 MiB margin; 4 KiB
# blocks and no reserved-block tax keep a small embedded image predictable.
set -euo pipefail

root="${1:?usage: mkdisk.sh <root-dir> <output.img>}"
out="${2:?usage: mkdisk.sh <root-dir> <output.img>}"
mke2fs="${MKE2FS:-}"
if [[ -z "${mke2fs}" ]]; then
    for candidate in /sbin/mke2fs /usr/sbin/mke2fs; do
        [[ -x "${candidate}" ]] && { mke2fs="${candidate}"; break; }
    done
fi

[[ -d "${root}" ]] || { echo "mkdisk: no root directory ${root}" >&2; exit 1; }
[[ -n "${mke2fs}" && -x "${mke2fs}" ]] || {
    echo "mkdisk: mke2fs not found (install e2fsprogs)" >&2
    exit 1
}

used_kb="$(du -sk "${root}" | cut -f1)"
size_kb=$((used_kb + used_kb / 4 + 16384))
(( size_kb < 65536 )) && size_kb=65536
blocks=$(( (size_kb + 3) / 4 ))

mkdir -p "$(dirname "${out}")"
rm -f "${out}"

mke2fs_args=(-q -t ext4 -b 4096 -I 256 -m 0 -L rinit -F)
tarball=""
use_tar=0
mke2fs_help="$("${mke2fs}" -h 2>&1 || true)"
if [[ "${MKDISK_FORCE_DIR:-0}" != 1 ]] &&
    grep -q 'root-directory|tarball' <<<"${mke2fs_help}"; then
    use_tar=1
fi

if ((use_tar)); then
    tarball="${out}.tar"
    trap 'rm -f "${tarball}"' EXIT
    # Older e2fsprogs accepts a tarball directly; its entries are explicitly
    # root:root, so /etc/dropbear, /root/.ssh and the units keep their owner.
    tar --owner=0:0 --group=0:0 -C "${root}" -cf "${tarball}" .
    "${mke2fs}" "${mke2fs_args[@]}" -d "${tarball}" "${out}" "${blocks}"
elif command -v fakeroot >/dev/null 2>&1; then
    # e2fsprogs 1.47.0 and older only accept a directory. `chown` inside
    # fakeroot changes the metadata mke2fs sees, not the build tree.
    fakeroot -- /bin/sh -c '
        chown -R 0:0 "$1" || exit 1
        shift
        exec "$@"
    ' sh "${root}" "${mke2fs}" "${mke2fs_args[@]}" -d "${root}" "${out}" "${blocks}"
elif command -v unshare >/dev/null 2>&1 && unshare -r true 2>/dev/null; then
    # Same root-owned view through a user namespace, without fakeroot.
    unshare -r --map-root-user "${mke2fs}" "${mke2fs_args[@]}" \
        -d "${root}" "${out}" "${blocks}"
else
    echo "mkdisk: warning: no fakeroot/user namespace; image files keep build-user ownership" >&2
    "${mke2fs}" "${mke2fs_args[@]}" -d "${root}" "${out}" "${blocks}"
fi
echo "mkdisk: wrote ${out} ($(du -h "${out}" | cut -f1), root ${used_kb} KiB)"
