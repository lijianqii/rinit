#!/usr/bin/env bash
# Exercise soak.sh's steady-state verdict against a large synthetic curve.
#
# The 72-hour acceptance run is 259,200 samples. The verdict used to read that
# window through `echo "$steady" | head -n ...`, which closes the pipe early:
# `echo` died of SIGPIPE, `set -o pipefail` turned that into exit 141, and a
# completed run was reported as a harness failure with no verdict at all. This
# test needs no rinit and no waiting - it feeds the same functions a curve of
# that size and asserts both the numbers and the exit status.
#
# Usage: scripts/soak-verdict-test.sh
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
work="$(mktemp -d "${TMPDIR:-/tmp}/soak-verdict-test.XXXXXX")"
trap 'rm -rf "$work"' EXIT

# Extract the verdict block from soak.sh verbatim, so the test exercises the
# shipped code rather than a copy that can drift from it.
python3 - "$repo_root/scripts/soak.sh" "$work/verdict.sh" <<'PY'
import sys

source = open(sys.argv[1], encoding="utf-8").read()
start = source.index("steady_rows=$(")
end = source.index("fail=0", start)
open(sys.argv[2], "w", encoding="utf-8").write(source[start:end])
PY

# A 72-hour run at one sample a second: the size that broke the old code.
rows=259200
warmup=5
awk -v n="$rows" 'BEGIN {
    for (i = 0; i < n; i++) {
        # Flat memory after the warmup, descriptors that stay put, no zombies.
        rss = 3000
        fds = (i % 1000 == 0) ? 22 : 20
        printf "%d %d 2 0\n", rss, fds
    }
}' >"$work/curve.txt"

run_verdict() {
    local curve=$1 warmup=$2
    bash -c '
        set -euo pipefail
        curve="$1"
        warmup="$2"
        # shellcheck disable=SC1090
        . "$3"
        printf "%s %s %s %s %s\n" \
            "$first_rss" "$last_rss" "$first_fds" "$last_fds" "$max_zombies"
    ' _ "$curve" "$warmup" "$work/verdict.sh"
}

verdict="$(run_verdict "$work/curve.txt" "$warmup")"
read -r first_rss last_rss first_fds last_fds max_zombies <<<"$verdict"

[[ "$first_rss" == "3000" && "$last_rss" == "3000" ]] || {
    echo "verdict test: RSS peaks wrong: got ${first_rss}/${last_rss}" >&2
    exit 1
}
[[ "$first_fds" == "20" && "$last_fds" == "20" ]] || {
    echo "verdict test: fd troughs wrong: got ${first_fds}/${last_fds}" >&2
    exit 1
}
[[ "$max_zombies" == "0" ]] || {
    echo "verdict test: zombie peak wrong: got ${max_zombies}" >&2
    exit 1
}

# The halves must actually be halves: a window that is read only in part would
# still produce plausible peaks. Put one tall sample at the end of each half.
awk -v n="$rows" 'BEGIN {
    for (i = 0; i < n; i++) {
        rss = 3000
        if (i == 129000 || i == 258000) rss = 9000
        printf "%d 20 2 0\n", rss
    }
}' >"$work/curve-spikes.txt"

spikes="$(run_verdict "$work/curve-spikes.txt" "$warmup")"
read -r first_rss last_rss _ _ _ <<<"$spikes"
[[ "$first_rss" == "9000" && "$last_rss" == "9000" ]] || {
    echo "verdict test: a spike in one half was not seen (${first_rss}/${last_rss})" >&2
    exit 1
}

# An odd row count must not double-count the middle sample: a single spike in
# the middle row belongs to the first half only, never to both.
awk -v n="$rows" 'BEGIN {
    for (i = 0; i < n; i++) {
        rss = (i == 129600) ? 9000 : 3000
        printf "%d 20 2 0\n", rss
    }
}' >"$work/curve-middle.txt"

middle="$(run_verdict "$work/curve-middle.txt" "$warmup")"
read -r first_rss last_rss _ _ _ <<<"$middle"
[[ "$first_rss" == "9000" && "$last_rss" == "3000" ]] || {
    echo "verdict test: the middle sample was counted twice (${first_rss}/${last_rss})" >&2
    exit 1
}

# A too-short curve must be refused rather than produce a verdict from nothing.
: >"$work/empty.txt"
if bash -c '
    set -euo pipefail
    curve="$1"
    warmup=5
    . "$2"
' _ "$work/empty.txt" "$work/verdict.sh" 2>/dev/null; then
    echo "verdict test: an empty curve was accepted" >&2
    exit 1
fi

echo 'soak-verdict-test: OK'
