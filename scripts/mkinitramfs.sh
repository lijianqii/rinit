#!/usr/bin/env bash
# Pack a prepared root directory into a gzipped newc cpio (initramfs).
#
# Usage: scripts/mkinitramfs.sh <root-dir> <output.cpio.gz>
set -euo pipefail

root="${1:?usage: mkinitramfs.sh <root-dir> <output.cpio.gz>}"
out="${2:?usage: mkinitramfs.sh <root-dir> <output.cpio.gz>}"

mkdir -p "$(dirname "${out}")"
( cd "${root}" && find . -print0 | cpio --null -o --format=newc --quiet | gzip -9 ) >"${out}"
echo "mkinitramfs: wrote ${out} ($(du -h "${out}" | cut -f1))"
