#!/usr/bin/env bash
# Pack a prepared root directory into a gzipped newc cpio (initramfs).
#
# Usage: scripts/mkinitramfs.sh <root-dir> <output.cpio.gz>
set -euo pipefail

root="${1:?usage: mkinitramfs.sh <root-dir> <output.cpio.gz>}"
out="${2:?usage: mkinitramfs.sh <root-dir> <output.cpio.gz>}"

mkdir -p "$(dirname "${out}")"
# `--owner=0:0` records root:root for every entry, which is what an initramfs
# should be: the tree is built by whatever user runs the build, and leaving that
# uid in the image means the image's `/` (and everything else) belongs to a user
# that exists on the build host. It matters for the same reason the mode of `/`
# does (plan §7.4): a service that drops privileges must not be able to write to
# directories that were merely left owned by the builder.
#
# This changes the *archive* only - the files on disk keep their owner - and
# needs no privileges, which is why it works when `chown -R` would not.
( cd "${root}" && find . -print0 | cpio --null -o --format=newc --quiet --owner=0:0 | gzip -9 ) >"${out}"
echo "mkinitramfs: wrote ${out} ($(du -h "${out}" | cut -f1))"
