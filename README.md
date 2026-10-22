# rinit

A lightweight, systemd-like PID 1 init and service manager written in Rust,
targeted at resource-constrained embedded Linux (aarch64). See
[`rinit-开发计划.md`](rinit-开发计划.md) for the full development plan; this
README tracks implementation status. A Chinese translation of this file lives at
[`README.zh-CN.md`](README.zh-CN.md); this English version is the authoritative one.

## Status: M0–M5 implemented — verified on aarch64

Every milestone's scope and every item of the plan's extension list (§7.1–§7.11)
is implemented and proven: the [Acceptance](#acceptance-plan-6s-checklist) section
answers plan §6's checklist line by line, with one exception - the 72 h soak, which
is a deployment-time run of `make soak` rather than a feature.

What is in place, and what it is proven by. Every claim below is either a host
test (`make check`) or a guest assertion (`make test-system`, 97 of them in
`scripts/system-test.sh`); "proven on the host" always means the fake-runtime or
`--dev` form of the same code path, and the guest means the real one.

| milestone | content | evidence |
| --------- | ------- | -------- |
| M0 | PID 1 via `rdinit=`, first-wave mounts (`/run` → `/proc` → `/sys` → `/dev` → `/tmp` → `/sys/fs/cgroup`) with fatal/degradable rules per plan §1.3(9), the second wave (`/etc/fstab`, plan §1.3(9)) in file order with failures logged and the boot continuing, `SIGCHLD` + `waitpid(-1, WNOHANG)` reaping, panic hook to `/dev/kmsg` | guest: "mounted /run…", "mounted /sys/fs/cgroup", "mounted /data (fstab)", "fstab-tmpfs /data tmpfs", "no rinit panic"; host: `crates/rinit-runtime/src/fstab.rs` (6), `crates/rinit/tests/smoke.rs` |
| M1 | unit files parsed from `/etc/rinit/units`, dependency graph resolved, units started level by level with each level in parallel | guest: "resolved a boot plan", "startup levels run in dependency order", "no target was exec'd" |
| M2 | supervision (spawn, track, reap, two-stage stop), restart policy with backoff and a circuit breaker, health probes, `Type=notify` readiness, the in-memory log ring and its optional file mirror (plan §1.3(7)) | guest: "a crashed service is scheduled for restart", "the breaker tripped on the failing unit"; host: `crates/rinit/tests/m2_lifecycle.rs` (43 tests) |
| M3 | control socket + `rctl` (`list`, `status`, `start`, `stop`, `restart`, `reset-failed`, `log`, `reload`), with a documented exit-code contract | guest: "rctl list reached the control socket" and the five assertions after it; host: `crates/rctl/tests/control.rs` (9 tests) |
| M4 | soak harness for sustained churn (`make soak`), leak and descriptor fixes, `--dev` control-socket peer relaxation | `scripts/soak.sh` (see [Testing](#testing)); `crates/rinit-core/tests/allocations.rs` |
| M5 | **§7.3–§7.11 done.** cgroup v2: `CgroupCPUMax=`, `CgroupMemoryMax=`, `CgroupMemoryHigh=`, `CgroupTasksMax=` under `rinit/<unit>`, the process placing itself, stop-flow signalling of the cgroup's members, `rmdir` cleanup, and OOM-kill detection into the restart policy. Sandboxing: `User=`, `Group=`, `NoNewPrivileges=`, `CapabilityBoundingSet=`, `AmbientCapabilities=`. Hardware watchdog: `/dev/watchdog` armed at boot, fed from the supervisor loop's health checkpoint, magic-closed on shutdown. Reload: `rctl reload` re-reads the unit directory, adds, changes, defers and removes, and refuses a directory that does not resolve. Legacy daemons: `Type=forking` with `PIDFile=`, located through the file or the unit's own cgroup. Terminals: `TTYPath=`/`TTYReset=` give a service a controlling terminal (plan §7.11). Templates: `worker@.service` with `%i`/`%n`/`%p` expanded at instantiation (plan §7.7). Sockets as dependencies: `Sockets=`/`NeedsSocket=`, resolved to a hard edge and a wait for the path (plan §7.8). Socket activation: `SocketActivate=`/`IdleTimeoutSec=`, rinit binding the socket and the connection starting the service (plan §7.9). With that, **every item of the plan's extension list (§7.1–§7.11) is implemented** | guest: "a limited unit was placed in its own cgroup", "the memory hard and soft limits were written in bytes", "a stopped unit's cgroup was removed", "the kernel OOM-killed a process inside the unit's cgroup", "rinit armed the guest's watchdog device", "the driver took the timeout rinit set", "the guest outlived the watchdog timeout with the device armed", "an added unit the boot target does not need was not started", "a changed unit that is running kept it for the next start", "a directory that does not resolve was refused whole", "the unit is active with the daemon the PIDFile named", "stopping a forking unit stopped the daemon it took over", "a TTYPath that is not a terminal fails the start", "a template is not a unit to start", "every specifier was expanded before the command ran", "a socket consumer waited for the path before starting", "a socket that never appears fails the start, naming it", "rinit bound the socket before any unit started", "starting it hands the service the listening socket", "a de-privileged service ran as nobody:nogroup with NoNewPrivs", "an empty CapabilityBoundingSet left a root service with no capabilities", "ambient capabilities reached a de-privileged service"; host: `crates/rinit/src/cgroup.rs` (19 tests), `crates/rinit-core/src/unit/cgroup.rs` (9), `crates/rinit-runtime/src/syscall/cgroup.rs` (14), `crates/rinit/src/watchdog.rs` (6), `crates/rinit-runtime/src/syscall/watchdog.rs` (6), `crates/rinit/src/unit_manager.rs` (5 for the reload itself, `reload.rs` is policy over them), `crates/rinit/tests/m2_lifecycle.rs` (11, end to end), `crates/rinit/src/sandbox.rs` (14), `crates/rinit-core/src/unit/sandbox.rs` (9), `crates/rinit-runtime/src/syscall/cred.rs` (14) |

A real boot of the committed tree on the serial console (`…` elides kernel
noise; the lines below are the ones `make test-system` asserts on, except that the
system test boots the acceptance image and so counts the `units/demo/` units in
its plan line too). This is `make run`, and the boot target's closure is the
console and the DHCP client:

```
[    2.344175] Run /sbin/rinit as init process
rinit: rinit 0.1.0 starting (pid 1)
rinit: mounted /run
rinit: mounted /proc
rinit: mounted /sys
rinit: mounted /dev
rinit: mounted /tmp
rinit: mounted /sys/fs/cgroup
rinit: mounted /data (fstab)
rinit: cgroup v2 ready at /sys/fs/cgroup
rinit: logging to /var/log/rinit/log
rinit: boot plan for 'multi-user.target': 5 unit(s) in 3 level(s)
rinit: plan 0: rinit-init.target (target)
rinit: plan 1: getty.service (service)
rinit: plan 1: network.service (service)
rinit: plan 2: multi-user.target (target)
rinit: starting level 0: 0 service(s)
rinit: level 0 reached: rinit-init.target active
rinit: starting level 1: 2 service(s)
rinit: started getty.service as pid 48
rinit: started network.service as pid 49
rinit: starting level 2: 0 service(s)
rinit: level 2 reached: multi-user.target active
rinit: boot complete: 2 unit(s) started from 'multi-user.target' (2 still running)
rinit: boot timing: first service at 5190ms of kernel uptime (rinit's own boot: 505ms)
rinit: watchdog /dev/watchdog armed: 30s timeout, feeding every 15s

(none) login:
```
Two lines in that boot are measurements rather than steps. `mounted /data (fstab)`
is plan §1.3(9)'s second wave - an `/etc/fstab` entry, mounted after the first
wave - and `boot timing: first service at 5190ms of kernel uptime (rinit's own
boot: 505ms)` is plan §4.3's number, on the kernel's own clock so it covers the
kernel's boot as well. Both figures come from a run whose *host* was also running
the 72 h soak, and they move from boot to boot (an earlier one under the same load
read 2590 ms and 239 ms); the guest asserts the measurement is the kernel's clock,
not that it hit a number.

Note that the plan resolves **5 units** and starts **2**: `units/` is the boot set
- the two targets, the console and its alternative, and the DHCP client - and only
the boot target's closure is started. `shell.service` is parsed and ordered but
not activated, because it would fight `getty.service` for `ttyAMA0`.
`network.service` is in the closure: `multi-user.target` requires it, and qemu's
user-mode DHCP gives the guest `10.0.2.15/24`, gateway `10.0.2.2` and DNS
`10.0.2.3`. The §7.x acceptance units
live in `units/demo/`, which `make test-system` packs and `make run` does not:
`priv`/`caps`/`ambient` (§7.4), `cglimit`/`cgrun`/`oom` (§7.3), `forking` (§7.10),
`logdemo`, `tty`, `sockd`, `sockc`, `api` and the `worker@` template are outside
the closure on purpose and are started from the console with `rctl`. M2's
`fault.service` is one of them, so the breaker it demonstrates appears in the
system test's log rather than in this one. The watchdog's `30s` is what `make run`
boots with; `make test-system` shortens it to `10s` (see the CLI section below) so
that the guest can outlive it and prove the device is being fed.

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
built from a variable (`v=populated; ... echo cgrun-$v` in the system test,
`$d/identity` here) rather than appearing literally in the input - an assertion that its own
echo satisfies proves nothing.

§7.6's reload, which is the one feature whose evidence *has* to come from the
guest: the initramfs unpacks the unit directory into a ramfs (the boot set, plus
`units/demo/` in the acceptance image), so the console can add,
change and remove unit files under a running rinit. The four reloads below are
"nothing changed", "one added", "one stopped unit changed and one running unit
changed", and "one removed":

```
~ # rctl reload
rinit: reload: 0 added (0 started), 0 changed (0 deferred), 0 removed (0 stopped), 17 unchanged
~ # echo "[Service]" > /etc/rinit/units/reload.service
~ # echo "Type=oneshot" >> /etc/rinit/units/reload.service
~ # echo "ExecStart=/bin/echo reloaded-service" >> /etc/rinit/units/reload.service
~ # rctl reload
rinit: reload: registered reload.service
rinit: reload: 1 added (0 started), 0 changed (0 deferred), 0 removed (0 stopped), 17 unchanged
~ # echo "ExecStartPre=/bin/echo reloaded-pre" >> /etc/rinit/units/logdemo.service
~ # echo "ExecStartPost=/bin/true" >> /etc/rinit/units/getty.service
~ # rctl reload
rinit: reload: logdemo.service: new definition in force
rinit: reload: getty.service is running; its new definition applies at the next start
rinit: reload: 0 added (0 started), 2 changed (1 deferred), 0 removed (0 stopped), 16 unchanged
~ # rm /etc/rinit/units/reload.service
~ # rctl reload
rinit: reload: removed reload.service
rinit: reload: 0 added (0 started), 1 changed (1 deferred), 1 removed (0 stopped), 16 unchanged
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

§7.8's socket dependencies and §7.9's activation, from the same console. The first
is a *wait* around a start order - the provider is `Type=simple`, so it is `Active`
long before it creates its socket - and the second is a socket rinit holds for a
unit that is deliberately not running:

```
~ # rctl start sockd.service          # creates /run/sockd.sock a second after exec
rinit: started sockd.service as pid 142
~ # rctl start sockc.service          # NeedsSocket=: blocks until the path is there
rinit: started sockc.service as pid 145
~ # rctl log sockc.service
sockc.service: socket-present
~ # test -S /run/api.sock && echo api-socket-bound
api-socket-bound
~ # s=$(rctl status api.service | grep state); echo api-state=$s
api-state=state: inactive
~ # rctl start api.service            # the socket rinit bound, handed over on fd 3
~ # rctl log api.service
api.service: activated-fd-3
```

`socket-present` is the wait: the consumer ran `test -f` on the path and found it,
which it could only do after waiting for a provider that had not created it yet.
`api-state=state: inactive` is the whole point of activation - the unit is down and
the socket exists - and `activated-fd-3` is the contract reaching the service:
`test -S /proc/self/fd/3` is true only for a descriptor that is a connected socket,
so the service was handed the *listening* one. The guest cannot connect (busybox
`nc` has no Unix-socket support), so the connection-driven start and the idle stop
are the m2 tests.

§7.7's template unit, which is the one case where a *name* is the feature. The
file is `worker@.service`; the instance does not exist until someone asks for it,
and the expansion happens before the parser sees the command:

```
~ # rctl start worker@.service
rctl: worker@.service is a template; start an instance, e.g. worker@<name>.service
~ # rctl start worker@42.service
rinit: started worker@42.service as pid 137
~ # rctl log worker@42.service
worker@42.service: worker-instance=42 name=worker@42.service prefix=worker env=42
```

`%i`, `%n` and `%p` are the instance, the full name and the prefix; `env=42` is
the same expansion inside an `Environment=` line, which is why the demo prints
both - `$INSTANCE` in the command is the *shell* reading the environment, a
different mechanism from the substitution. The template itself is listed as what
it is (`worker@.service: template`) and refuses to start, and an instance is a unit
like any other from that point on.

## Layout

```
crates/
  rinit-core/         # platform-independent engine (no_std + alloc)
    src/unit/         # parser.rs, dependency.rs, config.rs, job.rs, restart.rs,
                      # notify.rs, sandbox.rs (the §7.4 drop *plan*), cgroup.rs
                      # (the §7.3 limit model), state/
    src/              # control.rs (protocol), logger.rs, users.rs, error.rs,
                      # ratelimit.rs (console output, per call site)
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
units/                # the boot set: the two targets, getty.service (the console
                      # login `make run` boots), network.service (DHCP) and
                      # shell.service, its alternative
units/demo/           # the §7.x acceptance units; packed only into the image
                      # `make test-system` boots, and started from the console
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

Five layers, cheapest first:

```bash
make check          # fmt + clippy -D warnings + host tests  (no cross toolchain)
make soak           # 60s of sustained churn; watches RSS/fds/zombies
make test-system    # boot aarch64 qemu and assert the acceptance list
make test           # check + test-system
make gates          # acceptance gates: binary size + coverage thresholds
```

`cargo test --offline --workspace` (what `make check` runs last) executes **735
tests** across ten test binaries. The five shapes, and why each is not
redundant:

- **unit tests** next to the code (core 329, rinit 197, rinit-runtime 110,
  rctl 14), including `proptest` properties over the parser;
- **integration tests** in `crates/*/tests/`: the syscall layer against the real
  kernel (`rinit-runtime` 22: run unprivileged, they assert that each drop is
  *refused* rather than silently skipped - `EPERM` from `setuid(0)`, `setgroups`,
  `PR_CAPBSET_DROP`, `capset` and the ambient raise, each naming the call that
  failed - with the root-only success paths deferred to the guest), the
  lifecycle end to end in `--dev` mode (`rinit/tests/m2_lifecycle.rs` 43), the
  control CLI (`rctl/tests/control.rs` 9), the smoke test that runs the real
  binary (`rinit/tests/smoke.rs` 2), the allocation guard
  (`rinit-core/tests/allocations.rs` 2), and `shipped_units.rs` 7 - every unit in
  `units/` and `units/demo/` parses through the loader, so a unit that would fail
  at boot fails here first, and the two directories are checked not to shadow each
  other's names when the packing script flattens them;
- **the soak harness** (`scripts/soak.sh`, plan §3 M4/§4.4): runs rinit under
  churn, samples RSS/descriptors/threads/zombies, drives the control socket in a
  loop, and fails if the steady state is not flat or if `SIGTERM` does not exit 0.
  The default 60 s is a commit-time check; `make soak SOAK_ARGS='--seconds
  259200'` is the 72-hour acceptance run the plan asks for. rinit's console is the
  one unbounded thing in a run - the churn unit is deliberately unthrottled, so
  rinit logs every restart - and it goes through a reader that keeps only the
  counts the assertions need and a 2000-line tail, which is why a 72-hour run
  leaves ~100 KB behind instead of filling the disk. rinit bounds that console
  itself as well, per call site rather than per message text - a restart loop
  prints the same handful of messages with a different pid in each one, so a
  text-keyed limiter would fold nothing (`rinit_core::ratelimit`);
- **the acceptance gates** (`make gates`): plan §0.1's size budget and plan §6's
  coverage thresholds, measured rather than quoted, which is where the numbers in
  [Acceptance](#acceptance-plan-6s-checklist) and
  [Measurements](#measurements-plan-01-calibration-point) come from. They are
  deliberately *not* part of `make check`: `size-check` needs the release build
  and `coverage-check` takes minutes, while `make check` is the per-commit gate.
  `scripts/coverage-check.sh` prints the counts it measured as well as the
  verdict, and both thresholds are arguments so the gate's own arithmetic can be
  tested;
- **the guest system test** (`scripts/system-test.sh`, plan §4.3): boots the real
  initramfs on aarch64 qemu, types at the console to prove a root shell is
  reachable, drives `rctl` against the running system, and asserts 97 things -
  including that levels run in dependency order, that no target is ever exec'd,
  the §7.4 privilege-drop outcomes, the §7.3 limits read back out of the guest's
  own kernel, rinit's own RSS at boot and under 20 services read out of
  `/proc/1/status`, §4.3's boot timing read off the kernel's clock, §7.5's
  watchdog on the real device qemu attaches, the DHCP client's address, route and
  resolver, §7.6's reload
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

`make coverage-check` is where these come from - `cargo llvm-cov --offline
--workspace --summary-only` with plan §6's thresholds applied to the result - so
they are measured on the tree as it stands rather than copied from one run:

| crate            | line coverage | functions |
| ---------------- | ------------- | --------- |
| `rinit-core`     | **97.76%**    | 576/587   |
| `rinit-runtime`  | 84.93%        | 294/339   |
| `rinit`          | 91.13%        | 619/665   |
| `rctl`           | 95.56%        | 32/38     |
| **total**        | **92.34%**    | 1521/1629 |

The plan's checklist asks for ≥80% overall and ≥90% in the core, so these are the
numbers that answer it rather than a shape:

The lowest files are mostly ones that *cannot* run on a host, which is why the
guest tests exist rather than an omission: `syscall/reboot.rs` (0% - it would
reboot the machine), `syscall/mount.rs` (46.84% - needs `CAP_SYS_ADMIN`),
`syscall/process.rs` (56.80% - the child-side drop paths, which a host run
refuses by design), `src/main.rs` (46.67% - the entry point) and
`src/control.rs` (70.72% - the socket server, which the guest drives and the host
reaches only through `RINIT_CONTROL_DIR`). The one host-runnable file in that
range is `rinit-core/src/error.rs` (52.94%), whose uncovered lines are the
`From` impls and `Display` arms for `ParseError` and `TransitionError`.

Two kinds of line are uncovered, and they are worth telling apart: **guest-only**
lines (the control server, the child-side sandbox steps, `mount(2)`), which the
system test exercises on a real kernel but `cargo llvm-cov` cannot see because it
only runs the host suite, and **genuinely unreached** ones, which is what this
number is for. The second kind is what the audit that followed this measurement
went after: it deleted `rinit-testservice` (M0's bring-up helper, 0% because
nothing had run it for months - `--test-service` and the real unit path replaced
it), and it added the two tests the same listing pointed at, one for `SIGINT`
(handled, and the only one of the four shutdown signals with no test) and one for
every `ParseError` message.

```bash
cargo llvm-cov --offline --workspace --summary-only          # the table above
cargo llvm-cov --offline --workspace --html                  # target/llvm-cov/html
```

**Known gaps**, tracked rather than hidden:

- **The log *file* is a mirror, not a journal.** Plan §1.3(7)'s optional
  persistence is implemented (`--log-file`, `/var/log/rinit/log` by default,
  truncated at 1 MiB), but it is a copy of the ring's latest lines written with
  plain `write_all` - no per-unit files, no `journalctl` query over the file, and
  no syslog protocol. `rctl log` reads the ring, so a line is only in the file for
  as long as the file is; whether that outlives a reboot is the filesystem's
  business (tmpfs does not, a disk does).
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
- **A unit file cannot name a template instance** (plan §7.7): `Requires=worker@1.service`
  is rejected when the directory is loaded, because the dependency graph is built
  from the files and an instance is not one of them. Instances are started by name,
  which is also what creates them; systemd's `[Install]`/`WantedBy=` symlinks are
  what rinit has no equivalent of.
- `signals.rs`'s pipe-full path (the handler drops a byte rather than blocking)
  is not forced in a test. The descriptor-leak tests do not depend on fd numbers
  being recycled: they record the pipe's inode while the set is alive and check
  that it is closed afterwards, so a test running beside them cannot make them
  flaky.
- **`miri`, `valgrind` and `cargo-fuzz` cannot run in this environment**, and what
  stands in for each is worth naming rather than implying nothing checks:
  `miri` needs a *nightly* toolchain (only stable is installed, `cargo miri` says
  so, and the environment is offline), so what it would look for in `rinit-core` -
  leaks and UB - is covered directly instead: `crates/rinit-core/tests/allocations.rs`
  drives every allocation-owning path through a counting global allocator and
  asserts the live bytes do not grow between two identical batches (10,000
  iterations turn one leaked byte into 10 KB, so per-iteration leaks show above the
  harness's own noise), and the crate is `#![forbid(unsafe_code)]`, so there is no
  unsafe code in it for `miri` to find UB in. `cargo-fuzz`'s decoders are
  property-tested (`proptest` over the parser, the control codec and the job
  queue), and the runtime-side equivalent of `valgrind` is the soak harness's
  sampled RSS/descriptor/zombie curve.

## Acceptance: plan §6's checklist

Plan §6 is the definition of done. Every line of it is either a command in this
repository or a measurement in this README:

| plan §6 | where it stands |
| ------- | --------------- |
| boots as PID 1 in qemu (128M) with no kernel panic | `make test-system`: 97 assertions, including "no rinit panic" and four kernel-log checks |
| 20 services in parallel, visibly faster than serial | **19.2x**: `make parallel` - 20 `oneshot` services that each sleep 1 s finish in 1065 ms together and 20 424 ms chained |
| crash-restart, breaker, health checks | `make test-system` ("a crashed service is scheduled for restart", "the breaker tripped on the failing unit") and `crates/rinit/tests/m2_lifecycle.rs` |
| `rctl` can start, stop, query and read logs | `make test-system`'s M3 block and `crates/rctl/tests/control.rs` |
| graceful shutdown, no zombies, no fd leak | `make soak` samples descriptors, threads and zombie children through restart churn, and asserts each comes back to its baseline |
| 72 h soak with stable RSS | **running now**, seventh segment, started 2026-10-21 17:41 with its work directory under `build/` rather than `/tmp`, so the window ends 2026-10-24 17:41 and the numbers land in this row then; this run writes its log to `build/soak-charts/soak-72h-seg7.log`, so unlike the runs whose log lived only in `/tmp` it leaves timestamps behind even if the machine goes down. The sixth segment was stopped by hand after 1 h 50 m (109 samples), not by an assertion, once the harness itself was fixed: at `--interval 1` a sample cost ~70 ms, the loop slept a full interval *after* the work, and the default 60 s gate therefore collected 56 of 60 samples and failed its own coverage check (21 before the zombie scan became a single awk pass); the loop now sleeps until the next absolute slot and `make soak` collects 60/60. Earlier segments were all ended by something other than an assertion: **8.92 hours** (535 samples, 18 519 600 restarts) with RSS 5116 → 5116 kB peak per half, descriptors 14 → 14 at their lowest and zero zombies; **44.6 hours** (2587 samples) whose RSS moved between 3892 and 3960 kB in ~16 kB plateaus that went up *and* down - the second-half slope was −1.28 kB/h - with descriptors 13-14 at their lowest in every hour and zero zombies, ended by a host reboot that took the curves with it because the work directory was on tmpfs; and **43 minutes**, stopped by the harness's own free-space floor when something else on the machine filled the shared 978 MB `/tmp` - which is why the run now keeps its ~100 KB work directory on the repository's filesystem instead; and **13.6 hours** (796 samples, 2026-10-20 06:09 → 19:46), ended by the reboot that this time did not take the curve with it, because the work directory had already moved: RSS stepped down twice after boot, hourly means 5068-5087 kB for the first three hours, 4381-4443 kB for the next five and 4202-4252 kB for the last six, with last sample 4296 kB, descriptors 14-28, one thread and zero zombies. Four things changed because of these runs: the descriptor check compares the *lowest* count in each half rather than the peak (a peak is a coin flip once a run is long enough - the 8.92-hour segment's peaks read 25 → 24 while its lows were 14 → 14), rinit rate limits its own console, which is the sink that filled the disk a run died on, and the restart count the harness prints is a rate-limited lower bound - the line it counts is capped at 40 per 5 s per call site, so `churn: N restarts` reads ~8/s while the real churn, measured independently as the host's `fork` rate, is ~545/s; and the sampler sleeps until its next absolute slot, because sleeping a full `interval` after each sample made a 60 s run collect 56 of 60 samples and fail a coverage check it had already met |
| binaries ≤2 MB dynamic / ≤4 MB static | [Measurements](#measurements-plan-01-calibration-point): 1.57 MiB static aarch64, checked by `make size-check` |
| unit test coverage ≥80% overall, ≥90% in the core | **92.34%** and **97.76%** (`make coverage-check`, [above](#testing)) |
| documentation: architecture, unit format, `rctl`, porting | this README (architecture, status, measurements), [`docs/units.md`](docs/units.md) (every directive, its value syntax and its default), [`docs/porting.md`](docs/porting.md) (kernel, userspace, build, differences from systemd), and `rinit-开发计划.md` |

One number the checklist does not ask for, but plan §0.1's design goals do: rinit's
RSS, measured *in the guest* out of `/proc/1/status` rather than on the host -
**1 632 kB idle** and **1 872 kB with 20 services running**, against §0.1's 2 MB
and 6 MB. The growth is the interesting part: twenty more units cost under 200 kB,
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
4. Assemble `build/out/initramfs-<target>.cpio.gz` (busybox + rinit + rctl +
   `units/`, plus the `/etc` and `/var` skeleton the login shell expects). The
   acceptance image `build/out/initramfs-<target>-demo.cpio.gz` is
   the same root plus `units/demo/`, and is what `make test-system` boots.
5. Boot it in `qemu-system-aarch64 -machine virt -cpu cortex-a57 -m 128M` with
   `-device i6300esb`, the watchdog rinit arms (plan §7.5), and qemu's user-mode
   network (`-netdev user` plus `virtio-net-device`), which is what the DHCP
   client runs on.

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
make initramfs-demo # the same, plus units/demo/ (what the system test boots)
make qemu           # alias for `make run`
make check          # fmt + clippy + host tests (no cross toolchain)
make soak           # churn + memory/fd/zombie regression check (host, --dev)
make test           # check + the qemu system test
make MIRROR=https://... run   # different source mirror
make KERNEL_VERSION=6.6.158 BUSYBOX_VERSION=1.36.1 ...   # pin versions
```

The run image is rebuilt whenever `rinit`, `rctl`, `busybox`, a file in `units/`,
or one of the packing scripts changes — editing `units/getty.service` repacks it,
because the unit directory is what the guest boots. The acceptance image also
depends on `units/demo/`, and only on it in addition: editing a demo unit repacks
that image and leaves the run image alone.

### Getting a shell in the guest

`make run` boots `units/getty.service`, which puts a login prompt on the serial
console. Type `root` and press enter — the password field in `/etc/passwd` is
empty, so there is nothing to type after that. The transcript above is that
prompt. After login `/etc/motd` prints the commands that matter (`rctl list`,
`rctl status getty.service`, `rctl log getty.service`, `poweroff -f`), and
`/etc/profile` sets `PATH` and applies `/etc/hostname`. `network.service` runs
the DHCP client: qemu's user-mode network gives the guest `10.0.2.15/24`,
gateway `10.0.2.2` and DNS `10.0.2.3`, so the interface is configured without a
manual step.

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
- the initramfs is repacked when rinit, busybox, a file in `units/` (or, for the
  acceptance image, `units/demo/`), or one of the packing scripts changes.

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
      --log-file <PATH>      Mirror captured service output to this file
                             (default: /var/log/rinit/log; 'none' disables)
      --fstab <PATH>         Second-wave mount table (plan §1.3(9))
                             (default: /etc/fstab; 'none' disables)
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

rinit rate limits its **own** console output, per call site: 40 lines per 5 s,
after which it counts what it dropped and reports it as `N message(s) suppressed
at <file>:<line> (rate limit)` - at the next window, or on the shutdown path if
the flood stops first. A service that crash-loops restarts hundreds of times a
second and prints the same messages with different pids in them, so the limiter
is keyed by call site rather than by text; the largest legitimate burst (a boot
level starting its 20 units, one call site) is well inside the budget. Service
output is unaffected - the ring and the file mirror already bound that, per unit
and by size.

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
| `aarch64-unknown-linux-gnu` (static) | `rinit`             | 1.57 MiB (1 649 744 B)  | yes         |
| `x86_64-unknown-linux-musl` (static) | `rinit`             | 1.05 MiB (1 098 688 B)  | yes         |

`rctl` is 994 152 B on aarch64 and 406 200 B on musl; both are inside the
initramfs. `make size-check` is the gate, and it checks the two aarch64 binaries
against plan §0.1's 4 MiB static budget. The musl figures are a manual build
(`cargo build --release --target x86_64-unknown-linux-musl`), which is how that
target came to be broken for a while without anyone noticing: `ioctl`'s request
parameter is `unsigned long` in glibc and `int` in musl, so four call sites did
not compile there and this table's musl row was older than the breakage. A second
target in the gate would need a toolchain the build host may not have.

M0's bring-up helper `rinit-testservice` is gone: nothing ran it any more and it
was not shipped, so the §7.9 audit deleted it rather than keep a megabyte of dead
binary in every build. A repeat of that step is `--test-service` now, which runs a
command through the same spawn path a unit uses.

The packed images (`build/out/initramfs-aarch64-unknown-linux-gnu.cpio.gz` and
`…-demo.cpio.gz`: static busybox + rinit + rctl + the unit directory + the
`/etc`/`/var` skeleton) are 2.30 MiB (2 415 543 B) and 2.31 MiB (2 422 373 B)
gzipped. The difference is the fourteen files in `units/demo/`, which only the
second one carries. The packing is
reproducible - `scripts/mkinitramfs.sh` zeroes the mtimes and inode numbers the
archive would otherwise record, so repacking the same binaries gives the same
bytes, which is what makes the number checkable rather than merely re-measured. A
*forced* rebuild of busybox (`make -B`) is the exception: busybox's own build is
not byte-reproducible, so the image moves by a few bytes with it.

Both binaries are inside the plan §0.1 budget: the target is ≤2 MB per
executable, with static builds allowed up to 4 MB.

Plan §0.1's memory column is measured in the guest, out of `/proc/1/status`, by
`make test-system`: **1 632 kB at boot** and **1 872 kB with 20 services**, on the
128M machine, asserted at ≤2 MB and ≤6 MB. The host process the soak watches
reads higher - about 5 MB - because it is an unoptimised `--dev` build of the
same code.

## Next

The plan's extension list is complete - §7.7's templates, §7.8's socket
dependencies, §7.9's activation and §7.11's terminal directives were the last four
- and plan §6's checklist is answered item by item in the
[Acceptance](#acceptance-plan-6s-checklist) section.

`rinit-开发计划.md` is the frozen specification and is deliberately not edited:
its checkboxes stay as they were written, and this README is the only status
source. One consequence is worth knowing before reading the plan: its §2.2 scope
table still marks socket activation and template units as ❌ "not doing", which
was the plan's original boundary - both are in the extension list (§7.9, §7.7)
and both are implemented. The table records where the project started, not where
it ended; this README records the latter.

What is left is not a feature: the 72 h soak (a deployment-time run of
`make soak`), and the coverage and size numbers re-measured as the tree changes
rather than once - which `make gates` now does.
