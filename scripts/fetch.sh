#!/usr/bin/env bash
# Download a file, trying a list of URLs in order until one succeeds.
#
# Usage: scripts/fetch.sh <dest-file> <url> [url...]
#
# The first URL is expected to be the Tsinghua mirror; later URLs are upstream
# fallbacks (see the Makefile). Already-downloaded files are reused, so the
# build is resumable and `make run` only downloads once.
set -euo pipefail

dest="${1:?usage: fetch.sh <dest-file> <url> [url...]}"
shift
if [[ $# -eq 0 ]]; then
    echo "fetch: no URLs given for ${dest}" >&2
    exit 2
fi

if [[ -s "${dest}" ]]; then
    echo "fetch: using cached $(basename "${dest}")"
    exit 0
fi

mkdir -p "$(dirname "${dest}")"
part="${dest}.part"

download() {
    local url="$1"
    if command -v curl >/dev/null 2>&1; then
        curl -fL --retry 3 --connect-timeout 20 --progress-bar -o "${part}" "${url}"
    elif command -v wget >/dev/null 2>&1; then
        wget -O "${part}" "${url}"
    else
        echo "fetch: need curl or wget" >&2
        exit 127
    fi
}

for url in "$@"; do
    echo "fetch: trying ${url}"
    if download "${url}"; then
        mv "${part}" "${dest}"
        echo "fetch: saved ${dest} ($(du -h "${dest}" | cut -f1))"
        exit 0
    fi
    echo "fetch: failed: ${url}" >&2
done

echo "fetch: all sources failed for ${dest}" >&2
exit 1
