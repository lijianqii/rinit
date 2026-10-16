# Porting rinit to another board

rinit targets resource-constrained embedded Linux and is developed against
aarch64 (`qemu-system-aarch64 -machine virt`). This is what a port involves, in
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
- `/etc/passwd` and `/etc/group` if any unit uses `User=`/`Group=`. rinit parses
  them itself (no NSS, no `libnss_files.so`), so a minimal
  `name:passwd:uid:gid:...` file is enough.

The image `make initramfs` builds has exactly this layout; see
`scripts/build-initramfs.sh`. It chmods the image root to `0755`, which matters:
a service that drops privileges cannot traverse a root directory it may not
search, and the failure looks like a missing binary.

## 3. Building

```bash
cargo build --offline --release --target aarch64-unknown-linux-gnu
```

The release profile is part of the contract, not a preference: `panic="abort"`
(a panic in PID 1 must not unwind into a half-dead system), `overflow-checks` and
`lto`. `make rinit` wraps this and asserts the three binaries exist.

Nothing in the Rust source is architecture-specific. A new architecture needs
three things changed, and they are all in the scripts:

1. `TARGET` and `CROSS_COMPILE` (`make TARGET=riscv64gc-unknown-linux-gnu
   CROSS_COMPILE=riscv64-linux-gnu- ...`);
2. a case in `scripts/build-initramfs.sh`, which maps the target triple to the
   cross prefix and to busybox's own `ARCH` spelling - without it the busybox
   `install` step silently recompiles with the *host* compiler;
3. a qemu command line in `scripts/run-qemu.sh`: the machine, the console, and
   the watchdog device. `-device i6300esb` is what gives the aarch64 `virt`
   machine a watchdog to test plan §7.5 against; another board will have its own.

The kernel config the Makefile applies (`CONFIG_CFS_BANDWIDTH`,
`CONFIG_I6300ESB_WDT`, `CONFIG_WATCHDOG_SYSFS`) is `sed`-ed into a `defconfig`,
so it needs each symbol to exist for that architecture - `kernel-config-check`
says so at build time rather than at boot.

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
```

`--dev` is for developers, not for a target: it skips the mounts, does not
reboot, relaxes the control socket's peer rule, and does not touch the watchdog
or the machine's own cgroup hierarchy. On a real device it should not appear.

The boot target's *closure* is what starts. A unit that nothing references is
loaded and listed by `rctl` but never started - that is where the demo units in
`units/` sit, and `rctl start <unit>` is how they come up.

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
- `Sockets=` and socket activation, `Type=idle`/`dbus`, `$MAINPID`, cgroup v1:
  not implemented (plan §7.8-§7.9, P3).
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
make test-system   # boots the guest and asserts plan §6's list
```

`make test-system` is the one that matters for a port: it boots the real image on
the real kernel and asserts the acceptance list from plan §6 (see the README's
Acceptance section). For a new board, point `scripts/run-qemu.sh` at it - or run
the same list by hand, since every assertion is a console line or a `rctl` reply
in `scripts/system-test.sh`.
