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
Type=simple
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
| `SocketActivate` | absolute path | none | A socket rinit binds and starts the unit from (plan §7.9): the unit is *not* started at boot, the first connection is. The service is handed the listening socket on fd 3, with `LISTEN_FDS=1`. |
| `IdleTimeoutSec` | seconds, `0` = never stop | `0` | How long a socket-activated unit may stay up with no *new* connection before rinit stops it. |
| `Sockets` | absolute path (repeatable) | none | A socket path this unit creates (plan §7.8). rinit creates nothing - this is the declaration that lets another unit's `NeedsSocket=` resolve to *this* unit. |
| `NeedsSocket` | absolute path (repeatable) | none | A socket path this unit needs before it starts (plan §7.8). The unit waits for the path, bounded by `TimeoutStartSec`, and fails its start if it never appears. |
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

### Socket activation

`SocketActivate=` inverts the usual arrangement: rinit binds the socket, the unit
waits, and a connection is what starts the service (plan §7.9). The point is the
memory an idle system does not spend, so this is for a service that is needed
rarely and started quickly.

```ini
[Service]
Type=simple
SocketActivate=/run/api.sock
IdleTimeoutSec=60
ExecStart=/usr/bin/api
```

- **The unit is not started at boot.** `rctl list` shows it `inactive` and the
  socket exists anyway; nothing starts it until something connects (or an
  operator runs `rctl start`).
- **The service gets the listening socket**, on fd 3, with `LISTEN_FDS=1` in its
  environment - the contract `sd_listen_fds` implements. It accepts the connection
  itself; rinit never accepts, because the accepted connection belongs to the
  service.
- **`IdleTimeoutSec=` stops it again** once nothing has connected for that long.
  "Idle" is counted from the last *arrival* - the start, or a connection since -
  so a service serving one long-lived connection looks idle, exactly as it does
  under systemd. That is why the default is `0`, never stop.
- **The socket outlives the service.** rinit holds it for as long as rinit runs,
  and removes it on shutdown, so a client sees a service that comes and goes
  rather than a socket that does.

Two limits worth knowing: the socket is bound at boot, so a *reload* that adds or
removes an activated unit takes effect only after a restart; and rinit does not
create the directory policy around the socket beyond making it `0666` (systemd's
own `SocketMode=` default) - a deployment that wants it narrower puts the socket in
a directory with the permissions it means.

### Sockets as dependencies

`Sockets=` and `NeedsSocket=` express "this unit creates that socket, and that one
waits for it" as a dependency rather than as a start order (plan §7.8):

```ini
# api.service
[Service]
Type=simple
Sockets=/run/api.sock
ExecStart=/usr/bin/api
```

```ini
# client.service
[Service]
Type=simple
NeedsSocket=/run/api.sock
ExecStart=/usr/bin/client
```

Three rules make it work:

- **One provider per path.** Two units declaring the same `Sockets=` is a
  boot-time error, because a `NeedsSocket=` resolves *to* a unit and two
  candidates are an ambiguity nothing later can settle.
- **The edge is hard.** A `NeedsSocket=` whose path some unit declares becomes a
  `Requires=` and an `After=` on that unit, so the consumer is in a later level.
- **The wait is on the path, not on the state.** `Type=simple` is `Active` at
  exec, which is long before a daemon has bound its socket - so a consumer waits
  for the *file* to exist, bounded by its own `TimeoutStartSec`, and fails its
  start (naming the path) if it never does. rinit checks that the path exists, not
  that it is a socket: what to create is the unit's business.

A `NeedsSocket=` that **no** unit declares is not an error - the plan degrades it
to a weak dependency - and the wait still happens, which is the property that
matters. It is reported as a warning at boot so it can be found.

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

[`units/`](../units) is the boot set - the two targets, the console
(`getty.service`) and its alternative (`shell.service`) - and is the whole of what
`make run` packs. The acceptance units live in [`units/demo/`](../units/demo),
which only the image `make test-system` boots carries, and the console starts them
one at a time: `fault.service` (a crashing service, for the restart policy and the
breaker), `priv.service`/`caps.service`/`ambient.service` (§7.4),
`cglimit.service`/`cgrun.service`/`oom.service` (§7.3), `forking.service` (§7.10)
and `logdemo.service` (a `rctl log` target). Start with `getty.service`:

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

## Template units

A unit file whose name has an `@` with nothing after it - `worker@.service` - is
a **template**: not a unit, and not something anything can start. It is the
definition every instance of it is made from.

```
$ rctl list
worker@.service: template
$ rctl start worker@.service
rctl: worker@.service is a template; start an instance, e.g. worker@<name>.service
$ rctl start worker@42.service
started worker@42.service
```

Starting `worker@42.service` instantiates the template: the specifiers below are
expanded into the file's **text**, and the result is parsed as an ordinary unit.
An instance is a unit like any other from then on - it has its own name, its own
state, its own cgroup and its own log - and nothing ever expands it again.

| specifier | expands to | for `worker@42.service` |
| --------- | ---------- | ---------------------- |
| `%i` | the instance | `42` |
| `%n` | the full unit name | `worker@42.service` |
| `%p` | the prefix | `worker` |
| `%%` | a literal per cent | `%` |

Expansion happens over the whole file rather than one field at a time, so a
specifier works in `Environment=`, in `WorkingDirectory=` and in a comment alike.
Anything after a `%` that is not in the table is an **error** rather than a
literal: a template with a typo fails the start, saying which specifier it did not
understand, instead of handing a service the wrong argument. (`%%` is how you
write a per cent that is not a specifier - which matters because the same `%`
otherwise reaches the command line untouched.)

Two things to know before using one:

- **A unit file cannot name an instance.** `Requires=worker@1.service` is
  rejected at load time ("no such unit file exists"), because the dependency graph
  is built from the files in the directory and an instance is not one of them.
  Instances are started by name (`rctl start worker@42.service`), which is also
  what creates them.
- **There is no `enable`.** Nothing makes an instance start at boot, because
  that is what systemd's `[Install]`/`WantedBy=` symlinks are for and rinit has
  neither.

## Not supported

Deliberately, per plan §2.2's scope, and each is a decision rather than an
oversight (the README's deviation list has the reasoning):

- `PermissionsStartOnly=` and the `+`/`-`/`!` command prefixes: every `Exec*`
  runs as `User=`/`Group=`.
- `WantedBy=`/`[Install]` sections (rinit has no `enable`: the boot target's
  closure is what starts), `Type=idle`/`dbus`, `$MAINPID`, cgroup v1, and every
  other directive not in the tables above. An unknown key is an error, so a unit
  written for systemd fails loudly rather than silently losing a setting.

(`Sockets=` and socket activation were on this list before §7.8 and §7.9 landed;
they are in the tables above now, which is where their rules live.)
