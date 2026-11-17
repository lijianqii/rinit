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
expected="${SHA256:-}"

verify() {
    [[ -z "${expected}" ]] && return 0
    if printf '%s  %s\n' "${expected}" "${dest}" | sha256sum -c --status -; then
        return 0
    fi
    echo "fetch: SHA-256 mismatch for ${dest}" >&2
    return 1
}

if [[ -s "${dest}" ]]; then
    if verify; then
        echo "fetch: using cached $(basename "${dest}")"
        exit 0
    fi
    echo "fetch: discarding cached ${dest}" >&2
    rm -f "${dest}"
fi

mkdir -p "$(dirname "${dest}")"
# A PID suffix keeps two concurrent `make` invocations from writing the same
# partial file; the trap removes it on every exit path.
part="${dest}.part.$$"
trap 'rm -f "${part}"' EXIT

download() {
    local url="$1"
    if command -v curl >/dev/null 2>&1; then
        # A stalled mirror must not hold the build until the job timeout. The
        # 900 s ceiling is comfortably above a slow-but-progressing download.
        curl -fL --retry 3 --retry-delay 2 --connect-timeout 20 --max-time 900 \
            --progress-bar -o "${part}" "${url}"
    elif command -v wget >/dev/null 2>&1; then
        wget --timeout=60 --tries=3 -O "${part}" "${url}"
    else
        echo "fetch: need curl or wget" >&2
        exit 127
    fi
}

for url in "$@"; do
    echo "fetch: trying ${url}"
    rm -f "${part}"
    if download "${url}"; then
        mv "${part}" "${dest}"
        if ! verify; then
            # A mirror can serve an error page with status 200. That is a bad
            # source, not a reason to stop before trying upstream.
            rm -f "${dest}"
            echo "fetch: ${url} did not match the expected SHA-256; trying the next source" >&2
            continue
        fi
        echo "fetch: saved ${dest} ($(du -h "${dest}" | cut -f1))"
        exit 0
    fi
    echo "fetch: failed: ${url}" >&2
done

echo "fetch: all sources failed for ${dest}" >&2
exit 1
