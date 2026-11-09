# rinit-tools: independent system tools

Status: proposed, 2026-11-02.

This document defines the first expansion of rinit from a PID 1/service manager
into a small set of **independent system tools**. It is not a BusyBox clone:
there is no multi-call binary, no `argv[0]` dispatch, and no shell or coreutils.

## 1. Decisions

| decision | value |
| --- | --- |
| Binary shape | one independent ELF per tool |
| Linking | glibc dynamic, like the current images |
| Dispatch | none; the file at the standard path is the program |
| Default image profile | `replace-system` |
| Rollback/debug profile | `coexist` (not a release default) |
| BusyBox after replacement | still present for shell/coreutils and as an emergency fallback |
| Shell/coreutils/network tools | explicitly out of scope |

A release image installs real files at the standard paths and removes the
matching BusyBox symlinks. `/bin/busybox` itself is never removed, so
`busybox mount` remains available for diagnosis and rollback.

## 2. Why independent binaries

BusyBox's multi-call model saves disk space, but it also couples every applet to
one binary's size, dependencies, crash domain and release cadence. Independent
binaries give rinit:

- crash isolation: a broken diagnostic tool cannot take down PID 1;
- per-tool feature/size control;
- simple packaging: a file is the program, with no hidden `argv[0]` contract;
- the same glibc/loader packaging the images already use.

The cost is disk space and more build artifacts. That cost is accepted for the
first version and measured by a size gate.

## 3. Profiles

### `replace-system` (default)

Installs the tools at their standard paths and removes the corresponding BusyBox
symlinks:

```text
/bin/mount
/bin/umount
/bin/hostname
/sbin/reboot
/sbin/poweroff
/sbin/sysctl
/bin/ps
/usr/bin/free
/usr/bin/uptime
/bin/dmesg
/bin/df
/usr/bin/top
```

BusyBox keeps `sh`, `ash`, `ls`, `cp`, `mv`, `rm`, `grep`, `sed`, `awk`, the
network tools and every other applet not in the table above.

### `coexist`

Keeps all BusyBox applets and installs the rinit tools under a `rinit-` prefix
(for example `/bin/rinit-mount`). The exception is a tool whose install path is
not the BusyBox applet it replaces: `rlogin` and `rgetty` already use the names
`/bin/rlogin` and `/sbin/rgetty`, while BusyBox keeps `/bin/login` and
`/sbin/getty`, so those two keep their standard paths and the shipped
`getty.service` still works in this profile. This is a development and A/B
profile, and the rollback path if a replaced tool has a regression. It is not
the release default.

## 4. First version tool set

The three existing binaries keep their names and paths:

| binary | path |
| --- | --- |
| `rinit` | `/sbin/rinit` |
| `rctl` | `/bin/rctl` |
| `processes` | `/bin/processes` |

`rinit-check` (`/bin/rinit-check`) is the first tool with no BusyBox
counterpart; its contract is in section 6.

The first `replace-system` set is:

| BusyBox applet | install path | source binary | risk |
| --- | --- | --- | --- |
| `mount` | `/bin/mount` | `rinit-tools/src/bin/mount.rs` | high |
| `umount` | `/bin/umount` | `umount.rs` | high |
| `hostname` | `/bin/hostname` | `hostname.rs` | medium: must support `-F` |
| `reboot` | `/sbin/reboot` | `reboot.rs` | medium: must go through PID 1 |
| `poweroff` | `/sbin/poweroff` | `poweroff.rs` | high: rinit has no poweroff path yet |
| `sysctl` | `/sbin/sysctl` | `sysctl.rs` | medium |
| `ps` | `/bin/ps` | `ps.rs` | medium |
| `free` | `/usr/bin/free` | `free.rs` | low |
| `uptime` | `/usr/bin/uptime` | `uptime.rs` | low |
| `dmesg` | `/bin/dmesg` | `dmesg.rs` | medium |
| `df` | `/bin/df` | `df.rs` | low |
| `top` | `/usr/bin/top` | `top.rs` | medium |

The second wave replaces the file-oriented BusyBox applets:

| BusyBox applet | install path | source binary | risk |
| --- | --- | --- | --- |
| `ls` | `/bin/ls` | `ls.rs` | medium: formatting and symlink display |
| `cat` | `/bin/cat` | `cat.rs` | low |
| `cp` | `/bin/cp` | `cp.rs` | high: recursive copies and symlinks |
| `mv` | `/bin/mv` | `mv.rs` | medium: cross-device fallback |
| `rm` | `/bin/rm` | `rm.rs` | high: destructive operation |
| `mkdir` | `/bin/mkdir` | `mkdir.rs` | low |
| `ln` | `/bin/ln` | `ln.rs` | medium |
| `touch` | `/bin/touch` | `touch.rs` | low |

The third wave adds shell and script basics:

```text
echo [-neE] [STRING...]
sleep NUMBER[smhd]...
uname [-asnrvmpoi]
id [-ugGnr]
whoami
env [-i] [-u NAME] [NAME=VALUE...] [COMMAND [ARG...]]
which [-a] COMMAND...
dirname PATH...
basename NAME [SUFFIX]
realpath [-e|-m] [-q] [-z] PATH...
readlink [-fenmq] PATH...
mktemp [-dqu] [-p DIR] [TEMPLATE]
sync
stat [-L] [-c FORMAT] [-t] FILE...
```

`stat` supports the format directives `%n %s %a %f %u %U %g %G %h %i
%d %D %F %X %Y %Z %y %%`; `realpath` defaults to strict resolution and
`-m` allows missing components. `which` searches `PATH`, not the shell's
command table.

The text-filter additions are:

| BusyBox applet | install path | source binary | risk |
| --- | --- | --- | --- |
| `cut` | `/usr/bin/cut` | `cut.rs` | medium: range lists and field delimiters |
| `head` | `/usr/bin/head` | `head.rs` | low |
| `tail` | `/usr/bin/tail` | `tail.rs` | medium: no follow mode |

The console login pair uses the r-prefixed names. `replace-system` removes the
old BusyBox links:

| BusyBox applet | install path | source binary |
| --- | --- | --- |
| `getty` | `/sbin/rgetty` | `rgetty` |
| `login` | `/bin/rlogin` | `rlogin` |

`rgetty` opens the tty, creates the session, configures the terminal, writes a
`LOGIN_PROCESS` utmp/wtmp record, prints the login prompt and waits for
`/bin/rlogin`. `rlogin` reads `/etc/passwd` and `/etc/shadow`, verifies `$1$` MD5,
`$5$` SHA-256 and `$6$` SHA-512 crypt hashes in-process (the target image has no
libcrypt), writes a `USER_PROCESS` record, drops privileges and execs the
user's shell; `rgetty` writes `DEAD_PROCESS` when the session ends. Root login
honours `/etc/securetty` when that file is present and non-empty. It supports
`-p`, `-h HOST` and root-only `-f USER`; `rgetty` supports `-L`, `-l LOGIN`,
`-h HOST`, `-n`, `-t SEC` and `-I STRING`. `rgetty` resolves the tty operand and
requires it to remain under `/dev`; a non-terminal is rejected before any
ownership or mode change. PAM and yescrypt/bcrypt hashes are not implemented.

Still out of scope:

- `sh`, `ash`;
- `grep`, `sed`, `awk`;
- `ip`, `ifconfig`, `udhcpc`, `ntpd`, `wget`;
- filesystem creation/repair tools.

Tools with no BusyBox conflict keep a `rinit-` prefix. `rinit-check` is the
first of these; `rinit-logs`, `rinit-notify` and `rinit-watchdog` can follow.

## 5. Prerequisite: a real poweroff path

The current rinit shutdown path is:

```text
SIGTERM / SIGINT / SIGPWR -> shutdown services -> reboot()
```

There is no poweroff action. `/sbin/poweroff` must not call `reboot(2)` behind
rinit's back: that would skip service shutdown, log flushing, watchdog disarming
and the rest of the ordered shutdown.

Before implementing `/sbin/poweroff`, add a control command pair:

```text
rctl reboot    -> stop services, then RB_AUTOBOOT
rctl poweroff  -> stop services, then RB_POWER_OFF
```

`/sbin/reboot` and `/sbin/poweroff` are independent binaries that use the
control socket and request these commands. The exact protocol extension must be
versioned; old clients must continue to work.

This is a prerequisite for the `replace-system` profile, not an optional
follow-up.

## 6. Compatibility contract

Every tool documents the options it supports. Unsupported options fail with a
non-zero exit status; they are never silently ignored.

### `/bin/hostname`

Must support at least:

```text
hostname
hostname NAME
hostname -F FILE
```

`units/device/hostname.service` calls `/bin/hostname -F /etc/hostname`; a
replacement that omits `-F` breaks hostname configuration at boot.

### `/bin/mount` and `/bin/umount`

First version must support at least:

```text
mount
mount -t TYPE DEV DIR
mount -o OPTIONS ...
mount -a
mount -n
mount -r
mount -w
umount DIR
umount -a
umount -l
umount -f
```

`mount -a` must read `/etc/fstab`. The implementation must not rely on an
`/etc/mtab` file; the kernel's `/proc/mounts` is authoritative. Unsupported
filesystem types or options are an error, not a silent fallback.

### `/sbin/swapon` and `/sbin/swapoff`

The supported subset is:

```text
swapon [-a] [-s] [-p PRIORITY] [-v] [DEVICE...]
swapoff [-a] [-v] [DEVICE...]
```

`swapon -a` reads `/etc/fstab`, processes every `swap` entry and skips
`noauto`. `swapoff -a` first reads `/proc/swaps`, then processes every `swap`
entry in `/etc/fstab` including `noauto`, so a device activated by hand is
also released. Both commands also accept explicit devices after `-a`, matching
BusyBox. Under `-a`, an already-active device (`EBUSY`) for `swapon` and an
absent or inactive device (`ENOENT`/`EINVAL`) for `swapoff` are not failures.
Like BusyBox, the `/proc/swaps` pass reads the first whitespace-delimited field;
a path containing whitespace is still reached through its escaped fstab entry.

Fstab `pri=`/`priority=` selects a priority in `0..=32767`; the command-line
`-p` overrides it. `discard`, `discard=once` and `discard=pages` set the
corresponding kernel flags. `swapon -s` prints `/proc/swaps` unchanged. UUID
and LABEL resolution, `mkswap`, and swap-file creation or resizing are out of
scope.

### `/bin/rinit-check`

```text
rinit-check [--units DIR] [--default TARGET] [--strict] [--json|--plan]
```

`rinit-check` loads, parses and resolves a unit directory through the same
`rinit-runtime` plan PID 1 uses at boot. It is an offline deployment check, not
a second implementation of the parser.

Non-fatal warnings (for example a missing `Wants=` target) are printed but
exit 0; `--strict` makes them exit 1. A parse error, a dependency cycle, a
missing hard dependency or a missing default target exits 1. Usage errors exit
2. `--json` emits the target, unit/level/template counts, levels and warnings
as one JSON object; `--plan` prints the resolved level lines instead of the
summary. The check validates the static graph only: it does not start units,
probe kernel features, resolve `User=`/`Group=`, check `ExecStart=` paths or
touch cgroups, watchdogs and sockets.

### `/usr/bin/cut`, `/usr/bin/head` and `/usr/bin/tail`

```text
cut (-b LIST|-c LIST|-f LIST) [-d DELIM] [-s] [FILE...]
head [-n N|-c N] [-q|-v] [FILE...]
tail [-n N|-c N] [-q|-v] [FILE...]
```

`LIST` is a comma-separated set of 1-based positions and inclusive ranges:
`3`, `2-5`, `7-` and `-4` are accepted. Ranges are merged before use, and
fields are emitted in input order. `cut` reads bytes for both `-b` and `-c`;
`-c` is not locale-aware. `-d` takes exactly one byte and defaults to tab.
With `-f`, a line without the delimiter is copied unchanged unless `-s` is
given; `-s` and `-d` require `-f`.

`head` and `tail` accept non-negative decimal counts up to the platform's
usable size and default to 10 lines. `-c` counts bytes. Multiple files get
`==> FILE <==` headers (`==> standard input <==` for `-`), suppressed by `-q`
and forced by `-v`; `-` is standard input. `tail -c` seeks and streams regular
files without allocating the full count; non-seekable input is buffered only up
to 16 MiB and rejects larger counts with a clear error. `tail -f`, `-F`,
follow-mode `-s` and the `+N` tail form are rejected rather than approximated.
Live service logs belong to `rctl log`.

### `/sbin/reboot` and `/sbin/poweroff`

Both must go through the rinit control path. They must not call the reboot
syscall directly. Tests must cover service shutdown, log flush, watchdog disarm
and the final reboot/poweroff action.

### `ps`, `free`, `uptime`, `dmesg`, `df`, `top`

These provide a documented BusyBox-compatible subset of columns/options. Golden
tests compare stable fields against BusyBox; byte-for-byte parity is not a goal.
`ps` may print numeric UIDs if `libnss_files` is not packaged; if name lookup is
required, the NSS module becomes an explicit image dependency and gate.

### `sysctl`

Supports reading a key, writing a key, and listing the supported subset.
Writes use `/proc/sys`; permission failures are reported with a non-zero exit.

### File operations (second wave)

The supported subset is deliberately smaller than GNU coreutils; unsupported
options fail instead of being ignored.

```text
ls [-aAldh] [FILE...]
cat [-bnsu] [FILE...]
cp [-rRafpn] SOURCE... DEST
mv [-fn] SOURCE... DEST
rm [-rRfd] FILE...
mkdir [-p] [-m MODE] DIRECTORY...
ln [-sfn] TARGET... LINK
touch [-amc] [-t STAMP] FILE...
```

`cp` and `mv` resolve a destination directory to `DEST/basename(SOURCE)`;
multiple sources require an existing destination directory. `cp` preserves
symlinks instead of following them. `rm` refuses `/`, `.` and `..`. `touch -t`
accepts `MMDDhhmm`, `YYMMDDhhmm`, `CCYYMMDDhhmm`, each with an optional `.ss`.
Ownership is not preserved by `cp -p`; mode and timestamps are.
On Linux, `cp`/`mv` open the destination parent with `openat2` and
`RESOLVE_NO_SYMLINKS` when the kernel provides it. A symlinked parent component
is refused rather than followed, so a privileged copy cannot be redirected after
the destination was resolved.

## 7. Architecture

```text
crates/
  rinit-core/          existing: unit/config/state machine
  rinit-runtime/       existing: syscalls/process/signals
  rinit-tools/         new: common tool library and tool binaries
    src/lib.rs         argument parsing, errors, output, /proc helpers
    src/bin/
      mount.rs
      umount.rs
      swapon.rs
      swapoff.rs
      rinit-check.rs
      cut.rs
      head.rs
      tail.rs
      hostname.rs
      reboot.rs
      poweroff.rs
      sysctl.rs
      ps.rs
      free.rs
      uptime.rs
      dmesg.rs
      df.rs
      top.rs
  rinit/               existing PID 1; does not depend on rinit-tools
  rctl/                existing
  processes/           existing
```

Each `src/bin/*.rs` produces one ELF. The shared `rinit-tools` library is linked
statically into each tool; there is no custom `librinit-tools.so` in the first
version. glibc remains dynamic.

### PID 1 isolation

`rinit` must not link `rinit-tools`. Its dependency tree stays limited to
`rinit-core`, `rinit-runtime` and glibc. A tool crash may kill that tool, but it
must not affect PID 1.

## 8. Build and installation

### Tool manifest

`tools.list` is the source of truth for paths, source binaries, BusyBox applets
and size budgets:

```text
# applet   cargo-bin   install-path    busybox-link    max-kib  source
rinit-check rinit-check /bin/rinit-check -               600     crates/rinit-tools/src/bin/rinit-check.rs
cut        cut         /usr/bin/cut     /usr/bin/cut     600     crates/rinit-tools/src/bin/cut.rs
head       head        /usr/bin/head    /usr/bin/head    600     crates/rinit-tools/src/bin/head.rs
tail       tail        /usr/bin/tail    /usr/bin/tail    600     crates/rinit-tools/src/bin/tail.rs
mount      mount       /bin/mount      /bin/mount      1024     crates/rinit-tools/src/bin/mount.rs
umount     umount      /bin/umount     /bin/umount     1024     crates/rinit-tools/src/bin/umount.rs
swapon     swapon      /sbin/swapon    /sbin/swapon     600     crates/rinit-tools/src/bin/swapon.rs
swapoff    swapoff     /sbin/swapoff   /sbin/swapoff    600     crates/rinit-tools/src/bin/swapoff.rs
hostname   hostname    /bin/hostname   /bin/hostname    600     crates/rinit-tools/src/bin/hostname.rs
reboot     reboot      /sbin/reboot    /sbin/reboot     600     crates/rinit-tools/src/bin/reboot.rs
poweroff   poweroff    /sbin/poweroff  /sbin/poweroff   600     crates/rinit-tools/src/bin/poweroff.rs
sysctl     sysctl      /sbin/sysctl    /sbin/sysctl     600     crates/rinit-tools/src/bin/sysctl.rs
ps         ps          /bin/ps         /bin/ps          800     crates/rinit-tools/src/bin/ps.rs
free       free        /usr/bin/free   /usr/bin/free    600     crates/rinit-tools/src/bin/free.rs
uptime     uptime      /usr/bin/uptime /usr/bin/uptime  600     crates/rinit-tools/src/bin/uptime.rs
dmesg      dmesg       /bin/dmesg      /bin/dmesg       800     crates/rinit-tools/src/bin/dmesg.rs
df         df          /bin/df         /bin/df          800     crates/rinit-tools/src/bin/df.rs
top        top         /usr/bin/top    /usr/bin/top     800     crates/rinit-tools/src/bin/top.rs
```

The exact BusyBox symlink location may vary by BusyBox configuration; the
installer resolves it from the built rootfs and fails if the target is not the
expected applet link.

### Build

```bash
make ARCH=arm64 tools
make ARCH=arm32 tools
```

The default profile is `replace-system`. `tools` builds every binary in the
manifest for the selected target. Both image paths install the manifest:
`make run` does it while assembling the initramfs, and `make run-disk` does it
through `disk-image`. `run-disk` reuses an existing ext4 image, so
`make disk-image` is the explicit rebuild when the tool set changes.

### Install

`scripts/install-tools.sh`:

1. verifies each source is an ELF, not a symlink;
2. verifies the target, if present, is the expected BusyBox symlink;
3. removes that symlink;
4. installs the real file with mode `0755`;
5. never touches `/bin/busybox`;
6. emits the final mapping for the guest test to check.

After installation:

```text
/bin/mount       regular ELF, not a symlink
/bin/hostname    regular ELF, not a symlink
/sbin/reboot     regular ELF, not a symlink
/bin/ls          regular ELF, not a symlink
/bin/busybox     BusyBox ELF
/bin/sh          symlink to busybox
```

## 9. glibc dynamic-link gate

Every tool must pass:

1. interpreter:
   - aarch64: `/lib/ld-linux-aarch64.so.1`
   - armv7: `/lib/ld-linux-armhf.so.3`
2. `readelf -d` NEEDED allowlist:
   - `libc.so.6`
   - `libgcc_s.so.1`
   - `libm.so.6` only after it is also added to the image's runtime libraries
     (it is not shipped today)
   - `libnss_files.so.2` only if name lookup is explicitly enabled
3. no static executable, no musl interpreter, no undeclared shared object;
4. execution from a rootfs that contains only the image's loader and libraries;
5. `--help` and a minimal real operation succeed in that rootfs.

The image already packages the loader, `libc.so.6` and `libgcc_s.so.1`; the gate
must verify each new tool against that same set rather than relying on the build
host.

## 10. Size budgets

Current reference measurements:

```text
rinit       about 0.92 MB
rctl        about 0.33 MB
processes   about 0.46 MB
```

Provisional first-version budgets, to be replaced by measured values:

| profile | tools | total budget |
| --- | --- | --- |
| minimal | `rinit` only | 1.2 MB |
| `replace-system` (first three waves + console + swap + check + filters) | 45 including existing | 20 MB |
| full toolbox (later) | 50+ | 24 MB |

Each tool also has its own budget in `tools.list`. CI fails on either a per-tool
or a total overrun. If the installed set exceeds its total budget, evaluate a
shared `librinit-tools.so` for non-init tools; do not add it pre-emptively.

## 11. Testing

### Unit/golden

- argument parsing, missing/invalid options, exit codes;
- stable output fields compared with BusyBox for the supported subset;
- mount/umount against real tmpfs/devpts entries, not only `--help`;
- swap fstab filtering, `pri=`/`discard=` flags and the `-a` idempotency rules;
- `rinit-check` against valid, warning-only, cyclic and missing-target trees;
- `cut` range merging, empty fields and missing delimiters; `head`/`tail`
  boundaries, byte counts and multi-file headers;
- hostname `-F`;
- file operations: recursive copy, no-clobber, symlink preservation, directory
  removal guards, hard/symbolic links and `touch -t`;
- script basics: PATH search, environment handling, user/group lookup, path
  resolution, temporary names and `stat` formatting;
- console login: tty setup, shadow `$1$`/`$5$`/`$6$` authentication, utmp/wtmp
  records, securetty and shell exec;
- reboot/poweroff through a fake control socket and then a real guest.

### Multi-binary/install

- every installed path is a regular ELF, not a symlink;
- the corresponding BusyBox symlink is gone;
- `/bin/busybox` remains and `busybox mount` still works;
- `command -v mount` resolves to `/bin/mount`;
- no tool is missing its loader or libc in the image.

### Guest

- the existing 105 `make test-system` assertions on the `replace-system` image;
- a swap round trip: `swapon`, `swapon -s`, `swapoff` and `swapon -a`;
- `rinit-check` on the shipped directory and a rejected bad target;
- `cut`, `head` and `tail` with representative fields, byte ranges and files;
- T3 reboot/poweroff persistence tests;
- 24 h and 7 day soaks for `rinit`, with the tools not affecting the PID 1
  memory curve;
- `coexist` A/B run to compare behavior against BusyBox.

### CI

- build and run every tool on aarch64 and armv7;
- glibc interpreter/NEEDED gate;
- per-tool and total size gates;
- no symlink at a replaced path;
- BusyBox fallback still executable.

## 12. Rollout

| phase | content | estimate |
| --- | --- | --- |
| 0 | spec, `tools.list`, compatibility matrix, budgets | 1 week |
| 1 | `rinit-tools` library, install script, glibc/size gates | 2 weeks |
| 2 | safe tools: hostname, ps, free, uptime, dmesg, df, sysctl | 2–3 weeks |
| 3 | mount/umount compatibility and real filesystem tests | 2–3 weeks |
| 4 | `rctl reboot/poweroff` and `/sbin/reboot`, `/sbin/poweroff` | 1–2 weeks |
| 5 | `replace-system` image, 105 guest assertions, 24 h soak | 2 weeks |

MVP: about 8–10 weeks. Full first-version toolbox: about 3 months. These
estimates assume one or two engineers familiar with Rust and embedded Linux and
exclude real-board validation.

## 13. Risks

| risk | mitigation |
| --- | --- |
| `mount` compatibility | A/B against BusyBox, explicit option subset, keep `busybox mount` fallback |
| no poweroff path today | add `rctl poweroff` and protocol tests first |
| `hostname -F` regression | first-version requirement, guest assertion |
| removing BusyBox links breaks scripts | run the full 105 guest assertions; keep the `coexist` profile for rollback |
| glibc/loader mismatch | same cross toolchain, readelf/ldd gate, image-only rootfs test |
| total size over budget | per-tool and total gates; evaluate shared `.so` only if measured need |
| tool security | uniform parsing, path checks, fuzzing; no shell/coreutils scope |
| scope creep | fixed 12-tool first version; network/coreutils explicitly out |

## 14. Open questions

1. Exact `mount`/`umount` option subset for the first release.
2. Control protocol shape for reboot/poweroff and its versioning.
3. Whether `ps` must resolve user names in the first version, which would pull in
   `libnss_files.so.2`.
4. Whether the BusyBox binary keeps the replaced applets compiled (for
   `busybox mount` fallback) or drops them to save space. The current proposal
   keeps them for the first version.
5. Final per-tool size budgets after the first measured build.

## 15. References

- `README.md` - current project status and image/glibc layout.
- `docs/porting.md` - kernel and target requirements.
- `docs/units.md` - unit format and service-management behavior.
- `crates/rinit/src/boot.rs` - current shutdown/reboot path.
- `scripts/build-initramfs.sh` - image assembly and glibc packaging.
- `scripts/mkdisk.sh` - ext4 image creation.
