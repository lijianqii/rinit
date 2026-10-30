#!/usr/bin/env bash
# Build the test-only guest helper for one architecture.
#
# Usage: scripts/build-soak-helper.sh aarch64|arm
#
# The helper is injected into the soak image by scripts/soak-qemu.sh; it is not
# part of the shipped image.
set -euo pipefail

arch="${1:?usage: build-soak-helper.sh aarch64|arm}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${here}/.." && pwd)"
out="${repo_root}/build/soak-helper-${arch}"

case "${arch}" in
    aarch64) cc=aarch64-linux-gnu-gcc ;;
    arm) cc=arm-linux-gnueabihf-gcc ;;
    *) echo "unsupported arch: ${arch}" >&2; exit 2 ;;
esac

command -v "${cc}" >/dev/null || {
    echo "error: ${cc} is not installed" >&2
    exit 2
}
mkdir -p "${repo_root}/build"
"${cc}" -O2 -static -Wall -Wextra -o "${out}" \
    "${repo_root}/tests/soak/helper/helper.c"
echo "soak-helper: ${out}"
