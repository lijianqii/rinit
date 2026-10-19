#!/usr/bin/env bash
# Pack a prepared root directory into a gzipped newc cpio (initramfs).
#
# Usage: scripts/mkinitramfs.sh <root-dir> <output.cpio.gz>
set -euo pipefail

root="${1:?usage: mkinitramfs.sh <root-dir> <output.cpio.gz>}"
out="${2:?usage: mkinitramfs.sh <root-dir> <output.cpio.gz>}"

mkdir -p "$(dirname "${out}")"
# Every entry gets the epoch as its mtime, so the same sources pack into the same
# bytes. `install` (and busybox's own `make install`) stamp each file with the
# build time and cpio records it, so without this the image differs by a few bytes
# on every build and its size cannot be *checked* - only re-measured, which is
# what the README had to do before this line existed. `-h` covers the applet
# symlinks; nothing in the tree reads a file's mtime (the reload diffs contents,
# the log mirror reads a length), so the epoch is as good as any other value.
find "${root}" -exec touch -h -d @0 {} +

# `--owner=0:0` records root:root for every entry, which is what an initramfs
# should be: the tree is built by whatever user runs the build, and leaving that
# uid in the image means the image's `/` (and everything else) belongs to a user
# that exists on the build host. It matters for the same reason the mode of `/`
# does (plan §7.4): a service that drops privileges must not be able to write to
# directories that were merely left owned by the builder.
#
# This changes the *archive* only - the files on disk keep their owner - and
# needs no privileges, which is why it works when `chown -R` would not.
#
# `--reproducible` is the other half of the same property: it zeroes the device
# and inode numbers the newc format would otherwise record, and the packing root
# is a fresh `mktemp -d` on every run, so those numbers are different every time.
# It does *not* touch mtimes (that is the `touch` above) - a build needs both, and
# with both the same tree packs to the same bytes, which is what lets the README
# quote a size instead of an approximation.
( cd "${root}" && find . -print0 | cpio --null -o --format=newc --quiet --owner=0:0 --reproducible | gzip -9 ) >"${out}"
echo "mkinitramfs: wrote ${out} ($(du -h "${out}" | cut -f1))"
