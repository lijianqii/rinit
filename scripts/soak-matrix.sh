#!/usr/bin/env bash
# Run the aarch64 and armv7 soaks concurrently and analyze both.
#
# Both instances are stopped if either fails: a long-run verdict must not mix a
# clean architecture with a failed one. Feature probes are enabled by default;
# pass --no-feature for a core-only smoke.
#
# Usage:
#   scripts/soak-matrix.sh --key PATH [--seconds N] [--tag-prefix NAME]
#       [--aarch64-image FILE] [--arm-image FILE]
#       [--aarch64-kernel FILE] [--arm-kernel FILE]
#       [--port-aarch64 N] [--port-arm N]
#       [--helper-dir DIR] [--feature-suite FILE] [--no-feature]
set -u

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
seconds=86400
prefix=matrix
key=""
port_aarch64=2222
port_arm=2223
helper_dir="$repo_root/build"
feature_suite="$repo_root/tests/soak/feature-suite.sh"
feature=1
aarch64_image="$repo_root/build/out/rinit-aarch64-unknown-linux-gnu.ext4"
arm_image="$repo_root/build/arm32/out/rinit-armv7-unknown-linux-gnueabihf.ext4"
aarch64_kernel="$repo_root/build/src/linux-6.6.158/arch/arm64/boot/Image"
arm_kernel="$repo_root/build/arm32/src/linux-6.6.158/arch/arm/boot/zImage"
log_dir="$repo_root/build/soak-charts"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --seconds) seconds="${2:?}"; shift 2 ;;
        --tag-prefix) prefix="${2:?}"; shift 2 ;;
        --key) key="${2:?}"; shift 2 ;;
        --port-aarch64) port_aarch64="${2:?}"; shift 2 ;;
        --port-arm) port_arm="${2:?}"; shift 2 ;;
        --helper-dir) helper_dir="${2:?}"; shift 2 ;;
        --feature-suite) feature_suite="${2:?}"; shift 2 ;;
        --aarch64-image) aarch64_image="${2:?}"; shift 2 ;;
        --arm-image) arm_image="${2:?}"; shift 2 ;;
        --aarch64-kernel) aarch64_kernel="${2:?}"; shift 2 ;;
        --arm-kernel) arm_kernel="${2:?}"; shift 2 ;;
        --no-feature) feature=0; shift ;;
        -h | --help) sed -n '2,/^set -u$/p' "$0" | sed '$d'; exit 0 ;;
        *) echo "soak-matrix: unknown argument: $1" >&2; exit 2 ;;
    esac
done

[[ -n "$key" ]] || { echo "soak-matrix: --key is required" >&2; exit 2; }
for f in "$key" "$aarch64_image" "$arm_image" "$aarch64_kernel" "$arm_kernel"; do
    [[ -e "$f" ]] || { echo "soak-matrix: missing $f" >&2; exit 2; }
done
if ((feature)); then
    [[ -x "$helper_dir/soak-helper-aarch64" ]] ||
        { echo "soak-matrix: missing $helper_dir/soak-helper-aarch64" >&2; exit 2; }
    [[ -x "$helper_dir/soak-helper-arm" ]] ||
        { echo "soak-matrix: missing $helper_dir/soak-helper-arm" >&2; exit 2; }
    [[ -e "$feature_suite" ]] ||
        { echo "soak-matrix: missing $feature_suite" >&2; exit 2; }
fi
mkdir -p "$log_dir"

tag_aarch64="${prefix}-arm64"
tag_arm="${prefix}-arm32"
log_aarch64="$log_dir/$tag_aarch64-console.log"
log_arm="$log_dir/$tag_arm-console.log"
rm -f "$log_aarch64" "$log_arm"

child_aarch64=""
child_arm=""
cleanup() {
    local status=$?
    trap - EXIT
    [[ -n "$child_aarch64" ]] && kill "$child_aarch64" 2>/dev/null || true
    [[ -n "$child_arm" ]] && kill "$child_arm" 2>/dev/null || true
    wait "$child_aarch64" 2>/dev/null || true
    wait "$child_arm" 2>/dev/null || true
    exit "$status"
}
trap cleanup EXIT

start_arch() {
    local arch=$1 tag=$2 port=$3 image=$4 kernel=$5 log=$6 helper=$7
    local args=(
        --arch "$arch" --port "$port" --key "$key" --seconds "$seconds"
        --tag "$tag" --disk "$image" --kernel "$kernel"
        --checkpoint-interval 3600
    )
    if ((feature)); then
        args+=(--helper "$helper" --feature-suite "$feature_suite")
    fi
    "$repo_root/scripts/soak-qemu.sh" "${args[@]}" >"$log" 2>&1 &
    started_pid=$!
}

start_arch aarch64 "$tag_aarch64" "$port_aarch64" \
    "$aarch64_image" "$aarch64_kernel" "$log_aarch64" \
    "$helper_dir/soak-helper-aarch64"
child_aarch64=$started_pid
start_arch arm "$tag_arm" "$port_arm" \
    "$arm_image" "$arm_kernel" "$log_arm" \
    "$helper_dir/soak-helper-arm"
child_arm=$started_pid

echo "soak-matrix: aarch64=$tag_aarch64 pid=$child_aarch64 port=$port_aarch64"
echo "soak-matrix: arm32=$tag_arm pid=$child_arm port=$port_arm"
echo "soak-matrix: seconds=$seconds feature=$feature"

failed=0
wait "$child_aarch64" || failed=1
wait "$child_arm" || failed=1

if ((failed)); then
    echo "soak-matrix: FAIL (one architecture failed; both instances stopped)" >&2
    exit 1
fi

analysis=0
"$repo_root/scripts/soak-analyze.py" --dir "$log_dir" --tag "$tag_aarch64" \
    ${feature:+--require-feature} --json "$log_dir/$tag_aarch64-summary.json" || analysis=1
"$repo_root/scripts/soak-analyze.py" --dir "$log_dir" --tag "$tag_arm" \
    ${feature:+--require-feature} --json "$log_dir/$tag_arm-summary.json" || analysis=1

if ((analysis)); then
    echo "soak-matrix: FAIL (analysis rejected one or both results)" >&2
    exit 1
fi
echo "soak-matrix: OK tag-prefix=$prefix seconds=$seconds"
