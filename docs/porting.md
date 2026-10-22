# Porting rinit to another board

rinit targets resource-constrained embedded Linux and is developed against
aarch64 (`qemu-system-aarch64 -machine virt`) and ARMv7
(`qemu-system-arm -machine virt,highmem=off -cpu cortex-a15`). This is what a port involves, in
the order the pieces depend on each other. Everything here is checkable against
the repository: the numbers and paths are the ones the Makefile and the scripts
use, not a wish list.

## 1. What the kernel has to provide

rinit is PID 1 with no initramfs tools of its own beyond the ones the packing
script puts in the image, so the kernel has to hand it a few things:

| the kernel must have | why | where it is checked |
| -------------------- | --- | ------------------- |
| `CONFIG_DEVTMPFS` (+ mount) | `/dev` is devtmpfs, and `/dev/watchdog`, `/dev/kmsg` and `/dev/console` come from it | `crates/rinit-runtime/src/mounts.rs` |
| `CONFIG_PROC_FS`, `CONFIG_SYSFS` | `/proc` and `/sys`; `/proc` is what `rctl status` and reaping read | same |
| a cgroup v2 hierarchy: `CGROUP_SCHED`, `CFS_BANDWIDTH`, `MEMCG`, `CGROUP_PIDS` | the `Cgroup*=` limits (plan §7.3); without one of them the kernel answers `ENOENT` for that attribute and rinit skips the limit with a warning | `make kernel-config-check` |
| a watchdog driver (`WATCHDOG_CORE` + your hardware's) | plan §7.5; the Makefile builds `I6300ESB_WDT` because that is what qemu's `virt` machine can attach | `make kernel-config-check` |
| a NIC driver (`VIRTIO_NET` + `VIRTIO_MMIO` on qemu's `virt`) | `units/network.service` runs `udhcpc` on `eth0`; without a NIC `udhcpc` fails and the unit is left failed, but `multi-user.target` only `Wants=` it | `make kernel-config-check` |
| `CONFIG_WATCHDOG_SYSFS` | only so the *test* can read back the timeout rinit set; rinit itself does not need it | same |
| `CONFIG_TMPFS` | `/run` and `/tmp` | `mounts.rs` |
| signals that the kernel delivers normally (`SIGCHLD`, `SIGTERM`, ...) | rinit catches them with `sigaction` handlers and a self-pipe - no `signalfd`, no configuration | `crates/rinit-runtime/src/signals.rs` |
| `CONFIG_BINFMT_ELF` and whatever the image's busybox needs | the services themselves | - |

`make kernel-config-check` is the authority: it fails the build when a symbol the
guest's acceptance tests depend on is missing, which is the failure mode
`olddefconfig` otherwise hides (it drops an unmet symbol silently).

Boot with `rdinit=/sbin/rinit` (not `init=`): rinit expects to be the kernel's
first user process and mounts the first wave itself (`/run` → `/proc` → `/sys` →
`/dev` → `/tmp` → `/sys/fs/cgroup`). The first four are fatal if they fail;
`/tmp` and the cgroup hierarchy are degradable, so a kernel without cgroup v2
still boots with every unit unlimited.

## 2. Userspace the image has to contain

- `/sbin/rinit` - the binary (static, `panic="abort"`, LTO; see the README's
  Measurements for the size).
- `/bin/rctl` - the control client. Useless without the socket rinit binds at
  `/run/rinit/control.sock`, which is why the two ship together.
- `/etc/rinit/units/*.service|*.target` - the units. At minimum something that
  gives you a console (`getty.service`) or a shell, or a port to an appliance
  has nothing to talk to.
- busybox (or equivalent) for the units themselves: `sh`, `sleep`, `cat`, `grep`,
  `awk`, `cut`, `test` are what the acceptance tests use. rinit itself needs none
  of them - it execs a unit's `ExecStart` directly.
- `/etc/fstab` if the board has filesystems to mount after the first wave
  (plan §1.3(9)). It is optional: no file means no second wave, and `nofstab` on
  the kernel command line means the same. Entries are six fields, `noauto` is
  skipped, UUID/LABEL are *not* resolved, and a failing entry is a warning with
  the boot continuing - rinit creates each mountpoint, as it does for the first
  wave. `--fstab <PATH>` moves the file, which is also the test seam. The device
  image uses this table for `devpts` on `/dev/pts`; keep the default devpts
  instance so the devtmpfs `/dev/ptmx` node continues to work. The image also
  ships `/etc/mdev.conf` because mdev's fallback mode is `0660` root:root; the
  config preserves the standard character-device permissions.
- `/etc/passwd` and `/etc/group` if any unit uses `User=`/`Group=`. rinit parses
  them itself (no NSS, no `libnss_files.so`), so a minimal
  `name:passwd:uid:gid:...` file is enough.
- `/etc/shadow` for `getty`/`login`; the development image ships `root`/`toor`
  (SHA-512 crypt) and mode `0600`. Change it before deployment; dropbear is
  key-only and does not use this password.
- a small FHS skeleton: `/var/log`, `/var/tmp`, `/var/run -> ../run`, `/mnt`,
  `/home`, `/opt`, `/srv`, `/usr/local`, and `/etc/profile`, `/etc/hostname`,
  `/etc/hosts`, `/etc/resolv.conf`, `/etc/motd`. rinit reads none of them; they
  are what the login shell and the tools expect. `/etc/profile` applies
  `/etc/hostname`, because rinit does not call `sethostname(2)`.

The image `make initramfs` builds has this layout (plus busybox's applet tree);
see `scripts/build-initramfs.sh`. It chmods the image root to `0755`, which matters:
a service that drops privileges cannot traverse a root directory it may not
search, and the failure looks like a missing binary.

## 3. Building

```bash
cargo build --offline --release --target aarch64-unknown-linux-gnu
cargo build --offline --release --target armv7-unknown-linux-gnueabihf
```

The release profile is part of the contract, not a preference: `panic="abort"`
(a panic in PID 1 must not unwind into a half-dead system), `overflow-checks` and
`lto`. `make rinit` wraps this and asserts both binaries exist.

The Makefile selects the whole pipeline with `ARCH`:

```bash
make ARCH=arm64 run
make ARCH=arm32 run
```

`arm32` uses the hard-float ARMv7 target, Linux's `multi_v7_defconfig`,
`arch/arm/boot/zImage` and `qemu-system-arm -machine virt,highmem=off -cpu
cortex-a15`. `highmem=off` keeps the PCI ECAM below 4G; without it the 32-bit
kernel's `pci-host-generic` probe fails with `EOVERFLOW` and `i6300esb` never
appears. The Debian armhf toolchain has no `rcrt1.o`, so `.cargo/config.toml`
links ARMv7 as a plain static (non-PIE) binary rather than static-PIE; both forms
have no `ld.so`/`libc.so` dependency.

Nothing in the Rust source is architecture-specific. A new architecture needs
three things changed, and they are all in the scripts:

1. an `ARCH` case in the Makefile that sets `TARGET`, `CROSS_COMPILE`,
   `KERNEL_ARCH`, `KERNEL_DEFCONFIG`, the Image/zImage path and the qemu arch;
2. a case in `scripts/build-initramfs.sh`, which maps the target triple to the
   cross prefix and to busybox's own `ARCH` spelling - without it the busybox
   `install` step silently recompiles with the *host* compiler;
3. a qemu command line in `scripts/run-qemu.sh`: the machine, the console, and
   the watchdog device. `-device i6300esb` is what gives the aarch64 and arm32
   `virt` machines a watchdog to test plan §7.5 against; another board will have
   its own.

The kernel config the Makefile applies (the cgroup controllers, `VIRTIO_NET`,
`VIRTIO_MMIO`, `I6300ESB_WDT` and `WATCHDOG_SYSFS`) is `sed`-ed into the
selected defconfig, then `olddefconfig` resolves dependencies. ARM's
`multi_v7_defconfig` needs a second pass because `CFS_BANDWIDTH` depends on
`FAIR_GROUP_SCHED`, which the first pass has not defaulted yet; the Makefile
does both passes. `kernel-config-check` says at build time rather than at boot
whether a symbol disappeared.

## 4. Running it on the target

```bash
rinit [OPTIONS]
  --units <DIR>          unit directory (default /etc/rinit/units)
  --default <TARGET>     boot target (default multi-user.target)
  --cgroup-root <DIR>    where the cgroup v2 hierarchy is mounted
  --watchdog <DEVICE>    hardware watchdog (default /dev/watchdog)
  --watchdog-timeout <S> hardware timeout; fed every half (default 30)
  --log-file <PATH>      mirror service output to a file
                         (default /var/log/rinit/log; 'none' disables)
  --fstab <PATH>         second-wave mount table
                         (default /etc/fstab; 'none' disables)
```

`--dev` is for developers, not for a target: it skips the mounts, does not
reboot, relaxes the control socket's peer rule, and does not touch the watchdog
or the machine's own cgroup hierarchy. On a real device it should not appear.

The boot target's *closure* is what starts. A unit that nothing references is
loaded and listed by `rctl` but never started - that is where the demo units in
`units/demo/` sit, and `rctl start <unit>` is how they come up. (`units/` itself
is the boot set: the two targets, `getty.service`, `network.service` and
`shell.service`. The deployable-device services live in `units/device/` and are
packed only into the disk image.)

### Persistent disk image

`make ARCH=... run-disk` builds an ext4 root filesystem and boots it directly:

```bash
make ARCH=arm64 run-disk
make ARCH=arm32 run-disk
make disk-image          # destructive rebuild; run-disk reuses an existing image
```

The kernel command line is `root=/dev/vda rw rootfstype=ext4 init=/sbin/rinit
-- --default device.target`; there is no initramfs. The kernel needs
`CONFIG_EXT4_FS`, `CONFIG_VIRTIO_BLK`, `CONFIG_VIRTIO_MMIO`, and
`CONFIG_DEVTMPFS_MOUNT` **off** - rinit mounts `/dev` itself, and a kernel
auto-mount makes that first-wave mount fail with `EBUSY`.

The image is populated with `mke2fs -d` from a tar whose entries are explicitly
`root:root` (a directory populated as the build user would otherwise leave every
file owned by that uid). `units/device/` provides `hostname`, `network-online`
(DHCP lease, not just udhcpc exec), `syslog`/`klogd`, `seedrng`, `time-sync`,
`mdev` (busybox hotplug daemon) and `sshd`; `/etc/fstab` mounts `devpts` on
`/dev/pts` in the second wave, before
any unit starts. dropbear is static and key-only (`-s`), with host keys generated
on first connection under `/etc/dropbear`. `/root/.ssh/authorized_keys` starts
empty by design. `scripts/build-dropbear.sh` applies
`scripts/dropbear-pty.patch` before compiling, so an interactive SSH session
keeps the PTY master open until the slave becomes the controlling terminal. The
qemu user-net host-forward is `2222 -> 22` by default (`DISK_HOSTFWD`), so
provisioning a key is what makes `ssh -p 2222 root@...` work. `/etc`, `/var`,
host keys, logs and the RNG seed persist across boots because the ext4 image is
reused; `make disk-image` is what wipes them.

## 5. Differences from systemd to expect

rinit implements the plan's subset (§2.2). If a systemd unit file does not parse,
that is deliberate: the parser rejects unknown keys so a ported unit fails loudly
instead of silently losing a setting. The common ones:

- `[Install]` and `WantedBy=`: there is no `enable`. Edit the target's
  `Requires=`/`Wants=` instead.
- `WantedBy`-style *reverse* dependencies: rinit resolves forwards. A unit is
  started because a target reads it, not because it names a target.
- Template units work (`worker@.service`, `%i`/`%n`/`%p`), but with two systemd
  differences: there is no `enable`, so nothing starts an instance at boot, and a
  *unit file* cannot name an instance (`Requires=worker@1.service` is rejected
  when the directory is loaded) - instances are started by name. systemd gets the
  first from `[Install]` symlinks and the second from the same.
- `Sockets=`/`NeedsSocket=` work as a *dependency* (plan §7.8): the provider
  declares the path, the consumer waits for it.
- `SocketActivate=`/`IdleTimeoutSec=` work as *activation* (plan §7.9): rinit
  binds the socket, the first connection starts the service, and the service is
  handed the listening socket on fd 3 with `LISTEN_FDS=1`. Two differences from
  systemd: the socket is bound at boot only (a reload that adds an activated unit
  needs a restart), and there is no `Accept=yes` inetd-style mode - rinit always
  passes the listening socket.
- `Type=idle`/`dbus`, `$MAINPID`, cgroup v1: not implemented.
- `PermissionsStartOnly=` and the `+`/`-`/`!` prefixes: every `Exec*` runs as the
  unit's `User=`.
- `Type=forking` works, but the `PIDFile=` has to have been written by the time
  the main process exits (systemd polls for a few seconds; rinit does not), and a
  unit with no `PIDFile=` needs a `Cgroup*=` so the daemon can be found by
  cgroup.
- `systemctl` is `rctl`: `list`, `status`, `start`, `stop`, `restart`,
  `reset-failed`, `log`, `reload`. There is no `daemon-reload` equivalent
  mid-start: `rctl reload` is atomic (the whole directory resolves or nothing
  changes).
- No journal: `rctl log` reads a bounded in-memory ring. `--log-file` mirrors it
  to a file (the plan's `/var/log/rinit/log` by default, truncated at 1 MiB), but
  that is a file rather than a service - shipping, rotation beyond the one
  truncation and per-unit retention are the target's job.
- `rctl reload` is the only live reconfiguration: there is no per-unit
  `daemon-reload` to call while a unit is being edited, and a running unit keeps
  the definition it was started with until its next start (by design).

## 6. Checking a port

```bash
make check         # fmt, clippy, and the host test suite
make parallel      # 20 services in parallel vs chained: the scheduler's speedup
make soak          # restart churn, watching RSS, descriptors and zombies
make test-system   # boots the selected ARCH and asserts plan §6's list
make ARCH=arm32 test-system  # the ARMv7 guest
make ARCH=arm64 test-system  # the default aarch64 guest
make ARCH=arm32 run-disk     # ARMv7 ext4/device boot
make ARCH=arm64 run-disk     # aarch64 ext4/device boot
make gates         # the two numeric acceptance items: size and coverage
```

`make gates` is what keeps plan §6's numbers from being hand-copied: `size-check`
measures the shipped binaries against the §0.1 budget, and `coverage-check`
measures the suite against §6's 80% (overall) and 90% (`rinit-core`) and prints
the counts behind each percentage. Neither is in `make check`, which stays the
fast per-commit gate.

A note on the tools that are *not* in the list, because the plan asks for them and
this environment cannot run them: `miri` (nightly-only; the core is
`#![forbid(unsafe_code)]` and its leak checking is
`crates/rinit-core/tests/allocations.rs`, a counting-allocator balance test),
`valgrind` (the soak harness's sampled RSS/descriptor/zombie curve is the
equivalent) and `cargo-fuzz` (the parser, the control codec and the job queue are
`proptest`-tested). On a machine with a nightly, `cargo +nightly miri test -p
rinit-core` is the plan's §4.4 command.

`make test-system` is the one that matters for a port: it boots the real image on
the real kernel and asserts the acceptance list from plan §6 (see the README's
Acceptance section). For a new board, point `scripts/run-qemu.sh` at it - or run
the same list by hand, since every assertion is a console line or a `rctl` reply
in `scripts/system-test.sh`.
