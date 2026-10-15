# rinit

A lightweight, systemd-like PID 1 init and service manager written in Rust,
targeted at resource-constrained embedded Linux (aarch64). See
[`rinit-开发计划.md`](rinit-开发计划.md) for the full development plan; this
README tracks implementation status.

## Status: M0 complete (scaffolding + minimal PID 1) — verified on aarch64

M0 delivers a working PID 1 skeleton, verified booting in qemu:

- workspace layout: `rinit-core` (no_std + alloc), `rinit-runtime` (libc/tokio),
  `rinit` (PID 1 binary), `rctl` (CLI stub).
- single-threaded tokio runtime (`current_thread`), no resident background
  threads, no `spawn_blocking` around `fork`.
- first-wave mounts (`/run` → `/proc` → `/sys` → `/dev` → `/tmp`), fatal vs
  degradable handling per plan §1.3(9).
- child reaping via `SIGCHLD` + `waitpid(-1, WNOHANG)`.
- hardcoded service launch via `posix_spawn` (no config parsing yet).
- panic hook that routes to `/dev/kmsg` for serial visibility.

Verified on the **target architecture** (aarch64 qemu, 128M, Linux 6.6.158) via
`make run SKIP_BUILD=1 SERVICE="/sbin/rinit-testservice --ticks 6"`:

```
[    2.076903] Freeing unused kernel memory: 2624K
[    2.078772] Run /sbin/rinit as init process
rinit: rinit 0.1.0 starting (pid 1)
rinit: mounted /run
rinit: mounted /proc
rinit: mounted /sys
rinit: mounted /dev
rinit: mounted /tmp
rinit: launched test service '/sbin/rinit-testservice --ticks 6' as pid 48
rinit-testservice: main started (pid 48)
rinit-testservice: heartbeat (pid 49)
...
rinit-testservice: exiting after 6 ticks
rinit: reaped child pid 48 (exit 0)
```

That covers the whole M0 acceptance list: boots as PID 1 via `rdinit=`, mounts
the first wave, `fork`/`exec`s a child, and reaps it on `SIGCHLD`. No
`Kernel panic` / `BUG:` in the boot log, and rinit keeps running as PID 1 after
the service is gone. Full logs: `build/logs/qemu-aarch64-m0*.log`.

The same run on the x86_64 host kernel (fast iteration substitute) produces an
equivalent transcript.

## Layout

```
crates/
  rinit-core/       # platform-independent engine (no_std + alloc)
    src/unit/       # M0: UnitName/UnitType; M1: parser.rs, dependency.rs, config.rs
  rinit-runtime/    # syscall layer + mounts + signals + reaping (std/libc/tokio)
  rinit/            # PID 1 binary
    src/boot.rs     # boot sequence + main loop (orchestration only)
    src/service.rs  # service supervision: spawn, track, reap, stop
    src/bin/        # rinit-testservice helper
  rctl/             # CLI stub (real commands land in M3)
units/              # unit files; shell.service gives `make run` a root shell
scripts/            # initramfs / qemu / kernel / system-test helpers
```

## Building

This environment builds fully offline from the local cargo cache:

```bash
cargo build --offline                 # host (debug)
cargo test  --offline                 # unit + integration + smoke tests
cargo clippy --offline --all-targets

# embedded target (static, default per plan §2 risk mitigation)
cargo build --offline --release --target aarch64-unknown-linux-gnu
```

## Testing

Three layers, cheapest first:

```bash
make check          # fmt + clippy -D warnings + host tests  (no cross toolchain)
make test-system    # boot aarch64 qemu and assert the M0 acceptance list
make test           # both
```

`cargo test --offline` alone runs 92 tests: unit tests next to the code
(crate `#[cfg(test)]` modules, including `proptest` properties), integration
tests in `crates/*/tests/`, and an end-to-end smoke test that runs the real
binary in `--dev` mode.

`scripts/system-test.sh` is the automated form of the manual verification in
plan §4.3: it boots the guest, types at the console to prove a root shell is
reachable without a login, captures the serial console, and asserts the whole
M0/M1 acceptance list - including that startup levels run in dependency order
and that no target is ever exec'd. Exit codes are `0` pass, `1` assertion
failed, `2` missing artifacts, `3` qemu could not start. Logs land in
`build/logs/`.

Measured with `llvm-cov` (union across all test binaries):

| area                                   | region coverage |
| -------------------------------------- | --------------- |
| `rinit-core` unit naming + errors      | 100%            |
| `rinit/src/args.rs` (CLI parsing)      | 100%            |
| `rinit/src/service.rs` (supervision)   | ~95%            |
| `rinit-runtime/src/error.rs`           | 99%             |
| `rinit-runtime/src/mounts.rs`          | ~90%            |
| `rinit-runtime/src/signals.rs`         | ~85%            |
| `rinit-runtime` syscall wrappers       | 94–98%          |
| `rinit/src/boot.rs` (orchestration)    | ~65%            |

The mount policy (plan §1.3(9)) is covered through the `MountOps` trait, so
the fatal-vs-degradable rules are tested without `CAP_SYS_ADMIN` - this is the
"fake runtime" shape plan §4.2 asks for.

**Known gaps**, tracked rather than hidden:

- `boot.rs`'s reboot/halt paths are only exercised in the guest; on the host
  they must not run (they would reboot the machine).
- `signals.rs`'s pipe-full path (handler drops a byte) is not forced in a test.
- `rctl` has no tests yet (its exit-code contract is trivial to pin down).

Reproduce the coverage numbers with:

```bash
RUSTFLAGS="-C instrument-coverage" CARGO_TARGET_DIR=target-cov \
    cargo test --offline --workspace
llvm-profdata merge -sparse $(find . -name '*.profraw') -o target-cov/merged.profdata
llvm-cov report -instr-profile=target-cov/merged.profdata \
    --ignore-filename-regex='/\.cargo/|/rustc/' \
    -object target-cov/debug/deps/<each-test-binary>
```

## Running: `make run` (aarch64, full pipeline)

The one-shot target downloads, builds and boots everything for aarch64
(Linux **6.6.158** + busybox stable) from the Tsinghua mirror:

```bash
make run                 # default; builds rinit, kernel, busybox, initramfs, boots qemu
```

> Requires network access (for the source tarballs) and the aarch64 cross
> toolchain: `gcc-aarch64-linux-gnu qemu-system-arm bc flex bison libssl-dev`.
> Downloads are cached under `build/dl/`.

What it does, in order (see [Makefile](Makefile)):

1. `cargo build --release --target aarch64-unknown-linux-gnu` (static rinit).
2. Fetch `linux-6.6.158.tar.xz` from
   `mirrors.tuna.tsinghua.edu.cn/kernel/v6.x/` and build `arch/arm64/boot/Image`.
3. Fetch `busybox-1.36.1.tar.bz2` and build a **static** busybox with the
   applets the initramfs needs.
4. Assemble `build/out/initramfs-<target>.cpio.gz` (busybox + rinit + units).
5. Boot it in `qemu-system-aarch64 -machine virt -cpu cortex-a57 -m 128M`.

Useful knobs:

```bash
make run SERVICE=/bin/sh        # drop to an interactive busybox shell instead
make run SKIP_BUILD=1           # boot existing artifacts; never download/compile
make kernel                     # kernel only
make busybox                    # busybox only
make initramfs                  # rinit + initramfs only
make qemu                       # boot existing artifacts
make MIRROR=https://... run     # different mirror
make KERNEL_VERSION=6.6.158 BUSYBOX_VERSION=1.36.1 ...   # pin versions
```

### Getting a shell in the guest

`make run` boots into an interactive root shell on the serial console - there
is no login prompt and no password, so you can type as soon as the banner
appears:

```
rinit: boot complete: 1 unit(s) started from 'multi-user.target' (1 still running)
/bin/sh: can't access tty; job control turned off
~ # uname -m
aarch64
~ #
```

This is `units/shell.service`, required by `multi-user.target`. It exists for
bring-up and inspection, and is deliberately not a security boundary: anyone
who can reach the serial port already has the machine. `/etc/passwd` has an
empty password field rather than `x`, so `login` is bypassed entirely.

`units/getty.service` is the login-prompt alternative. Both want `ttyAMA0` and
only one can hold the console, so they are alternatives rather than
dependencies: drop the `Requires=shell.service` line from `multi-user.target`
to start getty instead. (They are deliberately *not* a `Conflicts=` pair - M1
parses conflicts but does not stop running units, which needs M2.)

`poweroff -f` from the shell halts the guest; `exit` returns to the boot loop.

Exiting qemu: `SIGTERM` to PID 1 is not reachable from the qemu monitor, so use
`Ctrl-A X` (qemu) to quit.

### Skipping downloads and builds

`make run` is **incremental by default**: every step is a file target, so a step
whose output already exists (and is newer than its inputs) is skipped. Re-running
`make run` after a successful build does no downloading and no compiling — it
just boots. In particular:

- a cached tarball in `build/dl/` is reused (no re-download);
- `linux-6.6.158/arch/arm64/boot/Image` present → kernel is not rebuilt;
- `busybox-1.36.1/busybox` present → busybox is not rebuilt;
- `target/aarch64-unknown-linux-gnu/release/rinit{,testservice}` present and
  newer than the sources → `cargo build` is not re-run;
- the initramfs is repacked only when the rinit binaries or busybox change.

To guarantee that nothing is downloaded or compiled (e.g. on a machine without
network or the cross toolchain), use `SKIP_BUILD=1`. It fails fast with a clear
message if a required artifact is missing:

```bash
make run SKIP_BUILD=1
```

## Running in qemu (host/x86 verification only)

The `make run` pipeline is aarch64-only. To sanity-check rinit as PID 1 on the
build host without downloading an aarch64 kernel, build the musl binary and boot
the Debian host kernel:

```bash
cargo build --release --target x86_64-unknown-linux-musl
scripts/build-initramfs.sh x86_64-unknown-linux-musl --out /tmp/ir.cpio.gz
scripts/run-qemu.sh --arch x86_64 \
    --kernel /boot/vmlinuz-6.12.111+deb13-amd64 \
    --initrd /tmp/ir.cpio.gz \
    --service "/sbin/rinit-testservice --ticks 6"
```

### rinit CLI (M0)

```
rinit [OPTIONS] [-- <SERVICE> [ARGS...]]

  -h, --help                 Show help
  -V, --version              Show version
      --dev                  Skip mounts; do not reboot (host/smoke testing)
      --test-service <CMD>   Hardcoded service (whitespace-split)
      -- <SERVICE> [ARGS]    Hardcoded service argv (kernel-cmdline form)
```

## Design decisions in this milestone

Three points differ from the written plan; all are deliberate and noted here so
they are not mistaken for drift:

1. **Signals use a self-pipe, not `tokio::signal`.** Plan §2.1④ says to unify
   on `tokio::signal`. In the verification environment tokio's global signal
   driver never woke the reactor, while a `sigaction` handler writing to a
   non-blocking `pipe2` + `AsyncFd` worked reliably. The self-pipe is the
   classic init-system pattern and keeps every `unsafe` line inside the audited
   `rinit-runtime` layer. See `crates/rinit-runtime/src/signals.rs`.
2. **`/dev` is `mkdir -p`'d before mounting `devtmpfs`.** Plan §1.3(9) allows
   skipping it, but `mount(2)` requires the target to exist, so creating it is
   strictly more robust.
3. **The mount sequence is expressed as a `MountOps` trait.** Plan §1.3(9)
   describes the order and the fatal/degradable split but not how to test it.
   Coding it against `mount(2)` directly made the whole policy untestable
   without `CAP_SYS_ADMIN`; behind a trait, `first_wave_mounts_with` runs
   against a recording fake on the host. Same shape plan §4.2 calls for.

## Measurements (plan §0.1 calibration point)

Stripped `release` binaries from this machine:

| target                              | binary              | size    |
| ----------------------------------- | ------------------- | ------- |
| `aarch64-unknown-linux-gnu` (static)| `rinit`             | 1.19 MB |
| `aarch64-unknown-linux-gnu` (static)| `rinit-testservice` | 0.99 MB |
| `x86_64-unknown-linux-musl` (static)| `rinit`             | 0.54 MB |

Both are well under the plan's ≤4 MB static budget. RSS-at-boot numbers are not
measured yet; that requires `/proc/<pid>/status` inside the guest (a later
milestone).

## Next

M1: unit-file parser + dependency DAG + parallel startup (see plan §3).
