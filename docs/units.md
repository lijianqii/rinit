# Unit files

rinit's unit format (plan §1.1). It is systemd's shape, deliberately narrowed to
the subset the plan commits to - see [Not supported](#not-supported) for the
rest, and `crates/rinit-core/src/unit/parser.rs` for the authority this document
is written from.

## Where units live

`/etc/rinit/units` by default, overridable with `--units <DIR>`. rinit reads
every `*.service` and `*.target` in that directory at boot, and again on
`rctl reload`. A file that does not parse is a **fatal** boot error rather than a
skipped unit: reference integrity is checked before anything starts (plan
§1.3(1)), so a directory that half-loads cannot produce a half-running system.

A unit's name is its file name minus the directory, and the suffix decides what
it is: `.service` spawns a process, `.target` is a synchronisation point that
spawns nothing (plan §7.2). A name may contain `A-Za-z0-9`, `:`, `_`, `.`, `@`
and `-` - and nothing else, so a name can never escape into a path. Wherever a
unit is *referenced* (`Requires=`, `rctl start`), a bare name gets `.service`
appended: `Requires=db` means `db.service`.

## Syntax

```ini
# a comment, to the end of the line
[Unit]
Description=A name for the console and rctl status

[Service]
Type=smart
ExecStart=/usr/bin/thing --flag
```

- `Key=Value`, one per line, in a `[Unit]` or `[Service]` section. A key before
  any section header, or a key that is not in the tables below, is an error
  (`UnknownKey`) - this parser fails closed on typos instead of ignoring them.
- A key that may appear **once** is an error the second time. Only these repeat:
  `Environment`, `Requires`, `Wants`, `After`, `Before`, `Conflicts`,
  `ExecStartPre`, `ExecStartPost`.
- Blank values are usually meaningful rather than errors; each directive says
  what its empty form means.
- A line ending in `\` continues on the next line, joined with a single space.
  A comment line never continues.
- Values are tokenised *before* they reach any program: spaces separate tokens,
  `"` groups them, and `\` escapes (`\\`, `\"`, `\n`, `\t`, `\s`; anything else
  is the character itself). `#` starts a comment even mid-value. **No shell is
  involved**: `$`, `$(...)` and `!` are ordinary characters, which is why a
  command that needs them asks a shell for them (`ExecStart=/bin/sh -c "..."`).

## `[Unit]`

| key | value | default | meaning |
| --- | ----- | ------- | ------- |
| `Description` | text | empty | Shown by `rctl status`. |
| `Documentation` | text | empty | Free-form reference; nothing reads it. |
| `Type` | `service`, `target` | from the file suffix | What the unit is. Naming it explicitly must agree with the suffix (`x.target` with `Type=service` is an error). |
| `Requires` | unit names | empty | Hard dependencies: a unit that fails takes its dependents with it, and they do not start. |
| `Wants` | unit names | empty | Soft dependencies: pulled in, but a failure does not propagate. |
| `After` | unit names | empty | Ordering only: start after these. Does not pull them in. |
| `Before` | unit names | empty | Ordering only: start before these. |
| `Conflicts` | unit names | empty | Mutual exclusion: starting this stops those first (plan §1.1). |

`Requires` and `After` are what put units in different *levels*; everything in
one level starts in parallel (see `scripts/parallel-boot.sh` for the measurement
of that). `After` alone orders without pulling in, exactly as in systemd.

## `[Service]`

### Startup and commands

| key | value | default | meaning |
| --- | ----- | ------- | ------- |
| `Type` | `simple`, `oneshot`, `notify`, `forking` | `simple` | How startup completion is judged (below). |
| `ExecStart` | command | none | The main command. Required for a service; a `target` has none. |
| `ExecStartPre` | command | none | Run before `ExecStart`; a non-zero exit fails the unit. Repeatable. |
| `ExecStartPost` | command | none | Run after the main command is under way (after exit, for `oneshot`). Repeatable. |
| `ExecStop` | command | empty | The stop action. **Empty means `SIGTERM`** to the process group and to every process in the unit's cgroup. |
| `PIDFile` | absolute path | none | Where a `Type=forking` daemon writes its pid (plan §7.10). |
| `TTYPath` | absolute path | none | A controlling terminal for the service (plan §7.11): the child opens it after `setsid`, takes it with `TIOCSCTTY` and uses it as stdin/stdout/stderr. |
| `TTYReset` | `true`/`false` | `true` | Hang the terminal up when taking it over, so a respawned console service starts on a clean one rather than on the last session's leftovers. Best effort: a device that does not implement it is still used. |
| `WorkingDirectory` | absolute path | `/` | The child's working directory. Empty keeps the default. |
| `Environment` | `KEY=VALUE` (one or more) | empty | Environment for every `Exec*` of the unit. Repeatable. |
| `EnvironmentFile` | absolute path | none | Read further `KEY=VALUE` lines from this file. One that cannot be read is a warning, not a failure - a board without the file still boots. |

`Type=` decides what "started" means (plan §1.3(3)):

- **`simple`** - started at `exec`, the default.
- **`oneshot`** - started when the command exits 0; the unit is then
  `Active(Exited)` and has no process.
- **`notify`** - started when the service sends `READY=1` on `$NOTIFY_SOCKET`;
  bounded by `TimeoutStartSec`.
- **`forking`** - started when the main process has exited 0 *and* the daemon it
  left behind has been located: through `PIDFile=`, or - only if the file is not
  set - through the unit's own cgroup, and only when that cgroup holds exactly one
  process. Anything else fails the start, naming the reason, rather than guessing
  (plan §7.10).

`TTYPath=` is what a `getty`-like service needs before it can run a login shell.
Two consequences are worth knowing: the terminal replaces the log ring for that
unit (its output goes to the terminal, so `rctl log` has nothing), and a
`TTYPath=` that cannot become a controlling terminal - a mistyped device, or
something that is not a terminal at all - **fails the start**, naming the step,
rather than leaving a service that believes it owns a terminal.

### Restarts and timeouts

| key | value | default | meaning |
| --- | ----- | ------- | ------- |
| `Restart` | `no`, `always`, `on-success`, `on-failure`, `on-abnormal` | `no` | Which exits are retried. |
| `RestartSec` | seconds, `0` allowed | `2` | Backoff base; the delay jitters upward from it. |
| `StartLimitBurst` | count, `0` rejected | `5` | Failures allowed inside the window before the breaker trips and the unit stops being retried. |
| `StartLimitIntervalSec` | seconds, `0` allowed | `10` | The window. `0` disables the breaker. |
| `TimeoutStartSec` | seconds, `0` = forever | `30` | How long a start may take (`notify`, `forking`, `oneshot`). |
| `TimeoutStopSec` | seconds, `0` = `SIGKILL` at once | `10` | How long a stop may take before `SIGKILL`. |

### Health checks

| key | value | default | meaning |
| --- | ----- | ------- | ------- |
| `HealthCheck` | command | empty = disabled | Run periodically; a non-zero exit is an unhealthy verdict. |
| `HealthIntervalSec` | seconds, `0` rejected | `30` | Period between probes. |
| `HealthTimeoutSec` | seconds, `0` rejected | `10` | How long one probe may take. |
| `HealthRestartOnFailure` | `true`/`false` | `false` | Whether an unhealthy verdict restarts the unit. |

A probe runs **outside** the unit's cgroup and as the unit's `User=`, like every
other `Exec*` (see the README's deviation list).

### Sandboxing (plan §7.4)

| key | value | default | meaning |
| --- | ----- | ------- | ------- |
| `User` | name, or a numeric uid | empty = root | Drop to this user in the child, before `exec`. A name is resolved from `/etc/passwd`; a number is taken as-is. |
| `Group` | name, or a numeric gid | empty = the uid's own group | Drop to this group; the user's supplementary groups come from `/etc/group`. |
| `NoNewPrivileges` | `true`/`false` | `false` | `PR_SET_NO_NEW_PRIVS`: setuid binaries inside the service stop working. |
| `CapabilityBoundingSet` | capability names | *unset* = inherit | An empty value (**`CapabilityBoundingSet=`**) clears the set: the most restrictive form. |
| `AmbientCapabilities` | capability names | *unset* = grant none | Capabilities kept after the drop, for a service that needs one (`NET_BIND_SERVICE`) without being root. |

Capability names are the kernel's, case-insensitively, with the `CAP_` prefix
optional (`net_bind_service`, `NET_BIND_SERVICE` and `cap_net_bind_service` are
one capability). A name that does not resolve is an error, and a `User=` or
`Group=` that does not resolve fails the start rather than running as root: the
whole point is that a service which cannot be de-privileged must not run.

### Resource limits (plan §7.3)

Every limit is applied to a cgroup of the unit's own, `rinit/<name>`, when the
unit starts; a unit with none of these has no cgroup of its own.

| key | value | default | meaning |
| --- | ----- | ------- | ------- |
| `CgroupMemoryMax` | bytes with optional `K`/`M`/`G` | unlimited | Hard limit; over it, the *cgroup's* OOM killer takes a process (reported on the console, and the unit's `Restart=` decides what happens next). |
| `CgroupMemoryHigh` | bytes, same suffixes | unlimited | Soft limit: throttling and reclaim pressure before the hard one. |
| `CgroupCPUMax` | `50%` or `quota period` in microseconds | unlimited | CPU bandwidth. |
| `CgroupTasksMax` | count, `0` rejected | unlimited | `pids.max`. |

A limit the kernel cannot express (no `pids` controller, no `cpu.max` without
`CONFIG_CFS_BANDWIDTH`) is skipped with a warning and the unit still runs; a write
the kernel *refuses* fails the start, because a unit file claiming a bound the
service does not have is the one outcome §7.3 exists to prevent.

## Examples

The units in [`units/`](../units) are the examples, and each one exists to be
asserted on by `make test-system`: `getty.service` (a console login),
`fault.service` (a crashing service, for the restart policy and the breaker),
`priv.service`/`caps.service`/`ambient.service` (§7.4), `cglimit.service`/
`cgrun.service`/`oom.service` (§7.3), `forking.service` (§7.10) and
`logdemo.service` (a `rctl log` target). Start with `getty.service`:

```ini
[Unit]
Description=Serial console getty
Requires=rinit-init.target
After=rinit-init.target

[Service]
Type=simple
ExecStart=/sbin/getty -L ttyAMA0 115200 vt100
Restart=always
```

## Not supported

Deliberately, per plan §2.2's scope, and each is a decision rather than an
oversight (the README's deviation list has the reasoning):

- `PermissionsStartOnly=` and the `+`/`-`/`!` command prefixes: every `Exec*`
  runs as `User=`/`Group=`.
- Template units (`worker@.service`, `%i`) and instances - plan §7.7.
- `Sockets=` and socket activation, `WantedBy=`/`[Install]` sections (rinit has
  no `enable`: the boot target's closure is what starts), `Type=idle`/`dbus`,
  `$MAINPID`, cgroup v1, and every other directive not in the tables above. An
  unknown key is an error, so a unit written for systemd fails loudly rather than
  silently losing a setting.
