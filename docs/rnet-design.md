# rnet: network manager and rncli

Status: implemented through P4; P5 release gates are tracked in
[`docs/release.md`](release.md). The `a62642a` 24 h runner completed, but its
post-run analyzer rejected invalid FD samples produced by the old sampler. The
sampler and tool are fixed and a fresh 24 h gate run is required.

This document is the design and task breakdown for `rnet`, a network manager
for rinit systems, and `rncli`, its control CLI. It records the decisions
already made and the work still required.

## 1. Decisions

| area | decision |
| --- | --- |
| Architecture | A separate `rnetd` daemon supervised by rinit, plus an `rncli` client |
| Configuration | One strict INI-style file: `/etc/rnet/config` |
| Netlink | Native netlink implementation through `libc`; no `rtnetlink` crate |
| DHCP | **Self-developed DHCPv4 client**; `udhcpc` is no longer the default path |
| DNS | `rnet` owns `/etc/resolv.conf` when `manage-resolv-conf = yes` |
| Persistence | Runtime mutations in v1; `rncli --persist` drop-ins in v1.1 |
| Boot integration | Keep the `network.service` unit name; replace its `ExecStart` with `rnetd` |
| Control protocol | Reuse rinit's 4-byte length framing with a text payload; no serde/JSON |
| v1 scope | Ethernet, static IPv4, DHCPv4, routes, bridges, DNS, hotplug, reload |
| Later scope | IPv6/RA/DHCPv6, VLAN, bonding, policy routing, Wi-Fi, PPPoE |

The self-developed DHCPv4 client is the largest single risk. It is deliberate:
the production image must not depend on BusyBox `udhcpc` for its primary
network configuration.

## 2. Goals and non-goals

### Goals

- One manager for link state, addresses, routes, bridges and DHCPv4.
- Declarative configuration with a strict validator, so a typo is an error
  rather than a half-configured interface.
- Dynamic operation: link hotplug, DHCP renewal/rebind, configuration reload.
- A CLI that can inspect and modify runtime state, with an explicit
  persistence path.
- No new Cargo dependencies and no runtime dependency on BusyBox network
  applets.
- Keep PID 1 out of the network event loop. A network bug must not be able to
  hang or crash rinit.
- Bounded memory and file descriptors; malformed netlink/DHCP/config input
  must never panic.

### Non-goals for v1

- Firewall/NAT (`nftables`), DNS server, VPN, traffic shaping.
- Wi-Fi (`wpa_supplicant`), PPPoE.
- Network namespaces.
- VLAN, bonding, bridge VLAN filtering.
- IPv6 in any form: static addresses, routes, router advertisements and
  DHCPv6. v1 rejects IPv6 literals in `address`, `gateway`, `dns`, `route`
  and `src`; IPv6 support is a later change, not a hidden fallback.
- Policy routing beyond the `main` table.

## 3. Architecture

```text
rinit (PID 1)
  └─ network.service  ->  rnetd --config /etc/rnet/config
        ├─ config parser and validator
        ├─ desired-state model and diff
        ├─ netlink backend (link / address / route / bridge)
        ├─ DHCPv4 client (self-developed)
        ├─ resolv.conf writer
        └─ /run/rnet/control.sock
                 ▲
              rncli
```

- `rnetd` is a normal rinit service. `Restart=on-failure` is what recovers it.
- `rnetd` is single-threaded and uses `poll(2)` for the netlink socket, DHCP
  sockets, the control listener and signal delivery. It does not need `tokio`.
- `rncli` never writes the kernel directly. Every mutation goes through the
  control socket so there is one writer.
- `network-online.service` becomes a `rncli wait-online` client. The client
  polls `status`; the daemon never blocks its event loop waiting for a lease.

### Crate layout

```text
crates/rnet/
  Cargo.toml
  src/lib.rs
  src/config.rs          # parser, schema, validation
  src/model.rs           # Config, Interface, Address, Route, Bridge
  src/state.rs           # desired/observed state, diff, reload
  src/control.rs         # rnet control protocol codec
  src/dns.rs             # resolv.conf generation
  src/cli.rs             # rncli command parsing and rendering
  src/runtime.rs         # rnetd poll loop, control server, DHCP actions
  src/netlink/mod.rs     # netlink socket/attributes/ACK, writes and observation
  src/netlink/event.rs   # RTNLGRP_LINK subscription
  src/dhcp/mod.rs        # module wiring
  src/dhcp/packet.rs     # DHCPv4 packet/option codec
  src/dhcp/lease.rs      # lease model and /run/rnet/leases
  src/dhcp/client.rs     # state machine and timers
  src/dhcp/socket.rs     # UDP transport
  src/bin/rnetd.rs
  src/bin/rncli.rs
```

`rnet` depends on `libc` only. It does not depend on `tokio`, `serde`, `toml`,
`rtnetlink` or `rinit-core`. Most `unsafe` code is in `src/netlink/` and the
DHCP socket setup; small process-level boundaries in `src/lib.rs` and
`src/runtime.rs` restore SIGPIPE, poll descriptors, create signalfd and change
the control-socket owner. All of these boundaries are documented and audited
like the `rinit-runtime` syscall layer.

## 4. Configuration

### 4.1 File and syntax

Default path: `/etc/rnet/config`, overridable with `rnetd --config` and
`rncli --config`.

- INI-style sections: `[global]`, `[interface NAME]`, `[route NAME]`.
- `key = value`, one per line. `#` starts a comment. A line ending in `\`
  continues on the next line.
- Values are tokenised by whitespace; a list is a single value such as
  `ports = eth1 eth2`. No shell and no environment expansion.
- Unknown section, unknown key, duplicate singleton key, duplicate section and
  invalid value are errors. A file that does not validate is never applied.
- Bounds: file at most 1 MiB; line at most 4 KiB; at most 256 interfaces, 1024
  routes, 64 addresses per interface, 16 DNS servers per interface, 64 bridge
  ports.
- Interface names must fit Linux `IFNAMSIZ` and may not contain `/`, `.` or
  `..`.

### 4.2 Schema

#### `[global]`

| key | value | default | meaning |
| --- | --- | --- | --- |
| `manage-resolv-conf` | `yes`/`no` | `yes` | Write `/etc/resolv.conf` from static and DHCP DNS |
| `manage-loopback` | `yes`/`no` | `yes` | Ensure `lo` is up |
| `wait-online` | `any`/`all`/`none` | `any` | What `rncli wait-online` requires |
| `wait-online-timeout` | seconds | `30` | Default timeout for `wait-online` |
| `dhcp-timeout` | seconds, `0` = forever | `0` | How long a DHCP client keeps retrying |
| `hostname` | text | empty | DHCP option 12, when set |
| `search` | domain, repeatable | empty | Search domains for resolv.conf |
| `dns` | IPv4, repeatable | empty | Global DNS servers |

#### `[interface NAME]`

| key | value | default | meaning |
| --- | --- | --- | --- |
| `type` | `ethernet`/`bridge` | `ethernet` | Device type |
| `enabled` | `yes`/`no` | `yes` | `no` brings the link down and removes managed state |
| `required` | `yes`/`no` | `no` | `yes` makes `wait-online` fail if the link is missing; `rnetd` keeps waiting for hotplug |
| `dhcp4` | `yes`/`no` | `no` | Run the built-in DHCPv4 client |
| `address` | IPv4 CIDR, repeatable | empty | Static IPv4 addresses |
| `gateway` | IPv4 | empty | Default IPv4 route |
| `dns` | IPv4, repeatable | empty | Extra DNS servers for this interface |
| `mtu` | integer | kernel default | Link MTU |
| `ports` | interface names | empty | Bridge ports; bridge only |
| `stp` | `yes`/`no` | `no` | Bridge STP; bridge only |
| `forward-delay` | seconds | `15` | Bridge forwarding delay |
| `hello-time` | seconds | `2` | Bridge hello time |
| `max-age` | seconds | `20` | Bridge max age |
| `wait-online` | `yes`/`no` | follows global | Per-interface wait-online override |

Rules:

- `dhcp4 = yes` and any static `address` or `gateway` are mutually
  exclusive. Static `dns` is allowed and is merged with the lease's DNS.
- v1 is IPv4-only. IPv6 literals in `address`, `gateway`, `dns`, `route` or
  `src` are validation errors, not silently ignored.
- `ports` is valid only on a bridge. A port may not have an address, DHCP or a
  gateway, and may not be listed in two bridges.
- A bridge may have `dhcp4 = yes`; the DHCP client binds to the bridge device.
- `enabled = no` on a bridge removes its ports from the bridge before the
  bridge is brought down.
- `required = yes` makes a missing link fail the `rncli wait-online` check,
  but `rnetd` stays alive and waits for hotplug. The default `no` keeps the
  link optional for `network-online`.

#### `[route NAME]`

| key | value | default | meaning |
| --- | --- | --- | --- |
| `to` | IPv4 CIDR or `default` | required | Destination |
| `via` | IPv4 | empty | Gateway |
| `dev` | interface name | empty | Output device |
| `metric` | integer | `0` | Route metric |
| `table` | `main` or number | `main` | Routing table |
| `src` | IPv4 | empty | Preferred source |
| `onlink` | `yes`/`no` | `no` | Treat the gateway as on-link |

A route must have at least one of `via` or `dev`. v1 supports IPv4 unicast
routes only.

### 4.3 Example

```ini
[global]
manage-resolv-conf = yes
manage-loopback = yes
wait-online = any
wait-online-timeout = 30
search = lan

[interface eth0]
type = ethernet
enabled = yes
dhcp4 = yes

[interface eth1]
type = ethernet
enabled = yes

[interface eth2]
type = ethernet
enabled = yes

[interface br0]
type = bridge
enabled = yes
ports = eth1 eth2
stp = yes
forward-delay = 2
address = 192.168.10.2/24
gateway = 192.168.10.1
dns = 192.168.10.1
mtu = 1500

[route lab]
to = 10.20.0.0/16
via = 192.168.10.1
dev = br0
metric = 100
```

Unlisted interfaces are untouched. `lo` is brought up by default but is not
listed unless the operator wants to manage it.

## 5. Runtime model

### 5.1 Desired, observed, diff

`rnetd` parses the config into a desired state, dumps the kernel state through
netlink, computes a diff, and applies it in this order:

1. Create bridges and other virtual devices (v2 for VLAN/bond).
2. Bring bridge ports up and enslave them.
3. Set MTU and link up/down.
4. Add/remove addresses.
5. Add/remove routes.
6. Start/stop DHCP clients.
7. Regenerate `resolv.conf`.

Every netlink operation is idempotent: `EEXIST` on add and `ENOENT` on delete
are treated as success.

### 5.2 Ownership and cleanup

- An interface is managed only if it has an `[interface]` section. Everything
  else is left alone.
- For a managed interface, `rnet` owns the addresses and routes it applies. On
  reload it removes managed state that is no longer desired.
- The process records applied addresses, routes, bridges and DHCP leases in
  `/run/rnet/state` and `/run/rnet/leases/`. The state file is not the source
  of truth; it exists so a restarted `rnetd` can clean up after itself.
- A bridge is deleted on reload only if `rnet` created it and it was in the
  previous configuration. A pre-existing bridge with the same name is reported
  and left alone.
- Physical links are never deleted. `enabled = no` only brings them down and
  removes managed addresses/routes.

### 5.3 Reload

`rncli reload` (or `SIGHUP`) makes `rnetd` parse and validate the new file
completely before touching the kernel. If validation fails, the old
configuration stays in force and the error is returned. If validation
succeeds, the diff is applied; a netlink failure is reported per object and
the daemon keeps the new desired state so the next reload can retry.

### 5.4 Hotplug and missing links

`rnetd` subscribes to `RTNLGRP_LINK`. When a link that appears in the config
shows up, its configuration is applied without a reload; when it disappears,
its runtime status is marked down and DHCP is stopped. If the kernel/container
does not provide the event group, it falls back to a one-second link-set
comparison.

A missing `required = yes` link makes `rncli wait-online` fail after its
timeout; `rnetd` itself stays active and keeps watching for hotplug. A missing
`required = no` link is only logged.

## 6. Self-developed DHCPv4

This is the part that replaces `udhcpc`. It is a client only; `rnetd` remains
the only writer of addresses, routes and DNS.

### 6.1 Transport

- One UDP socket per DHCP interface, bound to `0.0.0.0:68`.
- `SO_BINDTODEVICE` to the interface, `SO_BROADCAST`, `SO_REUSEADDR`, and
  `IP_PKTINFO` for receiving.
- The `BROADCAST` flag is set while the client has no address, so OFFER/ACK
  can be received before an address is configured.
- `CAP_NET_BIND_SERVICE` is needed for port 68; `CAP_NET_RAW` is needed for
  `SO_BINDTODEVICE`.
- Requests use a random transaction id from `getrandom(2)` and a client id of
  type 1 plus the interface MAC.
- Multiple DHCP interfaces are supported by the model. The implementation
  must verify Linux UDP demultiplexing with `SO_BINDTODEVICE` +
  `SO_REUSEADDR`; if one socket per interface is ambiguous, use one receive
  socket with `IP_PKTINFO` plus per-interface send sockets. That is an
  implementation detail and does not change the configuration.

### 6.2 Packet and options

The codec covers the fixed BOOTP header, the magic cookie and the options
needed by the client:

| option | use |
| --- | --- |
| 53 | DHCP message type |
| 54 | server identifier |
| 51 | lease time |
| 58 / 59 | T1 / T2 |
| 1 | subnet mask |
| 3 | router |
| 6 | DNS servers |
| 15 | domain name |
| 26 | MTU |
| 33 / 121 | static/classless routes |
| 12 | hostname |
| 55 | parameter request list |
| 57 | maximum message size |
| 61 | client identifier |

Parsing is length-checked and bounded. Malformed options, truncated packets,
a wrong transaction id, a wrong `chaddr`, or an OFFER/ACK for a different
interface are discarded. Option 121 is preferred over option 33 when both are
present, per RFC 3442.

### 6.3 State machine

| state | action | transition |
| --- | --- | --- |
| `INIT` | Send DISCOVER; retry 4/8/16/32/64 s with jitter | OFFER -> `REQUESTING` |
| `SELECTING` | Wait for OFFER; validate | timeout -> retry DISCOVER |
| `REQUESTING` | Send REQUEST with requested IP and server id | ACK -> `BOUND`; NAK -> `INIT` |
| `BOUND` | Apply lease; schedule T1/T2 | T1 -> `RENEWING`; expiry -> `INIT` |
| `RENEWING` | Unicast REQUEST to the server | ACK -> `BOUND`; NAK -> `INIT`; T2 -> `REBINDING` |
| `REBINDING` | Broadcast REQUEST | ACK -> `BOUND`; expiry -> deconfigure, `INIT` |

- T1 and T2 default to 0.5 and 0.875 of the lease, unless the server supplies
  options 58/59.
- `dhcp-timeout = 0` retries forever. A non-zero value stops the client after
  that many seconds and reports a failed lease.
- `rncli dhcp release` sends RELEASE, removes the lease's address/routes/DNS,
  and returns the interface to `INIT`.
- If address installation fails with `EEXIST`, send DECLINE and restart
  SELECTING. v1 does not perform an ARP probe before requesting; that is a v2
  hardening item.

### 6.4 Lease application

On ACK, apply in this order:

1. Remove the previous lease's managed address, routes and DNS.
2. Add the leased address with the lease prefix length.
3. Add the default route from option 3, unless a static gateway is configured.
4. Add classless static routes from option 121 (or option 33).
5. Set the interface MTU from option 26 when present and no static MTU is set.
6. Merge DNS from option 6 and the domain from option 15.
7. Write the lease to `/run/rnet/leases/<iface>` atomically.
8. Regenerate `resolv.conf`.

On deconfigure/expiry, remove the managed address, routes and DNS, then go back
to `INIT`.

### 6.5 Lease persistence

`/run/rnet/leases/<iface>` is a strict key=value file:

```text
address=192.168.1.50
prefix=24
server=192.168.1.1
lease=3600
t1=1800
t2=3150
router=192.168.1.1
dns=192.168.1.1 9.9.9.9
domain=lan
```

It is written through a temporary file and `rename`. It is diagnostic and
restart support, not the source of truth. v1 may always start with DISCOVER;
reusing an unexpired lease on restart is a v1.1 optimization.

### 6.6 Security and limits

- No authentication is possible in DHCP; the client only accepts replies on
  the bound interface and validates transaction id and client hardware address.
- The packet parser never allocates based on an option length without a bound.
- Log lines are rate-limited per interface/state.
- The client never invokes a shell and never executes a lease script.
- Maximum lease options, routes and DNS entries are bounded.

## 7. Netlink

The netlink module owns the kernel boundary. It uses `AF_NETLINK` /
`NETLINK_ROUTE` with sequence numbers, `NLM_F_ACK`, multipart dumps and
`NLMSG_ERROR` handling.

Operations needed for v1:

| object | messages |
| --- | --- |
| link | `RTM_GETLINK`, `RTM_NEWLINK`, `RTM_SETLINK` |
| address | `RTM_GETADDR`, `RTM_NEWADDR`, `RTM_DELADDR` |
| route | `RTM_GETROUTE`, `RTM_NEWROUTE`, `RTM_DELROUTE` |
| bridge | `RTM_NEWLINK` with `IFLA_LINKINFO`/`IFLA_INFO_KIND=bridge`; `RTM_SETLINK` with `IFLA_MASTER` for ports |
| events | `RTNLGRP_LINK` subscription; one-second link-set comparison if the event socket is unavailable |

Error handling maps the negative errno in `NLMSG_ERROR` to a message naming
the object and operation, for example:

```text
eth0: add 192.168.10.2/24: file exists
```

Receive buffers and attribute lengths are bounded. The encoder/decoder is
pure and covered by fixture tests.

## 8. DNS and resolv.conf

When `manage-resolv-conf = yes`, `rnetd` writes `/etc/resolv.conf` atomically
with a generated-file header:

```text
# Generated by rnet; do not edit.
search lan
nameserver 192.168.10.1
```

Merge order:

1. `[global] search` and `[global] dns`.
2. Static per-interface `dns`.
3. DHCP DNS and domain, in interface order.

Duplicates are removed while preserving order. If `/etc/resolv.conf` is a
symlink, `rnetd` refuses to replace it and logs an error; the operator can
disable management or point the symlink at a file rnet is allowed to own. This
avoids silently breaking a distro-managed resolver.

## 9. Control protocol and rncli

### 9.1 Socket and framing

- Socket: `/run/rnet/control.sock`, mode `0660`, owner `root:rnet`; the image
  creates the `rnet` group with gid 1000.
- Framing: the same 4-byte little-endian length prefix as the rinit control
  protocol, with a 64 KiB maximum body.
- Body: UTF-8 text, one `key=value` per line. The first line is the protocol
  version and operation; the response starts with `ok` or `err <code> <text>`.
  Repeated keys represent lists.
- No JSON and no serde. `rncli --porcelain` is the machine-readable form.

### 9.2 Commands

```text
rncli status [IFACE]
rncli show IFACE
rncli config check [FILE]
rncli config show
rncli reload
rncli wait-online [--timeout N]
rncli iface up|down IFACE
rncli addr add|del IFACE CIDR
rncli route add|del ...
rncli bridge create|delete|add-port|del-port ...
rncli dhcp renew|release IFACE
```

- `status` prints a table; `show` prints one interface in detail; `--porcelain`
  prints key=value lines.
- `wait-online` polls `status` every 500 ms and exits when the configured
  policy is satisfied. The server never blocks.
- `wait-online = any` succeeds when at least one managed, non-loopback
  interface with `wait-online` enabled has an address (and a default route
  when one is configured). `all` requires every such interface. `none`
  succeeds immediately. A missing `required = yes` interface makes the check
  fail at the timeout.
- Mutations are runtime-only in v1. v1.1 adds `--persist`, which writes a
  machine-owned drop-in `/etc/rnet/config.d/90-rncli.conf` and then reloads.
  The drop-in replaces the whole named section; it never rewrites the
  hand-edited main file.
- Exit codes: 0 success, 1 runtime failure, 2 usage/config error, 3 conflict
  or missing object.

## 10. rinit integration

Keep the `network.service` unit name so the boot graph and existing tests do
not move:

```ini
[Unit]
Description=Network manager
Requires=rinit-init.target
After=rinit-init.target

[Service]
Type=notify
ExecStart=/sbin/rnetd --config /etc/rnet/config
Restart=on-failure
RestartSec=2
TimeoutStartSec=30
NoNewPrivileges=true
CapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_RAW CAP_NET_BIND_SERVICE CAP_CHOWN
```

- `rnetd` sends `READY=1` after the initial desired state is applied and DHCP
  clients are running.
- `network-online.service` uses `rncli wait-online --timeout 30` instead of a
  shell loop. It declares `After=network.service`; the `Before=` is deliberately
  absent because the demo image does not ship `network-online` and rinit would
  reject a reference to a missing unit.
- `hostname.service` keeps `Before=network.service`.
- `multi-user.target` keeps `Wants=network.service`.
- The old `/etc/udhcpc/default.script` is no longer used by the default unit.
  It may stay in the image for diagnostics but must not write addresses.

### 10.1 Existing tests and scripts to update

- `scripts/system-test.sh`: `network.service` state, address, route and
  `resolv.conf` assertions; add `rncli status` assertions.
- `scripts/soak-guest.sh` and `scripts/soak-analyze.py`: replace the `udhcpc`
  process check with `rnetd` and `rncli` checks; update the CSV header and the
  analyzer together.
- `scripts/device-smoke.sh`: `network-online` remains the observable.
- `scripts/build-initramfs.sh`: install `rnetd`, `rncli`, `/etc/rnet/config`
  and the rnet unit; stop installing the old DHCP lease script as the writer.
- `size-check`: include `rnetd` and `rncli` in the binary-size budget.

### 10.2 Build targets

Add `crates/rnet` to the workspace members. Build `rnetd` and `rncli` as part
of the normal target build, install them into `/sbin` in the image, and add
them to the disk image and initramfs.

## 11. Security

- `rnetd` runs as root with a capability bounding set of `CAP_NET_ADMIN`,
  `CAP_NET_RAW` and `CAP_NET_BIND_SERVICE`, plus `NoNewPrivileges=true`.
- The control socket is `0660 root:rnet`; mutations require root or the `rnet`
  group. Read-only status is available to the same group.
- `/etc/rnet/config` is root-owned; v1 contains no secrets. Future Wi-Fi keys
  must use a `0600` file and a separate secret section.
- All user input is validated before netlink: interface names, CIDRs, route
  gateways, bridge ports, table names/numbers, DNS addresses.
- No shell, no command strings, no `popen`.
- Config, control frames and DHCP packets are length-bounded; parsers are
  fuzzed.
- Logs never print a full DHCP packet or a future secret.

## 12. Testing

### 12.1 Host tests

- Config parser/validator: valid/invalid files, unknown keys, duplicate
  sections, bounds, interface/route/bridge rules.
- Control codec: round-trip, version mismatch, oversized frame, malformed
  key=value, list fields.
- Netlink codec: fixture messages for link/address/route/bridge, attribute
  alignment, `NLMSG_ERROR`, multipart.
- DHCP codec: golden DISCOVER/OFFER/REQUEST/ACK/NAK packets, option 121,
  truncated/malformed options.
- DHCP state machine: fake clock and fake transport covering retry, renew,
  rebind, NAK, expiry, release.
- Diff engine: desired/observed cases, idempotency, unlisted interfaces.

### 12.2 QEMU integration

Add `scripts/rnet-test.sh` and run it on aarch64 and armv7:

- `rncli config check` on valid and invalid files.
- Static IPv4 address, default route and extra route.
- `config check` rejects IPv6 literals in address, gateway, DNS and route fields.
- Bridge creation with veth ports; port up/down; reload.
- DHCP from QEMU user-mode networking: lease, renew, release, re-acquire.
- Link hotplug: add/remove a veth pair.
- Reload adds/removes addresses, routes and bridges.
- Unlisted interface remains untouched.
- `rncli status`, `--porcelain`, exit codes and control-socket permissions.

Kernel config must include `CONFIG_BRIDGE`, `CONFIG_VETH` and `CONFIG_DUMMY`
for these tests. Add them to `make kernel-config-check`.

### 12.3 Soak

The 24 h soak must exercise:

- DHCP renew/rebind and periodic `rncli status`.
- A config reload that adds and removes an address/route.
- Optional link flap with a veth pair.
- RSS, descriptor and zombie stability; no growth in lease/state files.

### 12.4 Fuzzing and size

- Fuzz config parser, control codec and DHCP packet parser.
- `size-check` includes both binaries; record any budget change explicitly.

## 13. Task breakdown

Sizes are relative (S/M/L), not calendar estimates. Each task is complete only
when its acceptance checks pass.

### P0 - Skeleton and config

| ID | task | depends on | size | acceptance |
| --- | --- | --- | --- | --- |
| R0.1 | Create `crates/rnet` workspace member with `rnetd`/`rncli` stubs | - | S | Offline build passes; both `--version` work |
| R0.2 | Config model, parser, validator (IPv4 only) | R0.1 | L | Valid example parses; IPv6 literals are rejected; unknown/duplicate/invalid input reports file:line; unit tests |
| R0.3 | `rncli config check` and `config show` offline | R0.2 | S | Correct exit codes; no socket or kernel access |
| R0.4 | Control protocol codec and version/bounds tests | R0.1 | M | Round-trip and malformed-frame tests pass |
| R0.5 | Desired/observed diff skeleton with idempotency tests | R0.2 | M | Diff cases and "unlisted untouched" tests pass |

### P1 - Static IPv4 networking

| ID | task | depends on | size | acceptance |
| --- | --- | --- | --- | --- |
| R1.1 | Netlink core: socket, attributes, ACK/error, multipart | R0.1 | L | Fixture encode/decode; error mapping; no leaks |
| R1.2 | Link/address/route operations for IPv4, idempotent | R1.1 | L | QEMU: up/down, add/del IPv4 address and route; `ip` output matches |
| R1.3 | Apply static config through the diff engine | R1.2, R0.5 | M | Static example applied; reload adds/removes; no touch of unlisted links |
| R1.4 | `rnetd` poll loop, signal handling, control server | R0.4, R1.3 | L | `rncli status/show/reload` works; SIGTERM clean; SIGHUP reload |
| R1.5 | Add a static test profile: `rnetd` unit plus `rncli wait-online`; leave the default DHCP unit on `udhcpc` | R1.4 | M | Static test image boots; default DHCP image unchanged |
| R1.6 | QEMU static test script | R1.5 | M | aarch64 and armv7 static IPv4 cases pass against the test profile |

### P2 - Self-developed DHCPv4

| ID | task | depends on | size | acceptance |
| --- | --- | --- | --- | --- |
| R2.1 | DHCP packet/option codec | R0.1 | L | Golden packets and malformed-option tests; fuzz target |
| R2.2 | DHCP UDP transport: bind, broadcast, `SO_BINDTODEVICE`, `IP_PKTINFO` | R2.1 | M | Fake-server test sends OFFER/ACK; packets received on the right interface |
| R2.3 | State machine and timers | R2.2 | L | Fake clock/transport covers DISCOVER/OFFER/REQUEST/ACK/NAK/renew/rebind/expiry |
| R2.4 | Lease application and `/run/rnet/leases` | R2.3, R1.2 | M | IPv4 address/routes/DNS applied; release/expiry cleans up; atomic lease file |
| R2.5 | `rncli dhcp renew/release` and lease status | R2.4, R1.4 | S | Commands work; status shows address, server and remaining lease |
| R2.6 | QEMU DHCP test on QEMU user-net | R2.5 | M | Lease 10.0.2.15, renew, release, re-acquire; no `udhcpc` process |
| R2.7 | Cut the default `network.service` and `network-online.service` over to `rnetd`/`rncli`; remove `udhcpc` from the default path | R2.6 | M | Default image gets DHCP from rnetd; soak CSV/analyzer and docs updated |

### P3 - Bridges and hotplug

| ID | task | depends on | size | acceptance |
| --- | --- | --- | --- | --- |
| R3.1 | Netlink bridge create/delete and port enslave/detach | R1.2 | M | br0 created with STP/timers; ports enslaved; idempotent |
| R3.2 | Bridge config/apply integration | R3.1, R1.3 | M | Example bridge applied; invalid port/address combinations rejected |
| R3.3 | `RTNLGRP_LINK` events with poll fallback | R1.4 | M | veth add/remove triggers apply/cleanup; no busy loop |
| R3.4 | `rncli bridge` runtime commands | R1.4, R3.2 | S | create/delete/add-port/del-port work and report errors |
| R3.5 | QEMU bridge/hotplug test | R3.2, R3.3 | M | veth/bridge cases pass on both architectures |

### P4 - Runtime mutations and persistence

| ID | task | depends on | size | acceptance |
| --- | --- | --- | --- | --- |
| R4.1 | Runtime mutation RPCs for iface/addr/route/bridge/dhcp | R1.4, R2.5, R3.4 | M | Each command changes kernel state and is visible in status |
| R4.2 | `--persist` drop-in writer and merge | R4.1 | M | Atomic write; survives restart; failed apply does not persist |
| R4.3 | Persistence and CLI tests | R4.2 | S | Round-trip, conflict, rollback-on-failure, config check |

### P5 - Hardening and release

| ID | task | depends on | size | acceptance |
| --- | --- | --- | --- | --- |
| R5.1 | Fuzz and input-hardening pass | P0-P4 | M | No panics on fuzz corpus; bounds covered |
| R5.2 | Capability/socket/config permission tests | P1-P4 | S | Non-root mutation refused; status group behavior documented |
| R5.3 | 24 h soak with DHCP/reload/link flap | P2-P4 | M | Stable RSS/fds; no lease/state growth; no panic |
| R5.4 | Packaging and size gates | P1-P4 | M | Both binaries installed; size budget updated and met |
| R5.5 | Documentation and release gate update | all | S | README, porting, units, release doc updated; all gates recorded |

## 14. Risks and mitigations

| risk | impact | mitigation |
| --- | --- | --- |
| DHCPv4 parser/state machine correctness | wrong address, network outage | Golden-packet tests, fake clock/transport, QEMU lease/renew/release, fuzz, bounded parsing |
| Netlink message bugs | failed or wrong kernel state | Fixture tests, idempotency, QEMU tests, clear per-object errors |
| Two writers during migration | address/route flapping | Make `rnetd` the only writer; disable the old script's kernel writes in the same change |
| Boot ordering regression | sshd starts before network | Keep `network-online` as a oneshot; test boot graph and timing |
| `/etc/resolv.conf` conflict | lost DNS configuration | Refuse symlink replacement, atomic writes, opt-out |
| Missing link on a board | failed boot or long timeout | `required = no` default; `wait-online` has a timeout |
| Self-developed DHCP vs. BusyBox behavior | different lease handling | Document behavior; keep `udhcpc` in the image as a diagnostic fallback, not as a writer |
| Binary size | image budget | Measure in `size-check`; keep `rnet` free of tokio/serde/toml |
| Control socket abuse | unauthorized network changes | `0660 root:rnet`, capability-bounded daemon, input validation |

## 15. Acceptance gates

A release candidate is ready only when:

- `make check` passes, including the new unit tests.
- `make ARCH=arm64 test-system` and `make ARCH=arm32 test-system` pass with
  `network.service` running `rnetd`.
- `scripts/rnet-test.sh` passes on both architectures.
- The QEMU DHCP test acquires, renews, releases and re-acquires a lease
  without `udhcpc`.
- The 24 h soak passes with `rnetd`, DHCP renewal and a config reload.
- `size-check` passes for both architectures with the new binaries.
- `rncli` control-socket permissions, config errors and failure exit codes are
  covered by tests.
- `docs/release.md` records the new gates and the measured results.

## 16. Implementation status

- **P0:** `crates/rnet` skeleton, IPv4-only parser/validator, `rncli config
  check/show`, control codec and diff skeleton are implemented.
- **P1:** netlink link/address/route operations, `rnetd` poll loop, control
  server, `network.service` integration and `network-online` replacement are
  implemented.
- **P2:** the self-developed DHCPv4 codec, UDP socket, state machine, lease
  persistence, `rncli dhcp`, and the default `network.service` cutover are
  implemented. QEMU user-mode DHCP supplies address, default route and DNS.
- **P3:** bridge create/delete, port enslave/detach, bridge timers and link
  hotplug are implemented.
- **P4:** runtime `iface`/`addr`/`route`/`bridge`/`dhcp` commands and
  `rncli --persist` drop-ins are implemented.
- **P5:** `make check` and both architecture system tests pass, including the
  rnet runtime, bridge and persistence assertions. The `a62642a` 24 h runner
  completed, but the post-run analyzer rejected invalid FD samples from the old
  sampler. The sampler and tool are fixed; a fresh 24 h run is required. See
  `docs/release.md`.
