#!/usr/bin/env bash
# Exercise the resource gates without waiting for a long-running soak.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
work="$(mktemp -d "${TMPDIR:-/tmp}/soak-analyze-test.XXXXXX")"
trap 'rm -rf "$work"' EXIT

tag=fixture
prefix="$work/soak-qemu-$tag"

cat >"$prefix-run.txt" <<'EOF'
commit fixture
arch aarch64
duration 86400s interval 10s checkpoint 3600s
EOF

cat >"$prefix.log" <<'EOF'
Booting Linux on physical CPU 0x0
SOAK_DONE status=OK samples=8640 control_ok=17280 control_fail=0 quiescent=0 health_fail=0 sample_fail=0
EOF

make_csv() {
    local path=$1
    local growth_kb_per_hour=${2:-4}
    {
        printf 'elapsed,rss_kb,fds,threads,zombies,control_ok,control_fail,ip,route,rnetd,dropbear,network,sshd,device_target,disk_free_kb,mem_available_kb,swap_used_kb,load1,feature_active\n'
        awk -v growth="$growth_kb_per_hour" 'BEGIN {
            for (i = 0; i < 8640; i++) {
                elapsed = i * 10
                rss = elapsed < 7200 ? 2940 : 2940 + growth * int((elapsed - 7200) / 3600)
                fds = (i % 1000 == 0) ? 22 : 20
                printf "%d,%d,%d,2,0,2,0,10.0.2.15/24,10.0.2.2,1,1,active,active,active,20000,60000,0,0.1,0\n", elapsed, rss, fds
            }
        }'
    } >"$path"
}

expect_ok() {
    make_csv "$prefix-guest.csv"
    python3 "$repo_root/scripts/soak-analyze.py" --dir "$work" --tag "$tag" >/dev/null
}

expect_fail() {
    local reason=$1
    make_csv "$prefix-guest.csv"
    awk -F, -v OFS=, 'NR > 1 { $3 = 0 } 1' "$prefix-guest.csv" >"$prefix-guest.tmp"
    mv "$prefix-guest.tmp" "$prefix-guest.csv"
    if python3 "$repo_root/scripts/soak-analyze.py" --dir "$work" --tag "$tag" \
        >"$work/result.json"; then
        echo "expected analyzer failure: $reason" >&2
        exit 1
    fi
    grep -q "$reason" "$work/result.json"
}

expect_slope_failure() {
    make_csv "$prefix-guest.csv" 8
    if python3 "$repo_root/scripts/soak-analyze.py" --dir "$work" --tag "$tag" \
        >"$work/result.json"; then
        echo "expected analyzer slope failure" >&2
        exit 1
    fi
    grep -q 'RSS slope' "$work/result.json"
}

expect_ok
expect_fail "samples have no positive FD count"
expect_slope_failure

echo 'soak-analyze-test: OK'
