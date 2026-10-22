#!/usr/bin/env bash
# Create a swap file inside a prepared rootfs tree.
#
# Usage: scripts/mkswapfile.sh <path> [size-in-MiB]
#
# The file is created with `dd` rather than `fallocate`: ext4 swap files must
# have every block allocated, and a sparse file is rejected by swapon. Fill it
# with non-zero data too: mke2fs -d turns an all-zero tar member into a sparse
# inode, which swapon then rejects with "file has holes". The caller runs this
# before scripts/mkdisk.sh, so the image contains the header and `swap.service`
# only has to call swapon at boot.
set -euo pipefail

out="${1:?usage: mkswapfile.sh <path> [size-in-MiB]}"
size_mb="${2:-32}"

[[ "${size_mb}" =~ ^[0-9]+$ ]] && ((size_mb >= 1)) || {
    echo "mkswapfile: size must be a positive integer (MiB)" >&2
    exit 2
}
mkswap="${MKSWAP:-}"
if [[ -z "${mkswap}" ]]; then
    for candidate in /sbin/mkswap /usr/sbin/mkswap; do
        [[ -x "${candidate}" ]] && { mkswap="${candidate}"; break; }
    done
fi
[[ -n "${mkswap}" && -x "${mkswap}" ]] || {
    echo "mkswapfile: mkswap not found (install util-linux)" >&2
    exit 1
}

mkdir -p "$(dirname "${out}")"
rm -f "${out}"
dd if=/dev/urandom of="${out}" bs=1M count="${size_mb}" status=none
chmod 0600 "${out}"
"${mkswap}" -L rinit-swap "${out}" >/dev/null
echo "mkswapfile: ${out} (${size_mb} MiB)"
