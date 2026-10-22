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
tarball="${out}.tar"
trap 'rm -f "${tarball}"' EXIT
# mke2fs -d on a *directory* preserves the build user's uid/gid. Feed it a tar
# whose entries are explicitly root:root instead, so /etc/dropbear, /root/.ssh
# and the units are owned correctly in the guest.
tar --owner=0:0 --group=0:0 -C "${root}" -cf "${tarball}" .
"${mke2fs}" -q -t ext4 -b 4096 -I 256 -m 0 -L rinit -F \
    -d "${tarball}" "${out}" "${blocks}"
echo "mkdisk: wrote ${out} ($(du -h "${out}" | cut -f1), root ${used_kb} KiB)"
