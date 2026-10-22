#!/usr/bin/env bash
# System test: boot rinit as PID 1 in qemu and assert on the serial console.
#
# Usage:
#   scripts/system-test.sh [--arch aarch64|x86_64] [--kernel IMAGE] [--initrd FILE]
#                          [--timeout SECONDS] [--log FILE] [--keep-log]
#
# What it does:
#   1. boots the guest from its unit directory (units/multi-user.target), which
#      starts getty.service and gives an interactive root login;
#   2. types at that login to prove the shadow-password root shell is reachable,
#      following the console prompts rather than guessing at delays;
#   3. kills that console service and checks rinit restarts it (M2);
#   4. captures the serial console to a log file;
#   5. asserts the M0-M5 acceptance list on that log;
#   6. reports kernel panics / oopses as failures.
#
# It boots the *acceptance* image (`make initramfs-demo`), not the one `make run`
# packs: the §7.x units it drives from the console live in `units/demo/`, which
# the run image does not carry. Nothing in the boot closure references them, so
# they arrive inactive and the console starts them one at a time.
#
# `--keep-log` keeps the log even when every assertion passes (it is always
# kept on failure).
#
# Exit codes:
#   0  all assertions passed
#   1  an assertion failed (the log path is printed)
#   2  bad usage / missing artifacts
#   3  qemu itself could not be started
#
# This is the automated form of the manual verification in plan §4.3. It
# needs `qemu-system-<arch>` and prebuilt artifacts (see the Makefile's
# `test-system` target, which builds them first when needed).
set -euo pipefail

arch="aarch64"
kernel=""
initrd=""
timeout_s=90
log_file=""
keep_log=0
# No --service by default: from M1 the guest boots the unit directory, and
# passing a service on the kernel cmdline would override it and skip exactly
# the behaviour this test exists to check.
service=""
# Extra kernel cmdline tokens. The only one needed is the watchdog timeout
# (plan §7.5): the default is 30 s, and the test wants a window short enough for
# the guest to outlive it and *prove* the supervisor loop is feeding the device.
# Rinit's own default is what `make run` boots with.
kernel_args="--watchdog-timeout 10"

usage() { sed -n '2,27p' "$0"; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch) arch="${2:?}"; shift 2 ;;
        --kernel) kernel="${2:?}"; shift 2 ;;
        --initrd) initrd="${2:?}"; shift 2 ;;
        --timeout) timeout_s="${2:?}"; shift 2 ;;
        --log) log_file="${2:?}"; shift 2 ;;
        --service) service="${2:?}"; shift 2 ;;
        --keep-log) keep_log=1; shift ;;
        -h | --help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

: "${kernel:?--kernel is required}"
: "${initrd:?--initrd is required}"

for f in "${kernel}" "${initrd}"; do
    if [[ ! -f "${f}" ]]; then
        echo "error: missing artifact ${f}" >&2
        echo "hint: run 'make initramfs kernel' to build it." >&2
        exit 2
    fi
done

if [[ -z "${log_file}" ]]; then
    mkdir -p "${repo_root}/build/logs"
    log_file="${repo_root}/build/logs/system-test-${arch}.log"
fi
mkdir -p "$(dirname "${log_file}")"

qemu_bin="qemu-system-${arch}"
if ! command -v "${qemu_bin}" >/dev/null 2>&1; then
    echo "error: ${qemu_bin} not found in PATH" >&2
    exit 3
fi

echo "== rinit system test (${arch}) =="
echo "   kernel: ${kernel}"
echo "   initrd: ${initrd}"
echo "   log:    ${log_file}"
echo "   booting (timeout ${timeout_s}s, the guest is killed once assertions are satisfiable)..."

# The console shell reads from the serial port, so the test has to *type* at
# it to prove a shell is actually reachable. A here-doc piped straight into
# qemu would be consumed long before the guest boots, so the commands are fed
# through a FIFO on a delay instead.
#
# The delay has to clear the whole boot (kernel + mounts + plan + units); 14s
# is comfortable on the aarch64 virt machine and short enough not to dominate
# the run. If a future image boots much slower this becomes a flake, so the
# final `exit` is what guarantees the guest releases the pipe.
fifo_dir="$(mktemp -d "${TMPDIR:-/tmp}/rinit-system-test.XXXXXX")"
fifo="${fifo_dir}/console"
mkfifo "${fifo}"
trap 'rm -rf "${fifo_dir}"' EXIT

# Wait until the console log matches a pattern, or give up after `timeout_s`.
#
# Driving the guest off fixed sleeps is what made the login assertion flaky: a
# boot that finished early still worked, but one that took a moment longer had
# the username typed before getty was listening, and the input was lost. Asking
# the log instead makes the feeder follow the guest rather than race it.
await_log() {
    local timeout_s="$1" pattern="$2"
    local deadline=$((SECONDS + timeout_s))
    while ((SECONDS < deadline)); do
        if grep -qE -- "${pattern}" "${log_file}"; then
            return 0
        fi
        sleep 0.2
    done
    return 1
}

# Hold the write end open for the life of qemu, then type as the guest prompts.
(
    exec 3>"${fifo}"

    # getty shows `login:`, so answer with the username and the default password
    # from /etc/shadow. `id` afterwards proves the shadow/login path worked.
    await_log 40 'login:' || true
    printf 'root\n' >&3
    sleep 1
    printf 'toor\n' >&3
    sleep 1
    printf 'id\n' >&3
    await_log 10 'uid=0\(root\)' || true

    # The boot set's DHCP client. qemu's user-mode network answers the lease, so
    # the interface, the default route and the resolver all have to be set by
    # the unit - not by anything the test typed.
    printf 'n=$(rctl status network.service | grep state); echo net-$n; ip addr show dev eth0 | grep "inet "; ip route | grep default; grep nameserver /etc/resolv.conf\n' >&3
    await_log 10 'net-state: active' || true
    await_log 10 'inet 10\.0\.2\.15/24' || true
    await_log 10 'default via 10\.0\.2\.2' || true
    await_log 10 'nameserver 10\.0\.2\.3' || true

    # Plan §7.2: the default target has no process, but reaching it must leave
    # it `active` in the control interface, not merely in the boot log.
    printf 't=$(rctl status multi-user.target | grep state); echo boot-target-$t\n' >&3
    await_log 10 'boot-target-state: active' || true

    # Plan §0.1's memory column: rinit's own RSS at boot, read out of the kernel
    # rather than out of rinit. This is the *idle* number, so it is measured here,
    # before anything on this console starts another unit.
    printf 'v=$(grep VmRSS /proc/1/status); echo idle-$v\n' >&3
    await_log 10 'idle-VmRSS:' || true

    # Plan §1.3(9): the second wave. The image ships an `/etc/fstab` with two
    # entries (`devpts` on `/dev/pts` and a `tmpfs` on `/data`), so what is
    # asserted is the kernel's mount table rather than rinit's log line - one is
    # a claim about the filesystem, the other about intent.
    printf 'm=$(grep " /data " /proc/mounts); echo fstab-$m\n' >&3
    await_log 10 'fstab-tmpfs /data tmpfs' || true
    printf 'p=$(grep " /dev/pts " /proc/mounts); echo fstab-$p\n' >&3
    await_log 10 'fstab-devpts /dev/pts devpts' || true

    # M3: drive rinit from the guest console. Each step waits for output that
    # only that command can produce, so a failure surfaces as a timeout rather
    # than as a race.
    printf 'rctl list\n' >&3
    await_log 10 'logdemo.service: inactive' || true
    printf 'rctl status getty.service\n' >&3
    await_log 10 'target: started' || true
    printf 'rctl start logdemo.service\n' >&3
    await_log 10 'started logdemo.service' || true
    # Give the oneshot a moment to run and its output to reach the buffer.
    sleep 1
    printf 'rctl log logdemo.service\n' >&3
    await_log 10 'rinit-log-marker' || true
    # A completed oneshot has nothing to restart: exit 4.
    printf 'rctl restart logdemo.service\n' >&3
    await_log 10 'no process to restart' || true
    printf 'rctl stop logdemo.service\n' >&3
    await_log 10 'stopped logdemo.service' || true

    # M2's fault injection is no longer part of the boot closure: `fault.service`
    # lives in `units/demo/` and nothing references it, so the console starts it
    # here. `/bin/false` with `Restart=always` and a burst of two is what trips
    # the breaker, and the two static checks at the end of this script read those
    # lines back out of the log. The `reset-failed` comes after the failures, so
    # that "cleared the failed unit" is about a unit that has failed.
    printf 'rctl start fault.service\n' >&3
    await_log 10 'started fault.service as pid' || true
    await_log 20 'fault.service will restart in' || true
    await_log 20 'error: fault.service hit its start limit' || true
    printf 'rctl reset-failed fault.service\n' >&3
    await_log 10 'fault.service: cleared' || true

    # M5 §7.4: a unit that asks to be de-privileged must actually be so. The
    # command prints its uid, its gid and its own `NoNewPrivs` value, so this
    # one line is the whole claim - and it is read back through `rctl log`,
    # which also proves capture still works for a process that is not root.
    printf 'rctl start priv.service\n' >&3
    await_log 10 'started priv.service' || true
    sleep 1
    printf 'rctl log priv.service\n' >&3
    await_log 10 'uid=65534 gid=65534 nnp=1' || true

    # M5 §7.4: capabilities. `caps.service` stays root and empties its bounding
    # set; `ambient.service` runs as nobody and keeps exactly NET_BIND_SERVICE.
    # Both print the masks from their own /proc/self/status, so what is asserted
    # is the kernel's view of the service, not what rinit meant to do.
    printf 'rctl start caps.service\n' >&3
    await_log 10 'started caps.service' || true
    printf 'rctl start ambient.service\n' >&3
    await_log 10 'started ambient.service' || true
    sleep 1
    printf 'rctl log caps.service\n' >&3
    await_log 10 'bnd=0000000000000000' || true
    printf 'rctl log ambient.service\n' >&3
    await_log 10 'amb=0000000000000400' || true

    # M5 §7.3: cgroup v2 limits. `cglimit.service` reads its own placement and
    # its own limits back out of the kernel, so the assertions on its output are
    # about what the kernel did rather than what rinit meant to do.
    printf 'rctl start cglimit.service\n' >&3
    await_log 10 'started cglimit.service' || true
    sleep 1
    printf 'rctl log cglimit.service\n' >&3
    await_log 10 'cpu=50000 100000' || true

    # M5 §7.3: a *running* unit is inside its cgroup, and a stopped one leaves
    # nothing behind. `$(cat ...)` and `test -n` are how the console asks the
    # kernel directly: `cgroup.procs` is a live view (and st_size 0, so `test -s`
    # would be answered by `stat` rather than by the processes in it), and
    # `test -d` after the stop is the cleanup.
    #
    # `v=populated` is not decoration: the console *echoes* what is typed, and
    # those echo lines are in the same log the assertions read, so a command
    # written as `echo cgrun-populated` would satisfy its own assertion. Building
    # the marker from a variable keeps the literal string out of the input, which
    # is what makes the assertion about the shell's output.
    printf 'rctl start cgrun.service\n' >&3
    await_log 10 'started cgrun.service' || true
    sleep 1
    printf 'v=populated; p=$(cat /sys/fs/cgroup/rinit/cgrun.service/cgroup.procs); test -n "$p" && echo cgrun-$v || echo cgrun-empty\n' >&3
    await_log 10 'cgrun-populated' || true
    printf 'rctl stop cgrun.service\n' >&3
    await_log 10 'stopped cgrun.service' || true
    sleep 1
    printf 'v=gone; d=/sys/fs/cgroup/rinit/cgrun.service; test -d $d && echo cgrun-left || echo cgrun-$v\n' >&3
    await_log 10 'cgrun-gone' || true
    # The oneshot has exited by the time its log is read, so its cgroup has to
    # be gone too - the same cleanup, reached by the other exit path.
    printf 'v=gone; d=/sys/fs/cgroup/rinit/cglimit.service; test -d $d && echo cglimit-left || echo cglimit-$v\n' >&3
    await_log 10 'cglimit-gone' || true

    # M5 §7.3: the OOM link. This unit allocates past its own `memory.max`, so
    # the *cgroup* OOM killer takes it; rinit has to name that as the cause and
    # then follow the unit's restart policy. The string doubling is the cheapest
    # way to fill 16M from a shell.
    printf 'rctl start oom.service\n' >&3
    await_log 10 'started oom.service' || true
    await_log 20 'killed by the cgroup OOM killer' || true
    await_log 20 'oom.service will restart in' || true

    # M5 §7.5: the hardware watchdog. This one is *not* a stand-in: run-qemu.sh
    # attaches `-device i6300esb`, the guest kernel is built with the driver and
    # `CONFIG_WATCHDOG_SYSFS`, and devtmpfs gives rinit a real `/dev/watchdog`.
    # The 10 s timeout comes from `--kernel-args` (the default is 30 s; the test
    # wants a window short enough to outlive), so the feed interval is 5 s.
    #
    # Three things are asked of the kernel: what the device is, that it is
    # `active`, and what timeout it is running - the last one is the ioctl's
    # answer from a real driver, not rinit's intention. Then the uptime, because
    # the proof that the supervisor loop is *feeding* it is that this machine is
    # still here: with the device armed and nothing feeding it, the hardware
    # resets the machine within the timeout.
    printf 'd=/sys/class/watchdog/watchdog0; echo wd-ident=$(cat $d/identity) wd-state=$(cat $d/state) wd-timeout=$(cat $d/timeout)\n' >&3
    await_log 10 'wd-state=active' || true
    printf 'u=$(cut -d" " -f1 /proc/uptime); echo wd-uptime=$u\n' >&3
    await_log 10 'wd-uptime=' || true

    # M5 §7.6: reload. The unit directory is the ramfs the initramfs unpacked
    # `units/` into, so the console can change it while rinit is running - which
    # is what makes "does a reload do the right thing to a live system" a
    # question the guest can answer at all. Files are written a line at a time
    # because the console feeds the shell a line at a time.
    printf 'rctl reload\n' >&3
    await_log 15 'reload: 0 added' || true

    # An added unit is registered and *not* started: nothing the boot target
    # requires pulls it in. The summary line is what separates the two.
    printf 'echo "[Service]" > /etc/rinit/units/reload.service\n' >&3
    printf 'echo "Type=oneshot" >> /etc/rinit/units/reload.service\n' >&3
    printf 'echo "ExecStart=/bin/echo reloaded-service" >> /etc/rinit/units/reload.service\n' >&3
    printf 'rctl reload\n' >&3
    await_log 15 'registered reload.service' || true

    # A changed unit that is *not* running takes the new definition at once; one
    # that *is* running waits for its next start. `logdemo.service` is down and
    # `getty.service` is the console itself, and both files end inside their
    # `[Service]` section, so appending a key to them stays in that section.
    printf 'echo "ExecStartPre=/bin/echo reloaded-pre" >> /etc/rinit/units/logdemo.service\n' >&3
    printf 'echo "ExecStartPost=/bin/true" >> /etc/rinit/units/getty.service\n' >&3
    printf 'rctl reload\n' >&3
    await_log 15 'logdemo.service: new definition in force' || true
    await_log 15 'getty.service is running' || true

    # Removing the added unit: it never ran, so nothing has to be stopped, and
    # it is gone from `rctl` afterwards.
    printf 'rm /etc/rinit/units/reload.service\n' >&3
    printf 'rctl reload\n' >&3
    await_log 15 'removed reload.service' || true
    printf 'rctl start reload.service\n' >&3
    await_log 15 'no such unit: reload.service' || true

    # A directory that does not resolve is refused whole: `getty.service` is
    # required by the boot target, so its absence is a dangling reference. The
    # console is itself getty, so if the refusal did *not* leave the running
    # configuration alone, the rest of this list would not be typed at all.
    printf 'cp /etc/rinit/units/getty.service /tmp/getty.unit\n' >&3
    printf 'rm /etc/rinit/units/getty.service\n' >&3
    printf 'rctl reload\n' >&3
    await_log 15 'reload rejected' || true
    printf 'cp /tmp/getty.unit /etc/rinit/units/getty.service\n' >&3

    # M5 §7.10: `Type=forking`. `forking.service` is a shell that backgrounds a
    # `sleep` and exits, which is the shape of a double-fork daemon: the daemon
    # is reparented to rinit, is *not* the leader of its process group, and the
    # `PIDFile=` is the only thing that says which process the unit now is.
    printf 'rctl start forking.service\n' >&3
    await_log 15 'forking.service: main process exited; daemon is pid' || true
    printf 'd=$(cat /run/forking.pid); echo forking-daemon=$d\n' >&3
    await_log 10 'forking-daemon=' || true
    printf 'rctl status forking.service\n' >&3
    await_log 10 'state: active' || true

    # And the fail-closed half of the plan's boundary, on a unit written at
    # runtime: a `forking` unit whose `PIDFile=` is never written does not become
    # active. `/run/not-written.pid` does not exist, so the file is the reason.
    printf 'echo "[Service]" > /etc/rinit/units/badfork.service\n' >&3
    printf 'echo "Type=forking" >> /etc/rinit/units/badfork.service\n' >&3
    printf 'echo "PIDFile=/run/not-written.pid" >> /etc/rinit/units/badfork.service\n' >&3
    printf 'echo "ExecStart=/bin/true" >> /etc/rinit/units/badfork.service\n' >&3
    printf 'rctl reload\n' >&3
    await_log 15 'registered badfork.service' || true
    printf 'rctl start badfork.service\n' >&3
    await_log 15 'cannot locate the daemon' || true
    printf 's=$(rctl status badfork.service | grep state); echo bf-state=$s\n' >&3
    await_log 10 'bf-state=' || true

    # M5 §7.11: the terminal path. The guest has no spare tty - the console is
    # getty's - so what it can prove here is the fail-closed half: a `TTYPath=`
    # that is not a terminal must fail the start rather than hand the service a
    # terminal it does not have. The success path is the pty test in
    # `crates/rinit/tests/m2_lifecycle.rs`, which can create one on demand.
    printf 'rctl start tty.service\n' >&3
    await_log 15 'ioctl(TIOCSCTTY)' || true

    # M5 §7.9: socket activation. The guest cannot connect (busybox `nc` has no
    # Unix-socket support), so what it asserts is the rest of the mechanism: the
    # socket is bound at boot while the unit stays down, and a start - by hand,
    # which is the second way in - hands the service the *listening* socket.
    printf 'test -S /run/api.sock && echo api-socket-bound || echo api-socket-missing\n' >&3
    await_log 10 'api-socket-bound' || true
    printf 's=$(rctl status api.service | grep state); echo api-state=$s\n' >&3
    await_log 10 'api-state=' || true
    printf 'rctl start api.service\n' >&3
    await_log 15 'started api.service' || true
    sleep 1
    printf 'rctl log api.service\n' >&3
    await_log 10 'activated-fd-3' || true

    # M5 §7.8: sockets as dependencies. The provider creates its socket a second
    # after it is exec'd, so the consumer's start has to *wait for the path* - and
    # `rctl start sockc.service` blocks until it appears, which is why the reply
    # itself is part of the evidence.
    printf 'rctl start sockd.service\n' >&3
    await_log 15 'started sockd.service' || true
    printf 'rctl start sockc.service\n' >&3
    await_log 20 'started sockc.service' || true
    sleep 1
    printf 'rctl log sockc.service\n' >&3
    await_log 15 'socket-present' || true

    # And the other half: a path that never appears fails the start, naming it.
    # No unit declares `/run/never.sock`, which is also the plan's weak-dependency
    # degradation - no boot error, but the wait still happens.
    printf 'echo "[Service]" > /etc/rinit/units/waitsock.service\n' >&3
    printf 'echo "Type=oneshot" >> /etc/rinit/units/waitsock.service\n' >&3
    printf 'echo "NeedsSocket=/run/never.sock" >> /etc/rinit/units/waitsock.service\n' >&3
    printf 'echo "TimeoutStartSec=1" >> /etc/rinit/units/waitsock.service\n' >&3
    printf 'echo "ExecStart=/bin/sh -c \"echo should-not-run\"" >> /etc/rinit/units/waitsock.service\n' >&3
    printf 'rctl reload\n' >&3
    await_log 15 'registered waitsock.service' || true
    printf 'rctl start waitsock.service\n' >&3
    await_log 20 'NeedsSocket=/run/never.sock did not appear in time' || true

    # M5 §7.7: a template unit. `worker@.service` is not a unit - it cannot be
    # started - and the instance the console asks for is created out of it with
    # the specifiers already expanded.
    printf 'rctl start worker@.service\n' >&3
    await_log 15 'is a template; start an instance' || true
    printf 'rctl start worker@42.service\n' >&3
    await_log 15 'started worker@42.service' || true
    sleep 1
    printf 'rctl log worker@42.service\n' >&3
    await_log 15 'worker-instance=42' || true

    # Plan §1.3(7): the log file. The guest boots with the default path, so this
    # checks the plan's own setting rather than a flag the test invented - and the
    # directory it names does not exist in the initramfs, so rinit having created
    # it is part of what is being asserted.
    printf 'v=$(grep -c rinit-log-marker /var/log/rinit/log); echo logfile-marker=$v\n' >&3
    await_log 10 'logfile-marker=' || true

    # Stopping the unit has to stop the *daemon*: it is the unit's process, and
    # it is not in the process group the stop signal addresses, so this is also
    # the assertion for the fallback that reaches the pid itself.
    printf 'rctl stop forking.service\n' >&3
    await_log 15 'stopped forking.service' || true
    sleep 1
    printf 'v=gone; d=$(cat /run/forking.pid); kill -0 $d 2>/dev/null && echo daemon-alive || echo daemon-$v\n' >&3
    await_log 10 'daemon-gone' || true

    # Plan §0.1's other half: RSS with 20 services running. They are written and
    # registered through the reload from §7.6, then started one at a time -
    # serial is fine here, because what is being measured is what 20 live units
    # cost rinit, not how fast they start (that comparison is
    # `scripts/parallel-boot.sh`).
    printf 'i=0; while [ $i -lt 20 ]; do printf "[Service]\\nType=simple\\nExecStart=/bin/sleep 300\\n" > /etc/rinit/units/load${i}.service; i=$((i+1)); done\n' >&3
    sleep 1
    printf 'rctl reload\n' >&3
    await_log 15 '20 added' || true
    # One line per unit, each awaited: the console is a serial line, and twenty
    # commands written back to back overrun what the guest's tty buffers - ten of
    # the twenty used to arrive, which the "all 20 were running" assertion below
    # is what caught.
    for i in $(seq 0 19); do
        # The same names the loop above wrote: `load0`..`load19`, unpadded.
        name="$(printf 'load%d.service' "$i")"
        printf 'rctl start %s\n' "${name}" >&3
        await_log 10 "started ${name}" || true
    done
    sleep 2
    printf 'v=$(grep VmRSS /proc/1/status); echo load-$v\n' >&3
    await_log 10 'load-VmRSS:' || true

    # M2: kill the console service and let rinit bring it back. `login` execs
    # the shell, so `$$` *is* the getty process tree; killing it makes rinit
    # reap the exit and, because getty is `Restart=always`, start it again -
    # the "自动重启" half of the milestone's acceptance test.
    printf 'kill -9 $$\n' >&3
    await_log 20 'getty.service will restart in' || true
    sleep 1
    exec 3>&-
) &
feeder_pid=$!

# The guest never exits on its own (rinit stays PID 1), so a bounded `timeout`
# is the expected outcome. `timeout` returns 124 when it had to stop qemu.
#
# Ninety seconds, not sixty: the feeder types a lot more than it used to (every
# §7.x acceptance step), and the last assertion waits for a *restart* that follows
# the console work. Sixty was enough until §7.8's socket wait pushed the boot's
# tail - "getty.service will restart in 2.157s" was scheduled and then qemu was
# killed before it happened.
set +e
timeout -k 5 "${timeout_s}" \
    "${repo_root}/scripts/run-qemu.sh" \
    --arch "${arch}" --kernel "${kernel}" --initrd "${initrd}" \
    ${service:+--service "${service}"} \
    --kernel-args "${kernel_args}" <"${fifo}" >"${log_file}" 2>&1
qemu_rc=$?
wait "${feeder_pid}" 2>/dev/null || true
set -e

# 124 = we killed it (expected). Anything else means qemu died early.
if [[ ${qemu_rc} -ne 124 && ${qemu_rc} -ne 0 ]]; then
    echo "FAIL: qemu exited with ${qemu_rc} before the assertions could be checked" >&2
    tail -20 "${log_file}" >&2
    exit 3
fi

failures=0
pass() { printf '  ok   %s\n' "$1"; }
fail() {
    printf '  FAIL %s\n' "$1" >&2
    failures=$((failures + 1))
}

# expect <description> <fixed-string>
expect() {
    if grep -qF -- "$2" "${log_file}"; then
        pass "$1"
    else
        fail "$1 (missing: '$2')"
    fi
}

# The M0/M1 acceptance list: boots as PID 1, mounts the first wave, resolves
# the unit directory into a dependency-ordered schedule, and starts it.
expect "kernel executed rinit as init" "Run /sbin/rinit as init process"
expect "rinit started as PID 1" "starting (pid 1)"
expect "mounted /run" "rinit: mounted /run"
expect "mounted /proc" "rinit: mounted /proc"
expect "mounted /sys" "rinit: mounted /sys"
expect "mounted /dev" "rinit: mounted /dev"
expect "mounted /tmp" "rinit: mounted /tmp"

# M0's second wave (plan §1.3(9)): `/etc/fstab`, mounted after the first wave, and
# the filesystem there afterwards - `/proc/mounts` is the kernel's answer.
expect "rinit mounted an /etc/fstab entry" "rinit: mounted /data (fstab)"
expect "and the kernel has a filesystem on it" "fstab-tmpfs /data tmpfs"
# The device image gets its PTY mountpoint from this same table. Checking it in
# the initramfs proves the shipped fstab line is readable and actually mounted,
# not just present in the rootfs tree.
expect "rinit mounted devpts from /etc/fstab" "rinit: mounted /dev/pts (fstab)"
expect "and the kernel has devpts on /dev/pts" "fstab-devpts /dev/pts devpts"

# M1: a real boot comes from the unit directory, not a hardcoded service.
# Plan §4.3: how long the boot took, measured on the kernel's own clock - the
# same one the `[...] Run /sbin/rinit as init process` line is printed on, which
# is what makes "kernel start to first service" a number rather than a feeling.
expect "rinit reported the boot timing" "rinit: boot timing: first service at"
timing="$(grep -oE 'first service at [0-9]+ms' "${log_file}" | tail -1 | grep -oE '[0-9]+')"
rinit_ms="$(grep -oE "rinit's own boot: [0-9]+ms" "${log_file}" | tail -1 | grep -oE '[0-9]+')"
# A sanity check on the measurement, not a performance target: the plan names no
# budget, and a guest under whatever load the host is carrying is the wrong place
# to invent one. What this does catch is a clock that is not the kernel's - the
# first service cannot have started before rinit itself did - and a number several
# orders of magnitude out.
if [[ -n "${timing}" && -n "${rinit_ms}" && "${timing}" -ge "${rinit_ms}" && "${timing}" -lt 30000 ]]; then
    pass "the boot timing is the kernel's own clock (first service at ${timing}ms, ${rinit_ms}ms of it rinit's)"
else
    fail "the boot timing is the kernel's own clock (first '${timing:-missing}'ms, rinit '${rinit_ms:-missing}'ms)"
fi
expect "the init anchor is scheduled" "plan 0: rinit-init.target"
expect "the console getty is scheduled" "getty.service"
expect "the DHCP client is scheduled" "network.service"

# Boot must be ordered: mounts before any unit is started, and levels in
# dependency order.
line_of() { grep -n -m1 -F -- "$1" "${log_file}" | cut -d: -f1; }
require_order() {
    local desc="$1" first="$2" second="$3"
    local a b
    a="$(line_of "${first}")"
    b="$(line_of "${second}")"
    if [[ -n "${a}" && -n "${b}" && "${a}" -lt "${b}" ]]; then
        pass "${desc}"
    else
        fail "${desc} ('${first}' at ${a:-missing}, '${second}' at ${b:-missing})"
    fi
}
require_order "mounts complete before any unit starts" \
    "rinit: mounted /tmp" "starting level 0"
require_order "startup levels run in dependency order" \
    "starting level 0" "starting level 1"

# getty owns the console: it must actually be up and still running when the
# boot sequence finishes.
if grep -qF "started getty.service as pid" "${log_file}"; then
    pass "getty started as a process"
else
    fail "getty.service should have been started"
fi

# The boot closure is the console and the DHCP client. `units/` is the boot set
# - the two targets, getty and network - and the §7.x acceptance units live in
# `units/demo/`, which this image carries but the boot target does not reference.
# The count is exact on purpose: adding a unit to the closure is a design change,
# and this is where it should be noticed rather than in a boot log nobody reads.
boot_line="$(grep -oE 'boot complete: [0-9]+ unit\(s\) started' "${log_file}" | head -1)"
if [[ "${boot_line}" == "boot complete: 2 unit(s) started" ]]; then
    pass "the boot started exactly the console and the DHCP client"
else
    fail "boot should have started exactly 2 units (got: '${boot_line:-missing}')"
fi

# Logging in must work with the root password from /etc/shadow. `uid=0(root)`
# is the proof that the whole path - getty, login, shadow - actually worked.
expect "getty presents a login prompt" "login:"
expect "login succeeded and gave a root shell" "uid=0(root)"

# The DHCP client is a supervised rinit unit, and qemu's user-mode network is
# what answers it: the address, the route and the resolver are all consequences
# of `units/network.service`, not of anything the test typed.
expect "network.service is active" "net-state: active"
expect "the guest got its address from DHCP" "inet 10.0.2.15/24"
expect "the DHCP default route is installed" "default via 10.0.2.2"
expect "the DHCP DNS server reached /etc/resolv.conf" "nameserver 10.0.2.3"
expect "the default target is active" "boot-target-state: active"

# M3 acceptance (plan §3): the guest's own `rctl` drives the running system and
# reads its logs back. `list`/`status`/`start`/`log`/`stop`/`reset-failed` are
# all exercised from the console above; the assertions check the output each one
# is the only possible source of.
expect "rctl list reached the control socket" "logdemo.service: inactive"
expect "rctl status described the console unit" "target: started"
expect "rctl started a unit on demand" "started logdemo.service"
expect "rctl log returned captured service output" "rinit-log-marker"
expect "rctl stop stopped a unit" "stopped logdemo.service"
expect "rctl reset-failed cleared the failed unit" "fault.service: cleared"

# M5 §7.4 acceptance: the privilege drop. `priv.service` runs
# `/bin/sh -c "echo uid=$(id -u) gid=$(id -g) nnp=..."` as `User=nobody`,
# `Group=nogroup`, with `NoNewPrivileges=true`, so the exact line proves three
# things at once: the process became nobody, its group became nogroup's, and
# the no-new-privileges flag was applied. rinit runs as root in the guest, so
# this is the case the host cannot test at all.
expect "a de-privileged service ran as nobody:nogroup with NoNewPrivs" \
    "uid=65534 gid=65534 nnp=1"

# M5 §7.4 acceptance: capabilities. This is the strictest setting in the plan
# (an empty `CapabilityBoundingSet=`) on the hardest subject: a service that is
# still **root**. A root process is normally handed every capability at `exec` -
# the kernel builds that set as `bset | inheritable` - so `eff=0` here is only
# possible because rinit dropped the bounding set before the exec.
expect "an empty CapabilityBoundingSet left a root service with no capabilities" \
    "uid=0 bnd=0000000000000000 eff=0000000000000000"

# The other half of the capability story: the one capability asked for is the
# one present, in all three sets. NET_BIND_SERVICE is bit 10, so 0x400.
# `amb`/`eff` are what a non-root service cannot get any other way, and `nobody`
# holding exactly one bit is the least-privilege shape §7.4 is for.
expect "ambient capabilities reached a de-privileged service" \
    "uid=65534 amb=0000000000000400 eff=0000000000000400 bnd=0000000000000400"

# M5 §7.3 acceptance: cgroup v2. The hierarchy is mounted by rinit's own first
# wave, then brought up (controllers enabled, the `rinit/` group created) before
# any unit starts, which is what makes a unit's attributes exist at all.
expect "rinit mounted the cgroup v2 hierarchy" "rinit: mounted /sys/fs/cgroup"
expect "the cgroup v2 hierarchy was brought up" "cgroup v2 ready at /sys/fs/cgroup"

# What the *kernel* says about a limited unit, read back by the unit itself.
# `self=` is the service process's own `/proc/<pid>/cgroup`; `0::` is the v2
# unified-hierarchy marker, and `/rinit/cglimit.service` is the placement §7.3
# asks for.
expect "a limited unit was placed in its own cgroup" \
    "self=0::/rinit/cglimit.service"
# A process the service forked is in the same cgroup, inherited by the kernel
# without rinit tracking anything.
expect "a process the service forked inherited its cgroup" \
    "child=0::/rinit/cglimit.service"
# `CgroupCPUMax=50%` against the kernel's 100000us period.
expect "the CPU quota was converted from a percentage" "cpu=50000 100000"
# Bytes, not the `32M` spelling the unit file uses.
expect "the memory hard and soft limits were written in bytes" \
    "mem=33554432 high=25165824"
expect "the task limit was applied to the cgroup" "pids=64"

# The live half: a running unit's cgroup holds a process, and after the stop the
# directory is gone. Both are read from the guest's own view of cgroupfs, and
# neither can be faked by a host test - an ordinary directory of stand-in
# attribute files is never empty enough to `rmdir`.
expect "a running unit was inside its cgroup" "cgrun-populated"
expect "a stopped unit's cgroup was removed" "cgrun-gone"
expect "a finished oneshot's cgroup was removed" "cglimit-gone"

# M5 §7.3's OOM link. The first line is the kernel's own record of a kill it
# performed *in this cgroup* (plan §7.3: "内核 OOM-killer 在 cgroup 内挑选进程杀,
# 不影响整机"); the second is rinit reading `memory.events`' counter and naming
# the limit, which is what a bare `SIGKILL` cannot say; the third is the restart
# the plan's "检测到 OOM kill 后进入重启策略" asks for.
expect "the kernel OOM-killed a process inside the unit's cgroup" \
    "Memory cgroup out of memory: Killed process"
expect "rinit named the OOM kill and the memory limit behind it" \
    "killed by the cgroup OOM killer (memory.max=16777216)"
expect "an OOM-killed unit entered the restart policy" "oom.service will restart in"

# M5 §7.5: the hardware watchdog, on the real device the guest was given.
expect "rinit armed the guest's watchdog device" \
    "rinit: watchdog /dev/watchdog armed: 10s timeout, feeding every 5s"
expect "the guest's watchdog is the qemu device rinit opened" "wd-ident=i6300ESB timer"
expect "the driver is running the device rinit armed" "wd-state=active"
expect "the driver took the timeout rinit set" "wd-timeout=10"

# The device was armed within seconds of boot with a 10 s timeout, and this
# guest is long past that: a single missed feed would have reset the machine, so
# the uptime *is* the evidence that the supervisor loop is feeding it. The boot
# count is the same claim from the other side - a reset would leave a second
# "Run /sbin/rinit as init process" in the log.
uptime="$(grep -oE 'wd-uptime=[0-9]+' "${log_file}" | tail -1 | cut -d= -f2)"
if [[ -n "${uptime}" && "${uptime}" -gt 10 ]]; then
    pass "the guest outlived the watchdog timeout with the device armed"
else
    fail "the guest's uptime past the watchdog timeout (got: '${uptime:-missing}')"
fi
boots="$(grep -c -F "Run /sbin/rinit as init process" "${log_file}" || true)"
if [[ "${boots}" -eq 1 ]]; then
    pass "the guest booted exactly once (the watchdog never fired)"
else
    fail "the guest booted ${boots} times: the watchdog reset it"
fi

# M5 §7.6: reload, against a live system. The console edits the unit directory
# as it goes, so these are about what rinit did to a running system rather than
# about what a manager would do in a unit test.
expect "rctl reload reached the control socket" "control: reload ok"
expect "a reload that changed nothing reported exactly that" "reload: 0 added (0 started)"
expect "an added unit was registered" "registered reload.service"
expect "an added unit the boot target does not need was not started" "1 added (0 started)"
expect "a changed unit that is not running took the new definition at once" \
    "logdemo.service: new definition in force"
expect "a changed unit that is running kept it for the next start" \
    "getty.service is running; its new definition applies at the next start"
expect "a removed unit was removed" "removed reload.service"
expect "a removed unit is gone from rctl" "no such unit: reload.service"
expect "a directory that does not resolve was refused whole" \
    "reload rejected, the running configuration is unchanged"

# Plan §0.1's memory column, measured in the guest rather than in a host
# `--dev` run: `VmRSS` out of `/proc/1/status`, at boot and with 20 services.
rss_kb() { grep -oE "$1-VmRSS: *[0-9]+" "${log_file}" | tail -1 | grep -oE '[0-9]+' || true; }
idle_rss="$(rss_kb idle)"
load_rss="$(rss_kb load)"
if [[ -n "${idle_rss}" && "${idle_rss}" -le 2048 ]]; then
    pass "rinit's RSS at boot is within 2 MB (${idle_rss} kB)"
else
    fail "rinit's RSS at boot is within 2 MB (got: '${idle_rss:-missing}' kB)"
fi
if [[ -n "${load_rss}" && "${load_rss}" -le 6144 ]]; then
    pass "rinit's RSS with 20 services is within 6 MB (${load_rss} kB)"
else
    fail "rinit's RSS with 20 services is within 6 MB (got: '${load_rss:-missing}' kB)"
fi
par_units="$(grep -c -E 'started load[0-9]+\.service as pid' "${log_file}" || true)"
if [[ "${par_units}" -eq 20 ]]; then
    pass "all 20 services were running when the second number was taken"
else
    fail "only ${par_units} of the 20 services started"
fi

# M5 §7.10: `Type=forking`, on a daemon that really detaches. The last one is
# the assertion for the whole feature: the unit's *process* is the daemon, so the
# pid `rctl status` reports has to be the pid the `PIDFile=` names - and that pid
# has to be the one the stop reaches, even though it leads no process group.
expect "a forking unit's main process exited and its daemon was located" \
    "forking.service: main process exited; daemon is pid"
daemon="$(grep -oE 'forking-daemon=[0-9]+' "${log_file}" | tail -1 | cut -d= -f2)"
if [[ -n "${daemon}" ]] && grep -qF "state: active (pid ${daemon})" "${log_file}"; then
    pass "the unit is active with the daemon the PIDFile named (pid ${daemon})"
else
    fail "the unit's process is the daemon the PIDFile named (got: '${daemon:-missing}')"
fi
expect "a forking unit whose PIDFile was never written failed its start" \
    "cannot locate the daemon: cannot read PIDFile /run/not-written.pid"
expect "and the failed forking unit is not active" "bf-state=state: failed"
expect "stopping a forking unit stopped the daemon it took over" "daemon-gone"

# M5 §7.11: a `TTYPath=` that is not a terminal is a failed start, not a service
# that believes it owns a terminal.
expect "a TTYPath that is not a terminal fails the start" "ioctl(TIOCSCTTY) failed"

# M5 §7.9: the socket rinit binds is a real one, the unit behind it is not started
# until something asks, and a start hands over the listening socket on fd 3.
expect "rinit bound the socket before any unit started" \
    "api.service: listening on /run/api.sock"
expect "a socket-activated unit is listed but not started" "api.service: inactive"
expect "the socket is a real one on the filesystem" "api-socket-bound"
expect "and the unit is inactive behind it" "api-state=state: inactive"
expect "starting it hands the service the listening socket" "activated-fd-3"

# M5 §7.8: the socket directives. The consumer could only say `socket-present`
# after waiting for the path, and the unit whose path never appears fails its
# start rather than running against nothing.
expect "a socket consumer waited for the path before starting" "socket-present"
expect "a socket that never appears fails the start, naming it" \
    "NeedsSocket=/run/never.sock did not appear in time"
expect "and that failed start is reported as one" "waitsock.service failed to start"

# M5 §7.7: a template is a name to instantiate, and an instance is a unit with
# the specifiers expanded into the command it runs.
expect "a template is not a unit to start" \
    "worker@.service is a template; start an instance"
expect "an instance was created from its template" "started worker@42.service"
expect "every specifier was expanded before the command ran" \
    "worker-instance=42 name=worker@42.service prefix=worker env=42"

# Plan §1.3(7): the default log file exists and holds what a service wrote.
log_lines="$(grep -oE 'logfile-marker=[0-9]+' "${log_file}" | tail -1 | cut -d= -f2)"
if [[ -n "${log_lines}" && "${log_lines}" -ge 1 ]]; then
    pass "the default log file has a service's output (${log_lines} line(s))"
else
    fail "the default log file has a service's output (got: '${log_lines:-missing}')"
fi

# A completed oneshot has nothing to restart, so `restart` is a conflict
# (exit 4). `logdemo` proves it and that the unit is only started once.
if grep -qF "logdemo.service has no process to restart" "${log_file}"; then
    pass "rctl reported a restart conflict"
else
    fail "rctl restart of a completed oneshot should be a conflict"
fi
# An unrelated unit must not be pulled into the boot (plan §1.3(1)).
if [[ "$(grep -cF 'started logdemo.service as pid' "${log_file}")" -eq 1 ]]; then
    pass "logdemo.service only started because rctl asked"
else
    fail "logdemo.service must not be started at boot"
fi

# M2 acceptance (plan §3): kill a service and watch the manager restart it. The# console shell *is* the getty process tree, so `kill -9 $$` is a real crash of
# getty.service; a `Restart=always` unit must come back.
expect "a crashed service is scheduled for restart" "getty.service will restart in"
if [[ "$(grep -cF 'started getty.service as pid' "${log_file}")" -ge 2 ]]; then
    pass "the crashed console service was restarted"
else
    fail "getty.service must be restarted after it is killed"
fi
# A single crash is a restart, not a meltdown: the breaker must not have tripped.
if grep -qF "getty.service hit its start limit" "${log_file}"; then
    fail "a single crash must not trip getty's start limit"
else
    pass "one crash did not trip the breaker"
fi

# M2 acceptance: the breaker does trip when a service fails in a burst. The
# fault-injection unit fails immediately and twice, which exhausts its burst of
# two and parks it in Failed(StartLimitHit) instead of restarting forever.
expect "a repeatedly failing unit is retried at least once" "fault.service will restart in"
if grep -qE "error: fault.service hit its start limit \(2 failures\)" "${log_file}"; then
    pass "the breaker tripped on the failing unit"
else
    fail "fault.service should have tripped its start limit after two failures"
fi

# Plan §7.2: a target is an aggregation point with no process, so it must
# never appear in the "started ... as pid" stream.
if grep -qE "started (rinit-init|multi-user)\.target as pid" "${log_file}"; then
    fail "a target must never be exec'd (plan §7.2)"
else
    pass "no target was exec'd"
fi

# shell.service is the login-free alternative and must stay unstarted: it would
# fight getty for ttyAMA0.
if grep -qF "started shell.service" "${log_file}"; then
    fail "shell.service must not start while getty holds ttyAMA0"
else
    pass "shell.service correctly not started alongside getty"
fi

expect "boot completed" "boot complete"

# The panic hook must not have fired, and the kernel must not have died.
if grep -qF "rinit: PANIC" "${log_file}"; then
    fail "rinit reported a panic"
    grep -F "rinit: PANIC" "${log_file}" | head -5 >&2
else
    pass "no rinit panic"
fi

for pattern in "Kernel panic" "Internal error: Oops" "BUG: " "not syncing"; do
    if grep -qF -- "${pattern}" "${log_file}"; then
        fail "kernel log contains '${pattern}'"
        grep -F -- "${pattern}" "${log_file}" | head -3 >&2
    else
        pass "kernel log clean of '${pattern}'"
    fi
done

echo
if [[ ${failures} -eq 0 ]]; then
    echo "PASS: all assertions held on ${arch}"
    if [[ ${keep_log} -eq 1 ]]; then
        echo "log: ${log_file}"
    else
        # The log only matters when something failed; --keep-log asks for it
        # either way.
        rm -f "${log_file}"
    fi
    exit 0
fi

echo "FAIL: ${failures} assertion(s) failed on ${arch}" >&2
echo "full log: ${log_file}" >&2
exit 1
