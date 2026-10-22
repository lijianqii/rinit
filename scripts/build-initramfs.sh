#!/usr/bin/env bash
# Assemble a gzipped cpio initramfs, or populate a rootfs directory, for rinit
# (plan §4.3).
#
# Usage:
#   scripts/build-initramfs.sh <target-triple> [--busybox-src DIR] [--out FILE]
#                                                    [--demo-units] [--device-units]
#                                                    [--dropbear-bin DIR]
#                                                    [--rootfs DIR]
#
# Options:
#   --busybox-src DIR  install the given *built* busybox source tree
#                      (runs `make CONFIG_PREFIX=<root> install`) so a full
#                      applet set (sh/getty/login/mount/ps/...) is available.
#   --out FILE         output path (default: build/out/initramfs-<triple>.cpio.gz)
#   --demo-units       also pack units/demo/, the §7.x acceptance units. Without
#                      it the image carries the boot set only, which is what
#                      `make run` wants; `make test-system` passes it.
#   --device-units     also pack units/device/, the deployable-device services.
#   --dropbear-bin DIR install dropbear/dropbearkey from DIR.
#   --rootfs DIR       populate DIR as a root filesystem and stop (no cpio).
#                      Used by `make run-disk` before mke2fs.
#
# Examples:
#   scripts/build-initramfs.sh aarch64-unknown-linux-gnu --busybox-src build/src/busybox-1.36.1
#   scripts/build-initramfs.sh x86_64-unknown-linux-musl         # rinit only
#   scripts/build-initramfs.sh aarch64-unknown-linux-gnu --demo-units --out x-demo.cpio.gz
#
# The rinit binaries must be built first:
#   cargo build --release --target <triple>
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${here}/.." && pwd)"

triple=""
busybox_src=""
output=""
demo_units=""
device_units=""
dropbear_bin=""
rootfs_out=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --busybox-src) busybox_src="${2:?}"; shift 2 ;;
        --out) output="${2:?}"; shift 2 ;;
        --demo-units) demo_units=1; shift ;;
        --device-units) device_units=1; shift ;;
        --dropbear-bin) dropbear_bin="${2:?}"; shift 2 ;;
        --rootfs) rootfs_out="${2:?}"; shift 2 ;;
        -h | --help) sed -n '2,29p' "$0"; exit 0 ;;
        -*) echo "unknown option: $1" >&2; exit 2 ;;
        *) triple="$1"; shift ;;
    esac
done

: "${triple:?usage: build-initramfs.sh <target-triple> [--busybox-src DIR] [--out FILE] [--demo-units] [--device-units] [--dropbear-bin DIR] [--rootfs DIR]}"
[[ -n "${output}" ]] || output="${repo_root}/build/out/initramfs-${triple}.cpio.gz"

bin_dir="${repo_root}/target/${triple}/release"
if [[ ! -x "${bin_dir}/rinit" ]]; then
    echo "error: ${bin_dir}/rinit not found; run 'cargo build --release --target ${triple}'" >&2
    exit 1
fi
if [[ ! -x "${bin_dir}/rctl" ]]; then
    echo "error: ${bin_dir}/rctl not found; run 'cargo build --release --target ${triple}'" >&2
    exit 1
fi

if [[ -n "${rootfs_out}" ]]; then
    # Absolute path: busybox's `make install` runs with -C its source tree, so
    # a relative CONFIG_PREFIX would resolve against that tree, not the repo.
    work="$(realpath -m "${rootfs_out}")"
    rm -rf "${work}"
    mkdir -p "${work}"
else
    work="$(mktemp -d)"
    trap 'rm -rf "${work}"' EXIT
fi

# The work directory *is* the image's root directory, and `mktemp -d` creates it
# as `0700`. That is invisible while everything in the guest runs as root - root
# has `CAP_DAC_OVERRIDE` and never notices - but it is the whole image's
# traversal bit: a service that drops privileges (plan §7.4) cannot so much as
# `execve("/bin/sh")` through a root directory it may not search, and fails with
# `EACCES` that looks like a missing binary. Everything else in the layout is
# already world-readable; this is the one entry that is not.
chmod 0755 "${work}"

# Directory layout (plan §4.3). No /init symlink: we boot with rdinit=.
#
# The first group is what rinit mounts or execs from. The second is the FHS
# skeleton a login expects to find: `/var/log` for the log mirror and wtmp,
# `/mnt`, `/home`, `/opt`, `/srv` and `/usr/local` for an operator, and `/var/run`
# pointing at the `/run` tmpfs rinit mounts over the image's own directory.
install -d "${work}"/{sbin,bin,etc/rinit/units,etc/udhcpc,proc,sys,dev,run,usr/local/bin,usr/local/sbin}
install -d "${work}"/{var/log/rinit,var/spool,var/lib/seedrng,mnt,media,home,opt,srv}
install -d -m 0700 "${work}/root"
install -d -m 1777 "${work}/tmp" "${work}/var/tmp"
ln -s ../run "${work}/var/run"
: >"${work}/var/log/wtmp"
chmod 0644 "${work}/var/log/wtmp"

install -m 0755 "${bin_dir}/rinit" "${work}/sbin/rinit"
# The control client goes where a user can type it. It is useless without the
# socket rinit creates at boot (plan §1.3(10)), which is why the two ship
# together.
install -m 0755 "${bin_dir}/rctl" "${work}/bin/rctl"

# Plan §1.3(9)'s second wave: the mount table rinit reads after the first wave.
# It ships in the image because the second wave runs at *boot* - a table written
# at run time would be too late - and it is the repo's `fstab` verbatim so the
# guest's evidence points at a file instead of at the packing script.
install -m 0644 "${repo_root}/fstab" "${work}/etc/fstab"

# The rest of the skeleton. rinit reads none of these: they belong to the login
# shell and the tools an operator runs after it. Keeping them in the image means
# a `make run` guest behaves like a small root filesystem rather than just enough
# to start getty - PATH is defined, localhost resolves, the hostname is applied
# once a session starts, and the motd says where the units are.
write_config() {
    local dest="$1"
    cat >"${dest}"
    chmod 0644 "${dest}"
}
write_script() {
    local dest="$1"
    cat >"${dest}"
    chmod 0755 "${dest}"
}
write_config "${work}/etc/profile" <<'EOF'
# Login shell setup for the rinit development image.
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
umask 022

# rinit does not call sethostname(); apply the standard file once a session
# starts, so the next getty prompt and every command see the name too.
[ -s /etc/hostname ] && hostname -F /etc/hostname 2>/dev/null
EOF
printf 'rinit\n' >"${work}/etc/hostname"
chmod 0644 "${work}/etc/hostname"
write_config "${work}/etc/hosts" <<'EOF'
127.0.0.1	localhost
::1		localhost
127.0.1.1	rinit
EOF
write_config "${work}/etc/resolv.conf" <<'EOF'
# The development image configures no network. Add `nameserver` lines here if
# the guest is given an interface and a DHCP client.
EOF
write_config "${work}/etc/motd" <<'EOF'
rinit development guest

Units live in /etc/rinit/units; this image carries the boot set only.
  rctl list                  known units
  rctl status getty.service  one unit in detail
  rctl log getty.service     its buffered output
  poweroff -f                stop the guest

The acceptance image (make test-system) adds units/demo/.
EOF

# The DHCP client is a real unit (`units/network.service`) whose `ExecStart` is
# `udhcpc` directly. This is the client's *lease event* script (`-s`), not a
# startup wrapper: it applies the address, route and resolver when a lease
# arrives and clears the address when it is deconfigured.
write_script "${work}/etc/udhcpc/default.script" <<'EOF'
#!/bin/sh
# udhcpc event script for the development image. busybox exports the prefix
# length as $mask (and the dotted netmask as $subnet); $router and $dns are
# space-separated lists. Absolute paths: rinit starts this without a login shell.
case "$1" in
deconfig)
    /sbin/ip addr flush dev "$interface"
    /sbin/ip link set dev "$interface" up
    ;;
bound|renew)
    /sbin/ip addr flush dev "$interface"
    if [ -n "${broadcast:-}" ]; then
        /sbin/ip addr add "$ip/${mask:-24}" broadcast "$broadcast" dev "$interface"
    else
        /sbin/ip addr add "$ip/${mask:-24}" dev "$interface"
    fi
    /sbin/ip link set dev "$interface" up

    if [ -n "${router:-}" ]; then
        while /sbin/ip route del default dev "$interface" 2>/dev/null; do :; done
        for r in ${router}; do
            /sbin/ip route add default via "$r" dev "$interface" && break
        done
    fi

    {
        [ -n "${domain:-}" ] && echo "search $domain"
        for d in ${dns:-}; do echo "nameserver $d"; done
    } > /etc/resolv.conf
    ;;
esac
exit 0
EOF

# The unit directory, which is what the guest actually boots.
#
# `units/` is the boot set - the console and the two targets - and is all the
# image `make run` needs. The §7.x acceptance units live in `units/demo/` and go
# in only when `--demo-units` is passed, which is what `make test-system` does:
# each exists to be driven from the console one at a time, so a run image that
# carried them would be packing fourteen units nothing boots.
#
# Both directories are flattened into one `/etc/rinit/units`, so a name that
# appears in both would silently shadow - the second `install` wins and the image
# boots a different file than the one the host tests read. That is a build error,
# not a warning.
unit_dirs=("${repo_root}/units")
if [[ -n "${demo_units}" ]]; then
    unit_dirs+=("${repo_root}/units/demo")
fi
if [[ -n "${device_units}" ]]; then
    unit_dirs+=("${repo_root}/units/device")
fi
declare -A unit_from=()
for dir in "${unit_dirs[@]}"; do
    for glob in "${dir}"/*.service "${dir}"/*.target; do
        [[ -e "${glob}" ]] || continue
        name="$(basename "${glob}")"
        if [[ -n "${unit_from[${name}]:-}" ]]; then
            echo "error: ${name} is in both ${unit_from[${name}]} and ${dir}," \
                "and the image flattens them into one directory" >&2
            exit 1
        fi
        unit_from["${name}"]="${dir}"
        install -m 0644 "${glob}" "${work}/etc/rinit/units/"
    done
done

# Optional busybox: install the full built tree (creates applet symlinks).
if [[ -n "${busybox_src}" ]]; then
    if [[ ! -x "${busybox_src}/busybox" ]]; then
        echo "error: ${busybox_src}/busybox not built" >&2
        exit 1
    fi
    # busybox's Makefile only reads the cross prefix from the command line or
    # CONFIG_CROSS_COMPILER_PREFIX in .config (defconfig leaves it empty).
    # Without passing it here the install step silently recompiles busybox with
    # the HOST compiler and overwrites the cross-built binary.
    cross=""
    bbox_arch=""
    case "${triple}" in
        aarch64-*) cross="aarch64-linux-gnu-"; bbox_arch="arm64" ;;
        armv7-*)   cross="arm-linux-gnueabihf-"; bbox_arch="arm" ;;
    esac
    if [[ -n "${cross}" ]]; then
        make -C "${busybox_src}" CONFIG_PREFIX="${work}" CROSS_COMPILE="${cross}" ARCH="${bbox_arch}" install >/dev/null
    else
        make -C "${busybox_src}" CONFIG_PREFIX="${work}" install >/dev/null
    fi
    # An *empty* password field, not `x`.
    #
    # `x` means "the password lives in /etc/shadow", which this initramfs does
    # not have - busybox login then refuses every password, including the empty
    # one, and the console sits at a prompt nobody can get past. Leaving the
    # field empty means "no password required", which is what makes the console
    # usable with `make run`.
    #
    # This is safe only because the serial port is the sole access path and the
    # image is a development initramfs; anyone who can reach it already has the
    # machine. See units/shell.service.
    #
    # `nobody`/`nogroup` are here for the §7.4 acceptance test: a unit that sets
    # `User=`/`Group=` has to be seen dropping to a user that is *not* root, and
    # 65534 is the conventional id for that user. It is a name rather than a
    # number so the test exercises the `/etc/passwd`/`/etc/group` lookup too.
    # `x` means the password is in /etc/shadow. The default is deliberate:
    # root/toor is a development credential, not a production one - change it
    # (or switch to key-only access) before deploying. The hash is a fixed
    # SHA-512 crypt of "toor" with salt "rinit"; busybox's login supports it.
    printf 'root:x:0:0:root:/root:/bin/sh\nnobody:x:65534:65534:nobody:/nonexistent:/sbin/nologin\n' >"${work}/etc/passwd"
    printf 'root:$6$rinit$h.sfVK5a75HqcCQ8mjFVlpyRSK5rin/8GpC7okbfq82BH16wN0lJDJUvCd.0bpt2/q/MtfkobUfMxtG72uIuj.:0:0:99999:7:::\nnobody:!:65534:0:99999:7:::\n' >"${work}/etc/shadow"
    chmod 0600 "${work}/etc/shadow"
    printf 'root:x:0:\nnogroup:x:65534:\n' >"${work}/etc/group"
fi

# Dropbear is only installed for the disk/device image. It generates its host
# keys into /etc/dropbear on first start (`-R`), so that directory has to be
# persistent; the rootfs image provides that. `authorized_keys` starts empty:
# provisioning a client key is an operator decision, and an empty file is safer
# than a default credential.
if [[ -n "${dropbear_bin}" ]]; then
    install -d -m 0755 "${work}/usr/sbin" "${work}/usr/bin"
    install -d -m 0700 "${work}/etc/dropbear" "${work}/root/.ssh"
    install -m 0755 "${dropbear_bin}/dropbear" "${work}/usr/sbin/dropbear"
    install -m 0755 "${dropbear_bin}/dropbearkey" "${work}/usr/bin/dropbearkey"
    : >"${work}/root/.ssh/authorized_keys"
    chmod 0600 "${work}/root/.ssh/authorized_keys"
fi

if [[ -n "${rootfs_out}" ]]; then
    echo "rootfs: ${work}"
else
    "${here}/mkinitramfs.sh" "${work}" "${output}"
fi
