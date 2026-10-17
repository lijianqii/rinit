#!/usr/bin/env bash
# Coverage gate (plan §6: >=80% of lines overall, >=90% in rinit-core).
#
# `make check` is the commit-time gate and stays fast; this is the acceptance
# one, and it is separate because a workspace run of llvm-cov takes minutes
# rather than seconds. It prints the numbers it measured as well as the verdict,
# so the README's coverage figures have a command behind them instead of a
# hand-copied table.
#
# Usage:
#   scripts/coverage-check.sh [--overall PCT] [--core PCT] [--report PATH]
#                               [--summary PATH]
#
# Exit codes:
#   0  both thresholds held
#   1  a threshold was missed
#   2  bad usage, or llvm-cov could not run
set -euo pipefail

overall_min=80
core_min=90
report=""
summary=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --overall) overall_min="${2:?}"; shift 2 ;;
        --core) core_min="${2:?}"; shift 2 ;;
        --report) report="${2:?}"; shift 2 ;;
        # Score an existing summary instead of producing one. The gate's own
        # arithmetic is worth testing without a four-minute llvm-cov run.
        --summary) summary="${2:?}"; shift 2 ;;
        -h | --help) sed -n '2,/^set -euo pipefail$/p' "$0" | sed '$d'; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if [[ -n "$summary" ]]; then
    report="$summary"
else
    if [[ -z "$report" ]]; then
        mkdir -p build
        report="build/coverage-summary.txt"
    fi
    echo "coverage-check: cargo llvm-cov --offline --workspace --summary-only"
    if ! cargo llvm-cov --offline --workspace --summary-only >"$report"; then
        echo "coverage-check: llvm-cov failed (is the llvm-tools component installed?)" >&2
        exit 2
    fi
fi

# The summary table is one row per file plus TOTAL, with the line counts in
# columns 8 and 9:
#   Filename Regions Missed Cover Functions Missed Executed Lines Missed Cover ...
# Percentages are recomputed from the counts rather than read out of the table,
# so a rounding change in llvm-cov cannot move a threshold.
read -r overall core overall_covered overall_total core_covered core_total < <(
    awk '
        NR == 1 { next }
        {
            lines = $8 + 0
            missed = $9 + 0
            total += lines
            total_missed += missed
            if ($1 ~ /^rinit-core\//) {
                core_lines += lines
                core_missed += missed
            }
        }
        END {
            opct = (total > 0) ? (total - total_missed) * 100 / total : 0
            cpct = (core_lines > 0) ? (core_lines - core_missed) * 100 / core_lines : 0
            printf "%d %d %d %d %d %d\n", opct, cpct, total - total_missed, total, core_lines - core_missed, core_lines
        }
    ' "$report"
)

fail=0
if ((overall >= overall_min)); then
    printf 'coverage-check: overall %d%% (%d of %d lines) >= %d%%\n' \
        "$overall" "$overall_covered" "$overall_total" "$overall_min"
else
    printf 'coverage-check: FAIL overall %d%% (%d of %d lines) < %d%%\n' \
        "$overall" "$overall_covered" "$overall_total" "$overall_min" >&2
    fail=1
fi

if ((core >= core_min)); then
    printf 'coverage-check: rinit-core %d%% (%d of %d lines) >= %d%%\n' \
        "$core" "$core_covered" "$core_total" "$core_min"
else
    printf 'coverage-check: FAIL rinit-core %d%% (%d of %d lines) < %d%%\n' \
        "$core" "$core_covered" "$core_total" "$core_min" >&2
    fail=1
fi

printf 'coverage-check: report at %s\n' "$report"
if ((fail)); then
    echo "coverage-check: FAILED" >&2
    exit 1
fi
echo "coverage-check: OK"
