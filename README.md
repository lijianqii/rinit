# rinit — A Rust init system (PID 1)

`rinit` is a **systemd-inspired init system** written in Rust. It runs as **PID 1** on Linux, managing service lifecycles, supervising processes, enforcing resource limits via cgroups v2, and handling system shutdown — all from a single-threaded, event-driven async runtime.

> **Status**: early-stage framework. Core architecture is in place; service supervision and socket activation are under active development.

---

## Architecture

```
Kernel Interface  (init-core:    signalfd, cgroups, fork+exec, mount, capabilities)
       |
Event Loop        (init-event:   tokio single-threaded, select! multiplexing)
       |
Core Managers     (init-unit:    .service, .socket, .timer, .mount, .target)
   ├── Unit Manager        — parse TOML unit files, maintain registry
   ├── Dependency Solver   — Kahn topological sort, cycle detection, conflict resolution
   └── Process Supervisor  — fork+exec+supervise, restart policies, cgroup isolation
       |
Subsystems       (future:  socket activation, D-Bus IPC, journal, udev, CLI client)
```

### Crate map

| Crate | Purpose | Key deps |
|:---|:---|:---|
| `init-core` | Safe Linux kernel abstractions (signalfd, cgroups v2, fork+exec, mount) | `nix`, `libc` |
| `init-event` | Single-threaded async event loop driving all supervision | `tokio`, `init-core` |
| `init-unit` | Unit file parsing (TOML), dependency resolution (Kahn DAG) | `serde`, `toml` |
| `rinit` (binary) | PID 1 entry point: bootstrap → load units → enter event loop | all above |

---

## Project structure

```
rinit/
├── Cargo.toml                    # Workspace root
├── README.md
├── .cargo/
│   └── config.toml               # Cross-compilation linkers (aarch64, x86_64 musl)
├── src/
│   ├── main.rs                   # PID 1 entry point
│   └── bootstrap.rs              # Early init (mount /proc, /sys, /dev, block signals)
├── crates/
│   ├── init-core/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs            # Public API re-exports
│   │       ├── child.rs          # fork(2) + execve(2) + waitpid(WNOHANG)
│   │       ├── cgroup.rs         # cgroups v2 unified hierarchy
│   │       ├── fs.rs             # mount(2): /proc, /sys, /dev, /run
│   │       └── signal.rs         # signalfd: block, create, read pending signals
│   ├── init-event/
│   │   ├── Cargo.toml
│   │   └── src/lib.rs            # tokio select! event loop (signal + child reaping)
│   └── init-unit/
│       ├── Cargo.toml
│       └── src/
│           ├── lib.rs            # Unit registry, file loading
│           ├── types.rs          # Unit, ServiceSection, RestartPolicy, etc.
│           ├── parse.rs          # TOML deserialisation (serde)
│           └── deps.rs           # Kahn topological sort + cycle detection
├── config/
│   ├── default.target.toml       # Example: default boot target
│   └── test.service.toml         # Example: a simple echo test service
└── scripts/
    ├── build-static.sh           # Static musl build (x86_64)
    ├── mkinitramfs.sh            # Build minimal initramfs with rinit + busybox
    ├── qemu-run.sh               # Boot rinit in QEMU (x86_64, uses host kernel)
    └── aarch64-qemu.sh           # Cross-build + QEMU boot for aarch64 (ARM64)
```

---

## Quick start

### 1. Install dependencies (Debian / Ubuntu)

```bash
sudo apt install -y qemu-system-x86 musl-tools
```

### 2. Build a static binary

```bash
./scripts/build-static.sh
```

This compiles `rinit` against `x86_64-unknown-linux-musl`, producing a fully static binary at `target/x86_64-unknown-linux-musl/release/rinit`.

### 3. Create a minimal initramfs

```bash
./scripts/mkinitramfs.sh
```

Packages `rinit` as `/init` plus `busybox` for debugging into `target/initramfs/initramfs.cpio.gz`.

### 4. Boot in QEMU

```bash
./scripts/qemu-run.sh
```

You should see kernel messages followed by rinit taking over as PID 1:

```text
[INFO] rinit starting as PID 1
[INFO] early bootstrap: mounting virtual filesystems
[INFO] creating runtime directories
[INFO] entering event loop
```

Press `Ctrl+A` then `X` to quit.

### 5. Cross-compile and boot on aarch64 (ARM64)

```bash
# One-time: install aarch64 cross-compilation toolchain
sudo apt install -y gcc-aarch64-linux-gnu qemu-system-arm

# Build rinit + busybox for aarch64, then boot in QEMU.
# Provide your own aarch64 kernel Image as the argument.
./scripts/aarch64-qemu.sh /path/to/aarch64-kernel-Image
```

The script handles everything: `rustup target add`, `cargo build --release` for `aarch64-unknown-linux-musl`, downloads and cross-compiles busybox from source, and packages the initramfs. The linker is configured in `.cargo/config.toml`.

---

## Cross-compilation

The project uses `.cargo/config.toml` to map each target to the correct linker since the host `cc` is x86_64 and cannot link aarch64 objects (`EM: 183`):

| Target | Linker | Notes |
|:---|:---|:---|
| `x86_64-unknown-linux-musl` | `x86_64-linux-gnu-gcc` | Static binary for initramfs |
| `aarch64-unknown-linux-musl` | `aarch64-linux-gnu-gcc` | ARM64 / QEMU `virt` machine |

Rust provides its own musl `crt*.o` and `libc.a`, so the gnu linker only needs to perform the final ELF linking.

### Required packages for aarch64

```bash
sudo apt install -y gcc-aarch64-linux-gnu qemu-system-arm
```

### Manual cross-compilation

```bash
# x86_64
cargo build --release --target x86_64-unknown-linux-musl

# aarch64
cargo build --release --target aarch64-unknown-linux-musl
```

---

## Unit file format (TOML)

rinit uses **TOML** for unit files — cleaner, nestable, and type-safe with `serde`, compared to systemd's custom INI dialect.

### Service unit

```toml
# /etc/rinit/units/nginx.service
name = "nginx.service"

[unit]
description = "Nginx web server"
after = ["network.target"]
wants = ["journal.socket"]

[service]
exec_start = ["/usr/sbin/nginx"]
exec_reload = ["/usr/sbin/nginx", "-s", "reload"]
type = "forking"
restart = "on-failure"
restart_sec = 5

[service.limits]
memory_max = "512M"
cpu_weight = 100
tasks_max = 128
```

### Target unit (grouping)

```toml
# /etc/rinit/units/multi-user.target
name = "multi-user.target"

[unit]
description = "Multi-user system with networking"
requires = ["network.target"]
wants = ["sshd.service", "cron.service"]
after = ["basic.target"]
```

### Supported unit types

| Suffix | Purpose | Status |
|:---|:---|:---|
| `.service` | Long-running daemon | ✅ Parsing + types done; supervision in progress |
| `.target` | Grouping / synchronisation point | ✅ Done |
| `.socket` | Socket activation (fd passing) | 🔜 Planned |
| `.timer` | Time-based activation | 🔜 Planned |
| `.mount` | Filesystem mount point | 🔜 Planned |

---

## Dependency resolution

The dependency solver builds a Directed Acyclic Graph from `Requires` / `Wants` directives and computes the startup order using **Kahn's algorithm**:

- `After` / `Requires` edges form a DAG
- Kahn topological sort produces layers
- Units in the same layer start in **parallel**
- Cycles are detected at load time and rejected

### Dependency directives

| Directive | Behaviour |
|:---|:---|
| `requires` | Hard dependency. If the dependency fails, this unit fails too. |
| `wants` | Soft dependency. Dependency failure does not prevent this unit from starting. |
| `after` | Ordering only. This unit starts *after* the listed units, but does not depend on their success. |
| `before` | Reverse ordering. Listed units start after this one. |
| `conflicts` | Mutual exclusion. Starting this unit stops the conflicting one, and vice versa. |

---

## Design decisions

### Why single-threaded?

A PID 1 init system does **not** do compute-heavy work — it forks child processes that do. A single-threaded event loop eliminates lock contention, simplifies ownership, and makes the code auditable. Early systemd made the same choice.

### Why TOML instead of systemd's INI?

- **Nestable**: `[service.limits]` is clearer than `MemoryMax=512M`.
- **Type-safe**: integers stay integers. No manual `parse_u64()`.
- **serde**: zero-cost deserialisation directly into Rust structs.

### Why musl for the release build?

- Produces a **fully static** binary — no glibc dependency at runtime.
- Ideal for initramfs where no shared libraries are available.
- Smaller binary size.

---

## Development

```bash
# Compile & check (fast, glibc)
cargo check

# Run unit tests (includes DAG solver tests)
cargo test --workspace

# Build for deployment (static, musl)
cargo build --release --target x86_64-unknown-linux-musl
```

### Running as a regular user process

For quick iteration, you can run `rinit` as a normal user (not PID 1). It will immediately panic:

```text
rinit must run as PID 1 (current pid: 12345)
```

You can temporarily remove this check during development by commenting out the assertion in `src/main.rs`. Note that many kernel interfaces (cgroups, mount, signalfd) will fail without `CAP_SYS_ADMIN`.

---

## Roadmap

| Phase | Scope | Status |
|:---|:---|:---|
| 1 | Minimal PID 1 (bootstrap, signalfd, reap) | ✅ Done |
| 2 | Unit file parsing + dependency DAG | ✅ Done |
| 3 | Service fork+exec+supervise with restart policy | 🔨 In progress |
| 4 | cgroups v2 resource limits per service | 🔜 |
| 5 | Socket activation (`.socket` + fd passing) | 🔜 |
| 6 | D-Bus IPC + `initctl` CLI client | 🔜 |
| 7 | Structured journal logging | 🔜 |
| 8 | Device events (netlink uevent / `.device`) | 🔜 |

---

## Further reading

- [systemd for Administrators](https://0pointer.net/blog/projects/systemd-for-admins-1.html) — Lennart Poettering's series
- [systemd design rationale](https://systemd.io/) — Socket activation, cgroups, and more
- [Linux Insides: init](https://0xax.gitbooks.io/linux-insides/content/SysCall/linux-syscall-4.html) — Kernel internals of PID 1 startup
- [systemd source](https://github.com/systemd/systemd) — Reference implementation
- [cgroups v2 kernel docs](https://docs.kernel.org/admin-guide/cgroup-v2.html)

---

## License

MIT
