#!/usr/bin/env bash
# Install rinit-tools into a prepared rootfs.
#
# The default profile is `replace-system`: each standard path becomes a real ELF
# and the matching BusyBox symlink is removed. `/bin/busybox` is never touched.
#
# Usage:
#   scripts/install-tools.sh --rootfs DIR --bin-dir DIR [options]
#
#   --rootfs DIR        prepared root filesystem (required)
#   --bin-dir DIR       directory containing the built binaries (required)
#   --manifest FILE     tool manifest (default: tools.list)
#   --profile NAME      replace-system (default) or coexist
#   --arch NAME         aarch64 (default), armv7 or x86_64
#   --allow-missing     skip a tool whose binary has not been built yet
#
# Exit codes: 0 success, 1 install/gate failure, 2 usage error.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
rootfs=""
bin_dir=""
manifest="$repo_root/tools.list"
profile=replace-system
arch=aarch64
allow_missing=0

die() {
    echo "install-tools: $*" >&2
    exit 2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --rootfs) rootfs="${2:?}"; shift 2 ;;
        --bin-dir) bin_dir="${2:?}"; shift 2 ;;
        --manifest) manifest="${2:?}"; shift 2 ;;
        --profile) profile="${2:?}"; shift 2 ;;
        --arch) arch="${2:?}"; shift 2 ;;
        --allow-missing) allow_missing=1; shift ;;
        -h | --help) sed -n '2,/^set -euo pipefail$/p' "$0" | sed '$d'; exit 0 ;;
        *) die "unknown argument: $1" ;;
    esac
done

[[ -n "$rootfs" ]] || die "--rootfs is required"
[[ -n "$bin_dir" ]] || die "--bin-dir is required"
[[ -d "$rootfs" ]] || die "rootfs is not a directory: $rootfs"
[[ -d "$bin_dir" ]] || die "bin dir is not a directory: $bin_dir"
[[ -r "$manifest" ]] || die "manifest is not readable: $manifest"
case "$profile" in
    replace-system | coexist) ;;
    *) die "--profile must be replace-system or coexist" ;;
esac
case "$arch" in
    aarch64) interpreter=/lib/ld-linux-aarch64.so.1 ;;
    armv7) interpreter=/lib/ld-linux-armhf.so.3 ;;
    x86_64) interpreter=/lib64/ld-linux-x86-64.so.2 ;;
    *) die "--arch must be aarch64, armv7 or x86_64" ;;
esac
command -v readelf >/dev/null || die "readelf is not installed"

# Keep this in sync with the runtime libraries copied by build-initramfs.sh.
# libm is deliberately absent: the image does not ship it, so allowing it here
# would pass the build and fail at exec time in the guest.
allowed_needed=(
    libc.so.6
    libgcc_s.so.1
)
check_elf() {
    local bin=$1 install_path=$2 budget_kib=$3
    local size interpreter_found
    size=$(stat -c %s "$bin")
    if [[ "$budget_kib" != "-" ]]; then
        ((size <= budget_kib * 1024)) || {
            echo "install-tools: $install_path is $size bytes, budget ${budget_kib}KiB" >&2
            return 1
        }
    fi
    interpreter_found=$(readelf -l "$bin" 2>/dev/null |
        sed -n 's/.*Requesting program interpreter: \(.*\)]/\1/p' | head -1)
    [[ "$interpreter_found" == "$interpreter" ]] || {
        echo "install-tools: $install_path interpreter '$interpreter_found' != '$interpreter'" >&2
        return 1
    }
    local needed
    while read -r needed; do
        [[ -n "$needed" ]] || continue
        local ok=0
        for allowed in "${allowed_needed[@]}"; do
            [[ "$needed" == "$allowed" ]] && ok=1
        done
        ((ok)) || {
            echo "install-tools: $install_path has unexpected NEEDED '$needed'" >&2
            return 1
        }
    done < <(readelf -d "$bin" 2>/dev/null | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p')
}

installed=0
while read -r applet cargo_bin install_path busybox_link budget_kib _source; do
    [[ -z "${applet:-}" || "$applet" == \#* ]] && continue
    [[ "$applet" == "applet" ]] && continue
    [[ -n "${budget_kib:-}" ]] || die "malformed manifest line for $applet"

    bin="$bin_dir/$cargo_bin"
    if [[ ! -x "$bin" ]]; then
        if ((allow_missing)); then
            echo "install-tools: skip $applet (not built)"
            continue
        fi
        echo "install-tools: missing binary for $applet: $bin" >&2
        exit 1
    fi

    target_path="$install_path"
    if [[ "$profile" == coexist && "$busybox_link" != "-" ]]; then
        target_path="$(dirname "$install_path")/rinit-$applet"
    fi
    target="$rootfs$target_path"

    # Validate the replacement before removing anything from the rootfs.
    check_elf "$bin" "$target_path" "$budget_kib" || exit 1

    if [[ "$profile" == replace-system && "$busybox_link" != "-" ]]; then
        link="$rootfs$busybox_link"
        if [[ -L "$link" ]]; then
            target_of_link=$(readlink "$link")
            [[ "$(basename "$target_of_link")" == busybox ]] || {
                echo "install-tools: $busybox_link points at '$target_of_link', not busybox" >&2
                exit 1
            }
            rm -f "$link"
        elif [[ -e "$link" ]]; then
            # A previous replace-system install (or a deliberate operator
            # replacement) is a regular file; replacing it is idempotent.
            echo "install-tools: replacing existing $busybox_link"
        fi
    fi

    mkdir -p "$(dirname "$target")"
    install -m 0755 "$bin" "$target"
    echo "install-tools: $target_path <- $bin"
    installed=$((installed + 1))
done <"$manifest"

if ((installed == 0)) && ((allow_missing == 0)); then
    die "manifest installed nothing"
fi
echo "install-tools: profile=$profile arch=$arch installed=$installed"
