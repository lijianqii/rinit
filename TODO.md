# rinit — TODO

## 🔴 Critical

- [x] **Implement `shutdown()` and `emergency_shutdown()`** (`init-event/src/lib.rs:230-238`)
  - Send SIGTERM to all child processes
  - Wait for children to exit (with configurable timeout)
  - Send SIGKILL to any remaining children
  - Call `sync()` to flush filesystem buffers
  - Remount filesystems as read-only
  - Call `reboot(RB_POWER_OFF)` / `reboot(RB_AUTOBOOT)` / `reboot(RB_HALT_SYSTEM)`

- [x] **Set `PR_SET_CHILD_SUBREAPER`** so PID 1 reaps orphaned grandchildren
  - Without this, orphaned grandchildren become zombies if their immediate parent dies

- [x] **Open `/dev/console` as fd 0, 1, 2** early in bootstrap
  - PID 1 should own stdin/stdout/stderr on the console device
  - Prevents kernel messages from being lost

- [x] **Redirect child stdin/stdout/stderr** in `spawn_service()`
  - Redirect to `/dev/null` (or future journal socket) to prevent terminal hijacking

- [x] **Add restart rate limiting** (`StartLimitBurst` / `StartLimitInterval`)
  - Prevent tight restart loops when a service crashes immediately with `restart=always` and `restart_sec=0`

---

## 🟡 Important — parsed but not wired up

- [ ] **Apply cgroup isolation** in `start_service_unit()`
  - Call `create_service_cgroup()` + `attach_process()` when starting a service
  - Remove cgroup on service stop

- [ ] **Apply resource limits** from unit config
  - `memory_max` → write to `memory.max`
  - `cpu_weight` → write to `cpu.weight`
  - `tasks_max` → write to `pids.max`

- [ ] **Pass environment variables** to child process
  - `ServiceSection.environment` is parsed but `spawn_service()` doesn't accept env
  - `start_service_unit()` doesn't pass it through

- [ ] **Apply `working_directory`** in child process before exec
  - `chdir()` to the configured directory in the child after fork

- [ ] **Apply `oom_score_adj`** by writing to `/proc/<pid>/oom_score_adj` after spawn

- [ ] **Execute `exec_stop`** during service shutdown
  - Run the stop command before killing the main process

- [ ] **Execute `exec_reload`** on SIGHUP
  - Currently only reloads unit config files; should also run the reload command

- [ ] **Implement `forking` service type**
  - Wait for initial child to exit
  - Read PIDFile to track the actual daemonized process

- [ ] **Implement `oneshot` service type**
  - Wait for the process to exit before starting dependents
  - Do not restart on exit

- [ ] **Implement `notify` service type**
  - Set up `NOTIFY_SOCKET` environment variable
  - Listen on a Unix datagram socket for `sd_notify` READY=1 messages

- [ ] **Wire up `After` / `Before` ordering** in dependency resolver
  - `deps.rs` DAG build only uses `Requires`/`Wants`
  - `After`/`Before` edges should be added to the topological sort

- [ ] **Wire up `Conflicts` detection** in the event loop
  - `detect_conflicts()` exists but is never called
  - Should stop conflicting units before starting a new one

- [ ] **Evaluate `ConditionPathExists`** before starting a service
  - Skip unit activation if the required path does not exist

- [ ] **Implement `KernelOps` trait** (defined in `init-core/src/lib.rs:52-88`)
  - Trait is defined but has no `impl` — all calls go directly to free functions
  - Implement a real struct that delegates to the existing free functions

- [ ] **Implement `Capabilities` support** (`init-core/src/lib.rs:45-48`)
  - Type and trait method exist; no implementation for bounding/ambient caps before exec

---

## 🔵 Lower priority

- [ ] **Parse `/proc/cmdline`** for kernel boot parameters
  - `init=`, `quiet`, `single`, `rw`, `root=`, etc.

- [ ] **Remount root filesystem as read-write** during bootstrap
  - Kernel boots with `/` mounted read-only; PID 1 must remount it rw

- [ ] **Handle Ctrl-Alt-Del** reboot sequence

- [ ] **Apply sysctl kernel parameters** (read `/etc/sysctl.conf` or equivalent)

- [ ] **Load kernel modules** (modprobe equivalent during early boot)

- [ ] **Read `/dev/kmsg`** for kernel log buffer access

- [ ] **Bring up loopback interface** (`lo`) and basic network setup

- [ ] **Implement tmpfiles-like functionality** — create temp dirs, set permissions at boot

- [ ] **Implement `.socket` units** — socket activation with fd passing

- [ ] **Implement `.timer` units** — time-based activation

- [ ] **Implement `.mount` units** — filesystem mount management
