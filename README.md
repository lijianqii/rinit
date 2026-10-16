# rinit

A lightweight, systemd-like PID 1 init and service manager written in Rust,
targeted at resource-constrained embedded Linux (aarch64). See
[`rinit-开发计划.md`](rinit-开发计划.md) for the full development plan; this
README tracks implementation status.

## Status: M0–M4 complete, M5 in progress — verified on aarch64

What is in place, and what it is proven by. Every claim below is either a host
test (`make check`) or a guest assertion (`make test-system`, 74 of them in
`scripts/system-test.sh`); "proven on the host" always means the fake-runtime or
`--dev` form of the same code path, and the guest means the real one.

| milestone | content | evidence |
| --------- | ------- | -------- |
| M0 | PID 1 via `rdinit=`, first-wave mounts (`/run` → `/proc` → `/sys` → `/dev` → `/tmp` → `/sys/fs/cgroup`) with fatal/degradable rules per plan §1.3(9), `SIGCHLD` + `waitpid(-1, WNOHANG)` reaping, panic hook to `/dev/kmsg` | guest: "mounted /run…", "mounted /sys/fs/cgroup", "no rinit panic"; host: `crates/rinit/tests/smoke.rs` |
| M1 | unit files parsed from `/etc/rinit/units`, dependency graph resolved, units started level by level with each level in parallel | guest: "resolved a boot plan", "startup levels run in dependency order", "no target was exec'd" |
| M2 | supervision (spawn, track, reap, two-stage stop), restart policy with backoff and a circuit breaker, health probes, `Type=notify` readiness, in-memory log ring | guest: "a crashed service is scheduled for restart", "the breaker tripped on the failing unit"; host: `crates/rinit/tests/m2_lifecycle.rs` (31 tests) |
| M3 | control socket + `rctl` (`list`, `status`, `start`, `stop`, `restart`, `reset-failed`, `log`, `reload`), with a documented exit-code contract | guest: "rctl list reached the control socket" and the five assertions after it; host: `crates/rctl/tests/control.rs` (8 tests) |
| M4 | soak harness for sustained churn (`make soak`), leak and descriptor fixes, `--dev` control-socket peer relaxation | `scripts/soak.sh` (see [Testing](#testing)); `crates/rinit-core/tests/allocations.rs` |
| M5 (in progress) | **§7.3, §7.4, §7.5 and §7.6 done.** cgroup v2: `CgroupCPUMax=`, `CgroupMemoryMax=`, `CgroupMemoryHigh=`, `CgroupTasksMax=` under `rinit/<unit>`, the process placing itself, stop-flow signalling of the cgroup's members, `rmdir` cleanup, and OOM-kill detection into the restart policy. Sandboxing: `User=`, `Group=`, `NoNewPrivileges=`, `CapabilityBoundingSet=`, `AmbientCapabilities=`. Hardware watchdog: `/dev/watchdog` armed at boot, fed from the supervisor loop's health checkpoint, magic-closed on shutdown. Reload: `rctl reload` re-reads the unit directory, adds, changes, defers and removes, and refuses a directory that does not resolve. Legacy daemons: `Type=forking` with `PIDFile=`, located through the file or the unit's own cgroup. **Next**: the P3 items (§7.7 templates, §7.8 `Sockets=`, §7.9 socket activation) | guest: "a limited unit was placed in its own cgroup", "the memory hard and soft limits were written in bytes", "a stopped unit's cgroup was removed", "the kernel OOM-killed a process inside the unit's cgroup", "rinit armed the guest's watchdog device", "the driver took the timeout rinit set", "the guest outlived the watchdog timeout with the device armed", "an added unit the boot target does not need was not started", "a changed unit that is running kept it for the next start", "a directory that does not resolve was refused whole", "the unit is active with the daemon the PIDFile named", "stopping a forking unit stopped the daemon it took over", "a de-privileged service ran as nobody:nogroup with NoNewPrivs", "an empty CapabilityBoundingSet left a root service with no capabilities", "ambient capabilities reached a de-privileged service"; host: `crates/rinit/src/cgroup.rs` (19 tests), `crates/rinit-core/src/unit/cgroup.rs` (9), `crates/rinit-runtime/src/syscall/cgroup.rs` (14), `crates/rinit/src/watchdog.rs` (6), `crates/rinit-runtime/src/syscall/watchdog.rs` (6), `crates/rinit/src/unit_manager.rs` (5 for the reload itself, `reload.rs` is policy over them), `crates/rinit/tests/m2_lifecycle.rs` (4, end to end), `crates/rinit/src/sandbox.rs` (14), `crates/rinit-core/src/unit/sandbox.rs` (9), `crates/rinit-runtime/src/syscall/cred.rs` (14) |

A real boot of the committed tree on the serial console (`…` elides kernel
noise; the lines below are the ones `make test-system` asserts on). The last
three lines are M2 working: `fault.service` exists to fail, is restarted, and
then stops being restarted when the breaker trips:

```
[    2.939522] Run /sbin/rinit as init process
rinit: rinit 0.1.0 starting (pid 1)
rinit: mounted /run
rinit: mounted /proc
rinit: mounted /sys
rinit: mounted /dev
rinit: mounted /tmp
rinit: mounted /sys/fs/cgroup
rinit: cgroup v2 ready at /sys/fs/cgroup
rinit: boot plan for 'multi-user.target': 12 unit(s) in 3 level(s)
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
rinit: watchdog /dev/watchdog armed: 10s timeout, feeding every 5s
rinit: reaped pid 48
rinit: fault.service will restart in 1.195s

(none) login: rinit: started fault.service as pid 50
rinit: reaped pid 50
rinit: error: fault.service hit its start limit (2 failures); not restarting
```

Note that the plan resolves **12 units** and starts **2**: everything in `units/`
is parsed and ordered, and only the boot target's closure is started. The demo
units (`priv`, `caps`, `ambient`, `logdemo`, `cglimit`, `cgrun`, `oom`) are
outside it on purpose and are driven by `rctl` in the system test. The watchdog's
`10s` is `make test-system`'s own shortening of the 30 s default (see the CLI
section above); the arm line itself is what the committed binary prints, and
`make run` shows `30s ... every 15s` for the same boot.

The two halves of §7.3, from the same console. First the limits as the *service*
reads them back - its own `/proc/<pid>/cgroup`, its own attribute files - and
then an OOM kill, where rinit's line is followed by the kernel's:

```
~ # rctl log cglimit.service
cglimit.service: self=0::/rinit/cglimit.service child=0::/rinit/cglimit.service
cglimit.service: cpu=50000 100000 mem=33554432 high=25165824 pids=64

~ # rctl start oom.service
rinit: oom.service: cgroup /sys/fs/cgroup/rinit/oom.service with memory.max (memory.max=16777216)
rinit: started oom.service as pid 105
[   18.114347] Memory cgroup out of memory: Killed process 105 (sh) total-vm:26976kB, anon-rss:16264kB, file-rss:0kB, shmem-rss:1280kB, UID:0 pgtables:80kB oom_score_adj:0
rinit: oom.service was killed by the cgroup OOM killer (memory.max=16777216)
rinit: oom.service will restart in 1.139s
```

`self=0::/rinit/cglimit.service` is the placement, with `0::` the unified
hierarchy's marker; `child=` is a process the service forked, which the kernel
put in the same cgroup without rinit doing anything. The OOM unit is a shell that
doubles a string past its own `memory.max`, so the kill comes from the *cgroup's*
OOM killer; rinit then names the limit behind the `SIGKILL` and the unit's
`Restart=on-failure` brings it back (once; the second kill trips the breaker).

§7.5's watchdog, from the same console. The device is real - `run-qemu.sh`
attaches qemu's `i6300esb` and the kernel is built with its driver - so the
timeout below is that driver's answer to rinit's `WDIOC_SETTIMEOUT`, not a
stand-in file's. The uptime is the part that matters: the device was armed with a
10 s timeout within seconds of boot, so a machine still running long after that
is a machine whose supervisor loop is feeding it.

```
~ # d=/sys/class/watchdog/watchdog0; echo wd-ident=$(cat $d/identity) wd-state=$(cat $d/state) wd-timeout=$(cat $d/timeout)
wd-ident=i6300ESB timer wd-state=active wd-timeout=10
~ # u=$(cut -d" " -f1 /proc/uptime); echo wd-uptime=$u
wd-uptime=18.64
```

The commands are written that way on purpose: the console echoes what is typed
and those lines land in the same log the assertions read, so the markers are
built from a variable (`v=populated; ... echo cgrun-$v` above, `$d/identity`
here) rather than appearing literally in the input - an assertion that its own
echo satisfies proves nothing.

§7.6's reload, which is the one feature whose evidence *has* to come from the
guest: the initramfs unpacks `units/` into a ramfs, so the console can add,
change and remove unit files under a running rinit. The four reloads below are
"nothing changed", "one added", "one stopped unit changed and one running unit
changed", and "one removed":

```
~ # rctl reload
rinit: reload: 0 added (0 started), 0 changed (0 deferred), 0 removed (0 stopped), 12 unchanged
~ # echo "[Service]" > /etc/rinit/units/reload.service
~ # echo "Type=oneshot" >> /etc/rinit/units/reload.service
~ # echo "ExecStart=/bin/echo reloaded-service" >> /etc/rinit/units/reload.service
~ # rctl reload
rinit: reload: registered reload.service
rinit: reload: 1 added (0 started), 0 changed (0 deferred), 0 removed (0 stopped), 12 unchanged
~ # echo "ExecStartPre=/bin/echo reloaded-pre" >> /etc/rinit/units/logdemo.service
~ # echo "ExecStartPost=/bin/true" >> /etc/rinit/units/getty.service
~ # rctl reload
rinit: reload: logdemo.service: new definition in force
rinit: reload: getty.service is running; its new definition applies at the next start
rinit: reload: 0 added (0 started), 2 changed (1 deferred), 0 removed (0 stopped), 11 unchanged
~ # rm /etc/rinit/units/reload.service
~ # rctl reload
rinit: reload: removed reload.service
rinit: reload: 0 added (0 started), 1 changed (1 deferred), 1 removed (0 stopped), 11 unchanged
```

`1 added (0 started)` is the plan's "注册,不自动启动(除非是 default.target 依赖)" in
one line: nothing pulls `reload.service` in, so it exists and is not running.
`getty.service is running` is the deferral - the console itself keeps the
definition it was started with, `ExecStop=` included, and takes the new one at its
next start. The last reload still counts `getty.service` as changed, which is
accurate rather than noisy: the file still differs from what is running, so the
next start still has something to adopt.

§7.10's `forking` daemon, from the same console. `forking.service` is a shell that
backgrounds a `sleep` and exits, which is the shape of a double-fork daemon - and
the one kind of unit whose process is not the process rinit started:

```
~ # rctl start forking.service
rinit: started forking.service as pid 122
rinit: forking.service: main process exited; daemon is pid 123
~ # rctl status forking.service
unit: forking.service
state: active (pid 123)
~ # rctl stop forking.service
rinit: stopping forking.service (pid 123)
rinit: reaped pid 123
```

Started as pid 122, `Active` with pid 123: the `PIDFile=` is what says which
process the unit is now, and the unit's limits, its stop signal and its cleanup
all follow that pid rather than the one that exited. `stopping ... (pid 123)` is
the half that needed a change - a daemon inherits its process group from whatever
forked it, so the group signal would have addressed a group that no longer exists,
and the stop falls back to the pid itself. The guest asserts the daemon is gone
afterwards, which an untracked daemon would not be.

And the fail-closed boundary, on a unit written at runtime:

```
~ # rctl start badfork.service
rinit: error: badfork.service: cannot locate the daemon: cannot read PIDFile /run/not-written.pid: No such file or directory (os error 2)
rinit: badfork.service exited: failed
```

`badfork.service` is `Type=forking` with a `PIDFile=` that is never written and
no `Cgroup*=` to fall back to. Both are reasons not to guess: the unit fails, says
which file it read, and its dependents are told - which is the plan's "无法定位 →
Failed" and the reason `forking` is the one type this README recommends against for
anything new.

## Layout

```
crates/
  rinit-core/         # platform-independent engine (no_std + alloc)
    src/unit/         # parser.rs, dependency.rs, config.rs, job.rs, restart.rs,
                      # notify.rs, sandbox.rs (the §7.4 drop *plan*), cgroup.rs
                      # (the §7.3 limit model), state/
    src/              # control.rs (protocol), logger.rs, users.rs, error.rs
    tests/allocations.rs
  rinit-runtime/      # syscall layer, mounts, signals, reaping (std/libc/tokio)
    src/syscall/      # process.rs, wait.rs, cred.rs (ids + capabilities), fs.rs,
                      # mount.rs, socket.rs, signal.rs, reboot.rs, cgroup.rs,
                      # watchdog.rs (the feed/disarm protocol)
    tests/syscall.rs, tests/shipped_units.rs
  rinit/              # PID 1 binary
    src/boot.rs       # boot sequence + main loop (orchestration only)
    src/unit_manager.rs, service.rs, control.rs, logs.rs, log.rs
    src/sandbox.rs    # `User=`/`Group=`/capabilities -> Sandbox (one resolver)
    src/cgroup.rs     # §7.3: where limits are applied, and the failure taxonomy
    src/watchdog.rs   # §7.5: whether this boot has one, and the feed cadence
    src/reload.rs     # §7.6: the diff a `rctl reload` applies, and its report
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

`cargo test --offline --workspace` (what `make check` runs last) executes **645
tests** across eleven binaries. The four shapes, and why each is not redundant:

- **unit tests** next to the code (core 303, rinit 171, rinit-runtime 92,
  rctl 11), including `proptest` properties over the parser;
- **integration tests** in `crates/*/tests/`: the syscall layer against the real
  kernel (`rinit-runtime` 21: run unprivileged, they assert that each drop is
  *refused* rather than silently skipped - `EPERM` from `setuid(0)`, `setgroups`,
  `PR_CAPBSET_DROP`, `capset` and the ambient raise, each naming the call that
  failed - with the root-only success paths deferred to the guest), the
  lifecycle end to end in `--dev` mode (`rinit/tests/m2_lifecycle.rs` 31), the
  control CLI (`rctl/tests/control.rs` 8), the smoke test that runs the real
  binary (`rinit/tests/smoke.rs` 2), the allocation guard
  (`rinit-core/tests/allocations.rs` 2), and `shipped_units.rs` 4 - every unit in
  `units/` parses through the loader, so a unit that would fail at boot fails
  here first;
- **the soak harness** (`scripts/soak.sh`, plan §3 M4/§4.4): runs rinit under
  churn, samples RSS/descriptors/threads/zombies, drives the control socket in a
  loop, and fails if the steady state is not flat or if `SIGTERM` does not exit 0.
  The default 60 s is a commit-time check; `make soak SOAK_ARGS='--seconds
  259200'` is the 72-hour acceptance run the plan asks for;
- **the guest system test** (`scripts/system-test.sh`, plan §4.3): boots the real
  initramfs on aarch64 qemu, types at the console to prove a root shell is
  reachable, drives `rctl` against the running system, and asserts 74 things -
  including that levels run in dependency order, that no target is ever exec'd,
  the §7.4 privilege-drop outcomes, the §7.3 limits read back out of the guest's
  own kernel, §7.5's watchdog on the real device qemu attaches, §7.6's reload
  against the guest's own (writable, ramfs) unit directory, and §7.10's forking
  daemon - located through its `PIDFile=` and then stopped as the unit's process.
  Exit codes are
  `0` pass, `1` assertion failed, `2` missing artifacts, `3` qemu could not start.
  Logs land in `build/logs/` (and are deleted on success unless `--keep-log` is
  passed).

The mount policy (plan §1.3(9)) is covered through the `MountOps` trait, so the
fatal-vs-degradable rules are tested without `CAP_SYS_ADMIN` - the "fake
runtime" shape plan §4.2 asks for. The §7.4 drop plan is the same concern
expressed as data rather than a trait: `Sandbox::steps()` returns the ordered
steps, so "`setuid` before the ambient raise" is asserted in `rinit-core`
instead of only being observable as root on a real system.

§7.3 gets both treatments, because it has both kinds of content. The value model
(directive → file → contents, and the percentage conversion) is data in
`rinit-core`, unit-tested. The file operations take the cgroup directory as a
*parameter*, so `--cgroup-root` pointed at a directory of ordinary files runs the
real path - create, write, place, read back, clean up - in `--dev` on the host,
no root and no `/sys`. What that stand-in cannot do is `rmdir` a directory full
of stand-in attribute files, which is why the removal and the live `cgroup.procs`
checks are asserted in the guest, against a real cgroupfs.

§7.5 is the same shape with the polarity reversed. The watchdog's *protocol* is
bytes on a descriptor, so the host tests point `--watchdog` at a file and read
the feed bytes and the magic close back out of it - which is also the only safe
way to test the code on a machine that has its own `/dev/watchdog`. The *ioctl*
and the hardware only exist in the guest, which is why qemu's `i6300esb` device
and a kernel built with its driver are part of the test rig rather than an
optional extra: without them the guest could only ever exercise the
"no watchdog hardware" path.

`cargo llvm-cov --offline --workspace --summary-only`, measured at the §7.10
commit. The plan's checklist asks for ≥80% overall and ≥90% in the core, so these
are the numbers that answer it rather than a shape:

| crate            | line coverage | functions |
| ---------------- | ------------- | --------- |
| `rinit-core`     | **96.71%**    | 526/538   |
| `rinit-runtime`  | 86.47%        | 253/288   |
| `rinit`          | 91.49%        | 527/570   |
| `rctl`           | 96.08%        | 27/32     |
| **total**        | **92.46%**    | 1333/1428 |

The low outliers are the ones that *cannot* run on a host, which is why the guest
tests exist rather than an omission: `syscall/reboot.rs` (0% - it would reboot the
machine), `syscall/mount.rs` (50% - needs `CAP_SYS_ADMIN`), `syscall/process.rs`
(58% - the child-side drop paths, which a host run refuses by design),
`src/main.rs` (48% - the entry point) and `src/control.rs` (76% - the socket
server, which the guest drives and the host reaches only through
`RINIT_CONTROL_DIR`). `rinit-testservice` is 0% because nothing runs it any more:
it was M0's bring-up helper, and the smoke test drives the real binary and the
real unit path instead.

```bash
cargo llvm-cov --offline --workspace --summary-only          # the table above
cargo llvm-cov --offline --workspace --html                  # target/llvm-cov/html
```

**Known gaps**, tracked rather than hidden:

- **Log persistence is not implemented** (plan §1.3(7)): `rctl log` reads an
  in-memory ring, so a restart of rinit loses everything.
- **`--dev` cannot de-privilege to another user, or drop a bounding set.** An
  unprivileged rinit gets `EPERM` from `setgid`, `setgroups` and
  `PR_CAPBSET_DROP`, so those units fail rather than run with the drop skipped -
  fail-closed by design, and the reason the §7.4 *success* paths are only ever
  proven in the guest, where rinit is root. The host tests prove the refusals.
- **The shutdown path - `reboot(2)`, the SysRq fallback, the watchdog's magic
  close - is not covered by the automated tests.** On the host it must not run
  (it would reboot the machine), and the system test kills qemu rather than
  asking the guest to shut down, so sending `SIGTERM` to PID 1 in the guest is
  the manual check. The magic close is the one piece of it with automated
  evidence: asserted against a stand-in device in `m2_lifecycle.rs`, where the
  file shows the `V` after the feed bytes. In the guest the kernel also stops the
  device on a reboot (`watchdog_stop_on_reboot`), so a graceful shutdown has two
  independent reasons not to reset the machine.
- **What a watchdog is *for* cannot be tested without hanging rinit.** The reset
  itself is not exercised: proving it means deliberately wedging PID 1 until the
  hardware fires, which would end the run being validated. What is proven is
  everything short of it - the device is opened and reported, the timeout reaches
  a real driver, the supervisor loop feeds it (the guest outlives a timeout it
  would not survive unfed), and the guest boots exactly once.
- **A cgroup limit is only proven against the controllers the running kernel
  has.** The guest's kernel is built with all four the plan's directives need
  (the Makefile checks that), so the *skip* branches - no `pids` controller, no
  `cpu.max` - are exercised on the host, against a stand-in directory that simply
  omits the attribute. The same goes for a write the kernel *refuses* (any errno
  but `ENOENT`): a directory where the attribute file should be stands in for it,
  because making a real cgroupfs refuse a limit needs a value or a kernel this
  test does not control.
- **A `forking` unit that never detaches is swept by cgroup, so a unit with no
  `Cgroup*=` can still leave a daemon behind.** A `Type=forking` unit whose main
  process never exits is failed by `TimeoutStartSec`, and the sweep kills the
  process group and then everything in the unit's cgroup (plan §7.3's second
  stage). A daemon that left *both* - a double-fork that called `setsid`, under a
  unit that declares no cgroup - has nothing left pointing at it. The happy path
  needs no cgroup, because `PIDFile=` names the daemon; this is only the timeout
  path of a unit that is misbehaving.
- `signals.rs`'s pipe-full path (handler drops a byte) is not forced in a test.
- RSS-at-boot is not measured yet, so plan §0.1's memory column is still open
  (`/proc/<pid>/status` inside the guest: ≤2 MB idle, ≤6 MB with 20 services),
  as is the "20 services in parallel, faster than serial" comparison. The size
  numbers below are binary sizes, not a memory measurement.
- No `valgrind`, `miri` (stable only) or `cargo-fuzz` in this environment, so
  leaks are caught by the soak harness's sampled curve rather than by tooling.

## Acceptance: plan §6's checklist

Plan §6 is the definition of done. Every line of it is either a command in this
repository or a measurement in this README:

| plan §6 | where it stands |
| ------- | --------------- |
| boots as PID 1 in qemu (128M) with no kernel panic | `make test-system`: 77 assertions, including "no rinit panic" and four kernel-log checks |
| 20 services in parallel, visibly faster than serial | **19.2x**: `make parallel` - 20 `oneshot` services that each sleep 1 s finish in 1065 ms together and 20 424 ms chained |
| crash-restart, breaker, health checks | `make test-system` ("a crashed service is scheduled for restart", "the breaker tripped on the failing unit") and `crates/rinit/tests/m2_lifecycle.rs` |
| `rctl` can start, stop, query and read logs | `make test-system`'s M3 block and `crates/rctl/tests/control.rs` |
| graceful shutdown, no zombies, no fd leak | `make soak` samples descriptors, threads and zombie children through restart churn, and asserts each comes back to its baseline |
| 72 h soak with stable RSS | **not run here.** `make soak` is the harness and `make soak SOAK_ARGS='--seconds 259200'` is the run; what has been run is a five-minute one - 149778 restart cycles, RSS 4668 kB → 4672 kB (+4 kB) across the second half, descriptors 21 → 23, zero zombie children |
| binaries ≤2 MB dynamic / ≤4 MB static | [Measurements](#measurements-plan-01-calibration-point): 1.45 MiB, static aarch64 |
| unit test coverage ≥80% overall, ≥90% in the core | **92.46%** and **96.71%** (llvm-cov, [above](#testing)) |
| documentation: architecture, unit format, `rctl`, porting | this README (architecture, status, measurements), [`docs/units.md`](docs/units.md) (every directive, its value syntax and its default), [`docs/porting.md`](docs/porting.md) (kernel, userspace, build, differences from systemd), and `rinit-开发计划.md` |

One number the checklist does not ask for, but plan §0.1's design goals do: rinit's
RSS, measured *in the guest* out of `/proc/1/status` rather than on the host -
**1532 kB idle** and **1720 kB with 20 services running**, against §0.1's 2 MB and
6 MB. The growth is the interesting part: twenty more units cost under 200 kB,
because a unit is a config, a state and a few timers rather than a thread.

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
5. Boot it in `qemu-system-aarch64 -machine virt -cpu cortex-a57 -m 128M` with
   `-device i6300esb`, the watchdog rinit arms (plan §7.5).

There is nothing to configure: `make run` always builds whatever is out of date
and boots the unit directory. Re-running it does no downloading and no
compiling — the file targets below already encode that, which is why there is no
"skip the build" switch. One exception proves the rule: `kernel-config-check`
runs on every `make kernel`, because a kernel built from a config without the
symbols §7.3 and §7.5 need is a guest where a limit or the watchdog silently
does not exist.

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
      --cgroup-root <DIR>    cgroup v2 hierarchy for unit limits
                             (default: /sys/fs/cgroup)
      --watchdog <DEVICE>    Hardware watchdog to arm and feed
                             (default: /dev/watchdog; not opened in --dev)
      --watchdog-timeout <S> Hardware timeout in seconds; fed every half
                             (default: 30)
      --test-service <CMD>   Launch this instead of the unit directory
      -- <SERVICE> [ARGS]    Service as an argv (kernel-cmdline form)
```

With no `--test-service`, rinit boots the unit directory. `--test-service` (and
the `RINIT_TEST_SERVICE` environment variable) is the bring-up escape hatch
that bypasses it; the guest never sets it.

`--cgroup-root` exists for a system that mounts the hierarchy somewhere else,
and it is also the seam the host tests use: pointed at a directory of ordinary
files, the whole §7.3 path - create, write, place, read back - runs without
root, without `CAP_SYS_ADMIN` and without `/sys`. `--dev` does not touch the
machine's own hierarchy: with the mounts skipped, the limits are skipped too
(with a log line), because a developer host's `/sys/fs/cgroup` is not rinit's
to carve up. An explicit `--cgroup-root` wins over that.

`--watchdog` and `--watchdog-timeout` are §7.5, and `--dev` refuses the default
device for a sharper reason than convenience: opening `/dev/watchdog` starts a
hardware timer, and a host running `make check` is not going to feed it. A
device named explicitly is still honoured, which is how the host tests drive the
byte protocol against a file. On a kernel command line the two flags go in their
space-separated form - the kernel hands a `-flag=value` token to init as an
*environment* entry, not an argument (`init/main.c`), so `--watchdog-timeout 10`
reaches rinit and `--watchdog-timeout=10` does not.

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
  reload                  Re-read the unit directory and apply the difference

Exit codes: 0 success  1 error  2 usage  3 no such unit  4 operation conflict
```

`--tail N` returns the last N lines from the ring, with `0` meaning everything
buffered. The ring keeps a bounded number of bytes per unit (`LINE_OVERHEAD` is
part of that accounting, so a unit name cannot make the budget lie), and the
protocol is framed with a 64 KiB limit (`MAX_FRAME_BYTES`), which is what stops
a length prefix - on either side - from becoming an unbounded allocation.

`reload` (plan §7.6) is the one command that is about the directory rather than a
unit, so it takes no arguments, and its reply is a report: what was registered,
what took a new definition at once and what is waiting for a restart, what was
stopped and removed, and the counts. A directory rinit cannot resolve - a parse
error, a cycle, a `Requires=` with nothing behind it - is answered with an error
that says the running configuration is unchanged, and it *is*: nothing is touched
until the whole new schedule exists (see `crates/rinit/src/reload.rs`).

## Design decisions and deliberate deviations from the plan

Twenty points differ from the written plan or pin down something it leaves
open. All are noted here so they are not mistaken for drift:

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
10. **The child places itself in its cgroup, not the parent.** Plan §7.3 says
    "先 fork,父进程(仍是 root)把子进程 PID 写入 `cgroup.procs`,子进程再降权". The
    parent cannot do it at the right time: `spawn_sandboxed` returns only after
    the child has `exec`'d (the report pipe is what carries a failed `setuid` in
    the child back to the parent), so a parent-side write would land after the
    service is already running — and a service that forked in that window would
    have put a process into rinit's cgroup, outside its own limits. The child
    writes its own pid as its first action (after `setsid`, the descriptor
    shuffling and `chdir`, before any §7.4 drop), while it still owns
    `cgroup.procs` because it is still root. Same order as the plan, one process
    over; see `cgroup::join_from_child`.
11. **A `HealthCheck=` probe, an `ExecStop=` command and
    `ExecStartPre=`/`ExecStartPost=` run outside the unit's cgroup.** They are
    separate processes rinit tracks by pid, and having them join would make the
    unit's cgroup look populated for as long as they run — which is exactly what
    §7.3's `populated=0` rule reads as "keep this directory".
12. **A limit the *kernel* cannot express is skipped, not fatal.** §7.3 names one
    degrade case (no hierarchy at all). Two more are the same kind of thing: a
    controller the kernel was not built with (`CONFIG_CGROUP_PIDS` off, so
    `pids.max` does not exist), and an attribute that is absent while its
    controller is present — `cpu.max` is built only under
    `CONFIG_CFS_BANDWIDTH`, and the kernel answers `ENOENT` for it. Both warn,
    skip that limit and let the unit run: a unit file written for one kernel has
    to keep working on another. A write the kernel *refuses* — any other errno,
    meaning the attribute is there and the value was not accepted — still fails
    the start, because that is a unit file claiming a bound the service does not
    have.
13. **The kernel config is part of the build.** §7.3 puts this in M5's hands
    ("否则 M5 需检查内核 config"): the Makefile's `.config` rule turns
    `CONFIG_CFS_BANDWIDTH` on after `defconfig`, and then *checks* the four
    symbols the four directives hang off (`CGROUP_SCHED`, `CFS_BANDWIDTH`,
    `MEMCG`, `CGROUP_PIDS`). It has to check, because `olddefconfig` silently
    drops a symbol whose dependencies are unmet, and a missing `cpu.max` would
    otherwise show up as a guest boot in which a limit does not exist.
14. **An OOM kill is detected, not inferred, and `Restart=` still decides.**
    §7.3's "检测到 OOM kill 后进入重启策略" is read as *diagnosis* plus the existing
    policy rather than as a policy of its own: rinit reads `memory.events`'
    `oom_kill` counter and names the memory limit behind a `SIGKILL`, and the
    unit's `Restart=` then does what it says. Forcing a restart on every unit
    the cgroup OOM killer took would override `Restart=no`, which is a unit
    author's explicit decision. The counter is compared against the value read
    before the service could allocate anything, so a restart that reuses a kept
    cgroup directory is not misread as a fresh kill.
15. **`PermissionsStartOnly=` and the `+`/`-`/`!` command prefixes are not
    supported**, which §7.4 calls out as a decision to record rather than a gap
    to close. Every `Exec*` command - `ExecStart`, `ExecStartPre`,
    `ExecStartPost`, `ExecStop`, `HealthCheck` - runs as `User=`/`Group=`, so a
    preparation step cannot quietly run as root: `run_aux_command` resolves the
    same sandbox the service gets. Work that genuinely needs root belongs in a
    system oneshot (`rinit-init.service`), not in an `ExecStartPre=`.
16. **The watchdog is configured by flags, not by `/etc/rinit.conf`.** §7.5
    offers "编译期或 `/etc/rinit.conf` 全局开关" and shows a `[Global]` section.
    rinit has no global config file, and every other boot knob is a flag, so
    `--watchdog`/`--watchdog-timeout` carry the same two values
    (`WatchdogDevice=`, `WatchdogTimeoutSec=`) through the mechanism that
    already exists. A config file would add a parser, a second boot-time failure
    mode, and a second place to look for one setting.
17. **The watchdog is armed when the supervisor loop starts, not at the top of
    the boot.** §7.5 says "启动时打开" and then requires the feed to happen at the
    main loop's health checkpoint. Arming it earlier would mean feeding from the
    boot sequence as well, and a level that legitimately takes longer than the
    timeout - a service with a long `TimeoutStartSec=`, a slow filesystem -
    would reset a machine that is booting correctly. The window that is left
    uncovered (mounts, unit loading, the first start) is bounded by the start
    timeouts; the loop that follows is where an init silently wedges. A
    `--test-service` boot, which is a bring-up path rather than a system boot,
    arms nothing.
18. **The guest is *given* a watchdog, because the target qemu machine has
    none.** `-device i6300esb` in `scripts/run-qemu.sh` plus
    `CONFIG_I6300ESB_WDT=y` in the kernel the Makefile builds is what makes
    `/dev/watchdog` a real device behind a real driver, so the timeout the guest
    reads back out of `/sys/class/watchdog/watchdog0/timeout` is the driver's
    answer to rinit's ioctl rather than a stand-in file's. `CONFIG_WATCHDOG_SYSFS`
    is on for the same reason: without it there is no attribute to read the
    answer from. The x86_64 qemu path boots the host's `vmlinuz`, whose watchdog
    driver is a module that is not in that initramfs, so it has no device and
    rinit reports nothing - correct behaviour, but not evidence.
19. **A reload does not cascade.** §7.6 says a removed unit is stopped and
    removed, and says nothing about the units that depend on it - so a reload
    that deletes a unit another one still `Requires=` is *refused* (the reference
    is dangling, and reference integrity is checked before anything is applied,
    plan §1.3(1)) rather than leaving a running dependent without its dependency.
    Removing a unit therefore means removing the references to it in the same
    reload, which is the one thing the operator has to remember; the error names
    the unit. The other half of "removed" is a tombstone rather than a deletion:
    the runtime has to stay while the stop it asked for is in flight (`SIGKILL`
    deadlines and reaping need it), so it is hidden from every protocol answer at
    once and dropped by the next reload that finds it still gone and now down.
20. **A `forking` unit's `PIDFile=` is read once, and the daemon has to be in the
    unit's cgroup - or already there.** The plan says "主进程退出(码 0)且能定位到守护
    进程 → 进入 `Active`", which rinit takes literally: the file is read the moment
    the main process is reaped, and a daemon that writes it afterwards fails the
    start. systemd polls for a few seconds; this does not, so a legacy daemon
    that has not written its pid by the time its parent exits needs a wrapper
    that has. The order the plan gives is kept - the file first, the cgroup as the
    fallback - which is also why a `forking` unit with no `PIDFile=` needs a
    `Cgroup*=` directive to be locatable at all, and fails closed ("no PIDFile=
    and no cgroup of its own") rather than guessing from the process table. Two
    smaller consequences are recorded here because they are what the code does: a
    pid the file names that is *not* in the unit's cgroup is moved into it (that
    is the plan's "写入对应 cgroup", and it is what makes the unit's limits, its
    stop signalling and its cleanup apply to the daemon), and the stop path
    signals the unit's process *group* and then the pid itself - a daemon
    inherited its group from whatever forked it, so the group signal can address
    a group that no longer exists while the daemon is alive.

## Measurements (plan §0.1 calibration point)

Stripped `release` binaries (`panic="abort"`, `lto=true`, `codegen-units=1`),
byte counts as `ls -l` reports them:

| target                               | binary              | size                    | in an image |
| ------------------------------------ | ------------------- | ----------------------- | ----------- |
| `aarch64-unknown-linux-gnu` (static) | `rinit`             | 1.45 MiB (1 518 640 B)  | yes         |
| `aarch64-unknown-linux-gnu` (static) | `rinit-testservice` | 1.01 MiB (1 059 704 B)  | no          |
| `x86_64-unknown-linux-musl` (static) | `rinit`             | 0.92 MiB (967 584 B)    | yes         |

`rinit-testservice` is a host-side bring-up helper (it proves fork/exec/reap
deterministically), so it is built but not shipped: the guest's services come
from `units/` and busybox.

The packed image the guest boots
(`build/out/initramfs-aarch64-unknown-linux-gnu.cpio.gz`: static busybox +
rinit + `units/`) is 2.26 MiB (2 365 968 B) gzipped.

Both binaries are inside the plan §0.1 budget: the target is ≤2 MB per
executable, with static builds allowed up to 4 MB. RSS-at-boot is not measured
yet; that needs a sampling loop in the guest (the soak harness measures the host
`--dev` process instead).

## Next

The rest of the plan's extension list is P3: §7.7 template units and `%i`, §7.8
inter-unit socket dependencies, §7.9 socket activation, and §7.11's terminal
directives - each if it earns its place against §2.2's scope.

Plan §6's checklist is now answered item by item in the [Acceptance
section](#acceptance-plan-6s-checklist), with one exception: the 72 h soak, where
the harness is what this repository has and the long run is a deployment-time
exercise.
