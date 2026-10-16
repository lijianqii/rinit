# rinit

A lightweight, systemd-like PID 1 init and service manager written in Rust,
targeted at resource-constrained embedded Linux (aarch64). See
[`rinit-开发计划.md`](rinit-开发计划.md) for the full development plan; this
README tracks implementation status.

## Status: M0–M4 complete, M5 in progress — verified on aarch64

What is in place, and what it is proven by. Every claim below is either a host
test (`make check`) or a guest assertion (`make test-system`, 41 of them in
`scripts/system-test.sh`); "proven on the host" always means the fake-runtime or
`--dev` form of the same code path, and the guest means the real one.

| milestone | content | evidence |
| --------- | ------- | -------- |
| M0 | PID 1 via `rdinit=`, first-wave mounts (`/run` → `/proc` → `/sys` → `/dev` → `/tmp`) with fatal/degradable rules per plan §1.3(9), `SIGCHLD` + `waitpid(-1, WNOHANG)` reaping, panic hook to `/dev/kmsg` | guest: "mounted /run…", "no rinit panic"; host: `crates/rinit/tests/smoke.rs` |
| M1 | unit files parsed from `/etc/rinit/units`, dependency graph resolved, units started level by level with each level in parallel | guest: "resolved a boot plan", "startup levels run in dependency order", "no target was exec'd" |
| M2 | supervision (spawn, track, reap, two-stage stop), restart policy with backoff and a circuit breaker, health probes, `Type=notify` readiness, in-memory log ring | guest: "a crashed service is scheduled for restart", "the breaker tripped on the failing unit"; host: `crates/rinit/tests/m2_lifecycle.rs` (19 tests) |
| M3 | control socket + `rctl` (`list`, `status`, `start`, `stop`, `restart`, `reset-failed`, `log`), with a documented exit-code contract | guest: "rctl list reached the control socket" and the five assertions after it; host: `crates/rctl/tests/control.rs` (6 tests) |
| M4 | soak harness for sustained churn (`make soak`), leak and descriptor fixes, `--dev` control-socket peer relaxation | `scripts/soak.sh` (see [Testing](#testing)); `crates/rinit-core/tests/allocations.rs` |
| M5 (in progress) | **§7.4 done**: `User=`, `Group=`, `NoNewPrivileges=`, `CapabilityBoundingSet=`, `AmbientCapabilities=`. **Next**: §7.3 cgroup v2 | guest: "a de-privileged service ran as nobody:nogroup with NoNewPrivs", "an empty CapabilityBoundingSet left a root service with no capabilities", "ambient capabilities reached a de-privileged service"; host: `crates/rinit/src/sandbox.rs` (14 tests), `crates/rinit-core/src/unit/sandbox.rs` (9), `crates/rinit-runtime/src/syscall/cred.rs` (16) |

A real boot of the committed tree, `make run`, serial console (`…` elides kernel
noise). The last three lines are M2 working: `fault.service` exists to fail, is
restarted, and then stops being restarted when the breaker trips:

```
[    2.925620] Run /sbin/rinit as init process
rinit: rinit 0.1.0 starting (pid 1)
rinit: mounted /run
rinit: mounted /proc
rinit: mounted /sys
rinit: mounted /dev
rinit: mounted /tmp
rinit: boot plan for 'multi-user.target': 9 unit(s) in 3 level(s)
rinit: plan 0: rinit-init.target (target)
rinit: plan 1: fault.service (service)
rinit: plan 1: getty.service (service)
rinit: plan 2: multi-user.target (target)
rinit: starting level 0: 0 service(s)
rinit: level 0 reached: rinit-init.target active
rinit: starting level 1: 2 service(s)
rinit: started fault.service as pid 48
rinit: started getty.service as pid 49
rinit: starting level 2: 0 service(s)
rinit: level 2 reached: multi-user.target active
rinit: boot complete: 2 unit(s) started from 'multi-user.target' (2 still running)
rinit: reaped pid 48
rinit: fault.service will restart in 1.195s

(none) login: rinit: started fault.service as pid 50
rinit: reaped pid 50
rinit: error: fault.service hit its start limit (2 failures); not restarting
```

Note that the plan resolves **9 units** and starts **2**: everything in `units/`
is parsed and ordered, and only the boot target's closure is started. The demo
units (`priv`, `caps`, `ambient`, `logdemo`) are outside it on purpose and are
driven by `rctl` in the system test.

## Layout

```
crates/
  rinit-core/         # platform-independent engine (no_std + alloc)
    src/unit/         # parser.rs, dependency.rs, config.rs, job.rs, restart.rs,
                      # notify.rs, sandbox.rs (the §7.4 drop *plan*), state/
    src/              # control.rs (protocol), logger.rs, users.rs, error.rs
    tests/allocations.rs
  rinit-runtime/      # syscall layer, mounts, signals, reaping (std/libc/tokio)
    src/syscall/      # process.rs, wait.rs, cred.rs (ids + capabilities), fs.rs,
                      # mount.rs, socket.rs, signal.rs, reboot.rs
    tests/syscall.rs, tests/shipped_units.rs
  rinit/              # PID 1 binary
    src/boot.rs       # boot sequence + main loop (orchestration only)
    src/unit_manager.rs, service.rs, control.rs, logs.rs, log.rs
    src/sandbox.rs    # `User=`/`Group=`/capabilities -> Sandbox (one resolver)
    src/notify.rs, readiness.rs, deadline.rs, args.rs, manager.rs
    tests/m2_lifecycle.rs, tests/smoke.rs
  rctl/               # control CLI (main.rs, parse.rs, client.rs) + tests
units/                # unit files; getty.service is `make run`'s console login
scripts/              # fetch/kernel/busybox/initramfs/qemu/system-test/soak
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

Four layers, cheapest first:

```bash
make check          # fmt + clippy -D warnings + host tests  (no cross toolchain)
make soak           # 60s of sustained churn; watches RSS/fds/zombies
make test-system    # boot aarch64 qemu and assert the acceptance list
make test           # check + test-system
```

`cargo test --offline --workspace` (what `make check` runs last) executes **562
tests** across eleven binaries. The four shapes, and why each is not redundant:

- **unit tests** next to the code (core 293, rinit 136, rinit-runtime 70,
  rctl 10), including `proptest` properties over the parser;
- **integration tests** in `crates/*/tests/`: the syscall layer against the real
  kernel (`rinit-runtime` 21: run unprivileged, they assert that each drop is
  *refused* rather than silently skipped - `EPERM` from `setuid(0)`, `setgroups`,
  `PR_CAPBSET_DROP`, `capset` and the ambient raise, each naming the call that
  failed - with the root-only success paths deferred to the guest), the
  lifecycle end to end in `--dev` mode (`rinit/tests/m2_lifecycle.rs` 19), the
  control CLI (`rctl/tests/control.rs` 6), the smoke test that runs the real
  binary (`rinit/tests/smoke.rs` 2), and `shipped_units.rs` 3 - every unit in
  `units/` parses through the loader, so a unit that would fail at boot fails
  here first;
- **the soak harness** (`scripts/soak.sh`, plan §3 M4/§4.4): runs rinit under
  churn, samples RSS/descriptors/threads/zombies, drives the control socket in a
  loop, and fails if the steady state is not flat or if `SIGTERM` does not exit 0.
  The default 60 s is a commit-time check; `make soak SOAK_ARGS='--seconds
  259200'` is the 72-hour acceptance run the plan asks for;
- **the guest system test** (`scripts/system-test.sh`, plan §4.3): boots the real
  initramfs on aarch64 qemu, types at the console to prove a root shell is
  reachable, drives `rctl` against the running system, and asserts 41 things -
  including that levels run in dependency order, that no target is ever exec'd,
  and the §7.4 privilege-drop outcomes. Exit codes are `0` pass, `1` assertion
  failed, `2` missing artifacts, `3` qemu could not start. Logs land in
  `build/logs/` (and are deleted on success unless `--keep-log` is passed).

The mount policy (plan §1.3(9)) is covered through the `MountOps` trait, so the
fatal-vs-degradable rules are tested without `CAP_SYS_ADMIN` - the "fake
runtime" shape plan §4.2 asks for. The §7.4 drop plan is the same concern
expressed as data rather than a trait: `Sandbox::steps()` returns the ordered
steps, so "`setuid` before the ambient raise" is asserted in `rinit-core`
instead of only being observable as root on a real system.

Measured with `llvm-cov` **at M1**; not re-measured since, and treated as a
shape rather than a current score. Reproduce with the commands below before
quoting any number.

| area                                   | region coverage (M1) |
| -------------------------------------- | -------------------- |
| `rinit-core` unit naming + errors      | 100%                 |
| `rinit/src/args.rs` (CLI parsing)      | 100%                 |
| `rinit/src/service.rs` (supervision)   | ~95%                 |
| `rinit-runtime/src/error.rs`           | 99%                  |
| `rinit-runtime/src/mounts.rs`          | ~90%                 |
| `rinit-runtime/src/signals.rs`         | ~85%                 |
| `rinit-runtime` syscall wrappers       | 94–98%               |
| `rinit/src/boot.rs` (orchestration)    | ~65%                 |

```bash
RUSTFLAGS="-C instrument-coverage" CARGO_TARGET_DIR=target-cov \
    cargo test --offline --workspace
llvm-profdata merge -sparse $(find . -name '*.profraw') -o target-cov/merged.profdata
llvm-cov report -instr-profile=target-cov/merged.profdata \
    --ignore-filename-regex='/\.cargo/|/rustc/' \
    -object target-cov/debug/deps/<each-test-binary>
```

**Known gaps**, tracked rather than hidden:

- **Log persistence is not implemented** (plan §1.3(7)): `rctl log` reads an
  in-memory ring, so a restart of rinit loses everything.
- **`--dev` cannot de-privilege to another user, or drop a bounding set.** An
  unprivileged rinit gets `EPERM` from `setgid`, `setgroups` and
  `PR_CAPBSET_DROP`, so those units fail rather than run with the drop skipped -
  fail-closed by design, and the reason the §7.4 *success* paths are only ever
  proven in the guest, where rinit is root. The host tests prove the refusals.
- `boot.rs`'s reboot/halt paths are only exercised in the guest; on the host they
  must not run (they would reboot the machine).
- `signals.rs`'s pipe-full path (handler drops a byte) is not forced in a test.
- RSS-at-boot is not measured yet, so plan §0.1's memory column is still open
  (`/proc/<pid>/status` inside the guest: ≤2 MB idle, ≤6 MB with 20 services),
  as is the "20 services in parallel, faster than serial" comparison. The size
  numbers below are binary sizes, not a memory measurement.
- No `valgrind`, `miri` (stable only) or `cargo-fuzz` in this environment, so
  leaks are caught by the soak harness's sampled curve rather than by tooling.

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

There is nothing to configure: `make run` always builds whatever is out of date
and boots the unit directory. Re-running it does no downloading and no
compiling — the file targets below already encode that, which is why there is no
"skip the build" switch.

```bash
make run            # build if needed, boot → interactive root shell
make kernel         # kernel only
make busybox        # busybox only
make initramfs      # rinit + initramfs only
make qemu           # alias for `make run`
make check          # fmt + clippy + host tests (no cross toolchain)
make soak           # churn + memory/fd/zombie regression check (host, --dev)
make test           # check + the qemu system test
make MIRROR=https://... run   # different source mirror
make KERNEL_VERSION=6.6.158 BUSYBOX_VERSION=1.36.1 ...   # pin versions
```

The image is rebuilt whenever `rinit`, `busybox`, a file in `units/`, or one of
the packing scripts changes — editing `units/getty.service` repacks the
initramfs, because the unit directory is what the guest boots.

### Getting a shell in the guest

`make run` boots `units/getty.service`, which puts a login prompt on the serial
console. Type `root` and press enter — the password field in `/etc/passwd` is
empty, so there is nothing to type after that. The transcript above is that
prompt.

This is deliberately not a security boundary: anyone who can reach the serial
port already has the machine, and a development initramfs is not the place for
real credentials. Giving root a password would mean writing `/etc/shadow`,
which is not shipped.

`units/shell.service` is the alternative: it skips `login` entirely, so you get
a `~ #` prompt with no username to type. Both units want `ttyAMA0` and two
processes on one console both misbehave, so they are alternatives rather than
dependencies — swap the two `Requires=` lines in `multi-user.target` to switch.

`poweroff -f` from the shell halts the guest; `exit` returns to the boot loop.

Exiting qemu: `SIGTERM` to PID 1 is not reachable from the qemu monitor, so use
`Ctrl-A X` (qemu) to quit.

### How the incremental build works

`make run` is incremental by construction: every step is a file target, so a
step whose output already exists and is newer than its inputs is skipped.
Re-running it after a successful build does no downloading and no compiling —
it just boots. In particular:

- a cached tarball in `build/dl/` is reused (no re-download);
- `linux-6.6.158/arch/arm64/boot/Image` present → kernel is not rebuilt;
- `busybox-1.36.1/busybox` present → busybox is not rebuilt;
- `target/.../release/rinit` newer than the sources → `cargo build` is not
  re-run;
- the initramfs is repacked when rinit, busybox, a file in `units/`, or one of
  the packing scripts changes.

Because that already holds, there is no `SKIP_BUILD` knob to forget to pass.

## Running in qemu (host/x86 verification only)

The `make run` pipeline is aarch64-only. To sanity-check rinit as PID 1 on the
build host without downloading an aarch64 kernel, build the musl binary and boot
the Debian host kernel:

```bash
cargo build --offline --release --target x86_64-unknown-linux-musl
scripts/build-initramfs.sh x86_64-unknown-linux-musl --busybox-src build/src/busybox-1.36.1 --out /tmp/ir.cpio.gz
scripts/run-qemu.sh --arch x86_64 \
    --kernel /boot/vmlinuz-6.12.111+deb13-amd64 \
    --initrd /tmp/ir.cpio.gz
```

### rinit CLI

```
rinit [OPTIONS] [-- <SERVICE> [ARGS...]]

  -h, --help                 Show help
  -V, --version              Show version
      --dev                  Skip mounts; do not reboot (host/smoke testing)
      --units <DIR>          Unit directory (default: /etc/rinit/units)
      --default <TARGET>     Boot target (default: multi-user.target)
      --test-service <CMD>   Launch this instead of the unit directory
      -- <SERVICE> [ARGS]    Service as an argv (kernel-cmdline form)
```

With no `--test-service`, rinit boots the unit directory. `--test-service` (and
the `RINIT_TEST_SERVICE` environment variable) is the bring-up escape hatch
that bypasses it; the guest never sets it.

### rctl

```
rctl <COMMAND> [ARGS]

  list                    List all known units
  status [UNIT] [--tail N] Show unit status (optionally with recent log)
  start <UNIT>            Start a unit
  stop <UNIT>             Stop a unit
  restart <UNIT>          Restart a unit
  reset-failed <UNIT>     Clear a unit's failed/start-limit state
  log [UNIT] [--tail N]   Show buffered log

Exit codes: 0 success  1 error  2 usage  3 no such unit  4 operation conflict
```

`--tail N` returns the last N lines from the ring, with `0` meaning everything
buffered. The ring keeps a bounded number of bytes per unit (`LINE_OVERHEAD` is
part of that accounting, so a unit name cannot make the budget lie), and the
protocol is framed with a 64 KiB limit (`MAX_FRAME_BYTES`), which is what stops
a length prefix - on either side - from becoming an unbounded allocation.

## Design decisions and deliberate deviations from the plan

Nine points differ from the written plan or pin down something it leaves open.
All are noted here so they are not mistaken for drift:

1. **Signals use a self-pipe, not `tokio::signal`.** Plan §2.1④ says to unify on
   `tokio::signal`. In the verification environment tokio's global signal driver
   never woke the reactor, while a `sigaction` handler writing to a non-blocking
   `pipe2` + `AsyncFd` worked reliably. The self-pipe is the classic init-system
   pattern and keeps every `unsafe` line inside the audited `rinit-runtime`
   layer. See `crates/rinit-runtime/src/signals.rs`.
2. **`/dev` is `mkdir -p`'d before mounting `devtmpfs`.** Plan §1.3(9) allows
   skipping it, but `mount(2)` requires the target to exist, so creating it is
   strictly more robust.
3. **The mount sequence is expressed as a `MountOps` trait.** Plan §1.3(9)
   describes the order and the fatal/degradable split but not how to test it.
   Coding it against `mount(2)` directly made the whole policy untestable
   without `CAP_SYS_ADMIN`; behind a trait, `first_wave_mounts_with` runs
   against a recording fake on the host. Same shape plan §4.2 calls for.
4. **No `initgroups(3)`.** Plan §7.4 names it, but it goes through glibc's NSS,
   which loads modules at runtime — exactly what §7.4 elsewhere forbids for the
   `getpwnam`/`getgrnam` path, and worst in a static binary. The supplementary
   groups are computed from `/etc/group` in `rinit_core::users` and applied with
   `setgroups(2)` instead. Same result, no dynamic loader, and testable.
5. **The capability steps are split around `setuid`.** Plan §7.4 lists one
   `capset()` for the bounding and ambient sets, before `setgid`. That cannot be
   used as written: `cap_emulate_setxuid` in `security/commoncap.c` clears the
   ambient set on every root-to-non-root `setuid`, `PR_SET_KEEPCAPS` or not, so
   an ambient capability raised there would be gone by `exec`. The bounding set
   half still runs early, which is what §7.4's stated rationale ("after `setuid`
   there may be no `CAP_SETPCAP` left") is about; the ambient raise runs after
   the uid change. The order is data (`SandboxStep`), so it is a unit test rather
   than something only root on a real system can check.
6. **An empty `CapabilityBoundingSet=` is a step, not an absence.** It is the
   strictest setting §7.4 offers, and it is implemented as "drop every
   capability", which for a service that stays root is what leaves it with
   nothing (`handle_privileged_root` builds a root process's post-`exec` set as
   `bset | inheritable`).
7. **An unknown name fails the unit, always.** Unknown `User=`, `Group=` or
   capability name: no start, no default. The alternative - running as root, or
   with a quietly smaller capability set - makes a typo into a security change
   that only shows up later, far from the unit file.
8. **The initramfs image is root-owned and traversable.** The archive is built
   with `cpio --owner=0:0`, and its root directory entry is `0755`, because that
   entry becomes the image's `/` mode. The first version was built by hand from
   a `0700` work directory owned by the build user, and every `exec` in the guest
   failed `EACCES` — a permission bug that no host test can see.
9. **`--dev` cannot apply a privilege drop, so it fails the unit instead.** An
   unprivileged rinit gets `EPERM` from `setgid`, `setgroups` and
   `PR_CAPBSET_DROP`; carrying on without the drop would be exactly the failure
   §7.4 exists to prevent. It is also why the §7.4 success paths are only proven
   in the guest, and why those tests print the log they assert on.

## Measurements (plan §0.1 calibration point)

Stripped `release` binaries (`panic="abort"`, `lto=true`, `codegen-units=1`),
byte counts as `ls -l` reports them:

| target                               | binary              | size                    | in an image |
| ------------------------------------ | ------------------- | ----------------------- | ----------- |
| `aarch64-unknown-linux-gnu` (static) | `rinit`             | 1.45 MiB (1 518 640 B)  | yes         |
| `aarch64-unknown-linux-gnu` (static) | `rinit-testservice` | 1.01 MiB (1 059 704 B)  | no          |
| `x86_64-unknown-linux-musl` (static) | `rinit`             | 0.90 MiB (938 912 B)    | yes         |

`rinit-testservice` is a host-side bring-up helper (it proves fork/exec/reap
deterministically), so it is built but not shipped: the guest's services come
from `units/` and busybox.

The packed image the guest boots
(`build/out/initramfs-aarch64-unknown-linux-gnu.cpio.gz`: static busybox +
rinit + `units/`) is 2.24 MiB (2 353 264 B) gzipped.

Both binaries are inside the plan §0.1 budget: the target is ≤2 MB per
executable, with static builds allowed up to 4 MB. RSS-at-boot is not measured
yet; that needs a sampling loop in the guest (the soak harness measures the host
`--dev` process instead).

## Next

The rest of M5, in rough order: §7.3 cgroup v2 (CPU/memory/task limits per unit,
`rinit/<unit>` under `/sys/fs/cgroup`, degrade with a warning when cgroup v2 is
absent), then the watchdog, then socket activation if it earns its place, then
the docs/examples/acceptance checklist.
