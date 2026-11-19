# Release gate

This is the checklist a release candidate must satisfy. It is deliberately
stricter than `make check`: host tests are fast enough for every commit, while
the target, soak and packaging checks answer "does this boot and stay up".

## 0. Candidate state

- [ ] `git status` is clean.
- [ ] `HEAD` is the intended release commit and has been pushed.
- [ ] `README.md` / `README.zh-CN.md` test, coverage and image figures were
  measured from that commit.
- [ ] No new dependency or generated artifact is uncommitted.

## 1. Host gates

```text
make check
make coverage-check
make ARCH=arm64 size-check
make ARCH=arm32 size-check
```

Required:

- [ ] `make check` reports `0 failed`.
- [ ] `scripts/soak-analyze-test.sh` reports `OK` as part of `make check`.
- [ ] Overall coverage is at least 80% and `rinit-core` at least 90%.
- [ ] Both `size-check` runs report `OK`, including `rnetd` and `rncli`.

## 2. Target gates

Build the release images first, then run both architectures:

```text
make ARCH=arm64 tools initramfs initramfs-demo disk-image
make ARCH=arm64 test-system
make ARCH=arm32 tools initramfs initramfs-demo disk-image
make ARCH=arm32 test-system
```

Required:

- [ ] `test-system` passes all assertions on aarch64 (including the rnet
  config, runtime address/route, bridge and persistence checks).
- [ ] `test-system` passes all assertions on armv7.

## 3. Tools gates

Against the same images:

```text
scripts/tools-test.sh --arch aarch64 --key <key>
scripts/tools-test.sh --arch arm --key <key> --port 2223
scripts/install-tools-test.sh
```

Required:

- [ ] Both `tools-test` runs report `TOOLS OK` with zero failures.
- [ ] `install-tools-test` reports zero failures.

## 4. Long-running gates

```text
scripts/soak-qemu.sh --arch aarch64 --key <key> \
  --seconds 86400 --interval 10 --checkpoint-interval 3600 \
  --port 2271 --tag release-24h
scripts/reboot-test.sh --arch aarch64 \
  --image build/out/rinit-aarch64-unknown-linux-gnu.ext4 \
  --kernel build/src/linux-6.6.158/arch/arm64/boot/Image --key <key>
```

Required:

- [ ] The 24 h soak prints `SOAK OK`.
- [ ] `soak-qemu.sh` reports `Analyzer: OK`; its JSON is saved as
  `build/soak-charts/soak-qemu-<tag>-analysis.json`.
- [ ] The final guest CSV has at least `seconds / interval` data rows.
- [ ] The serial log contains exactly one `Booting Linux` line; the harness
  fails early with `reason=guest-reboot` if the guest resets.
- [ ] Every scheduled checkpoint is `ok`, or a failed checkpoint is explained
  and the final CSV is complete. A checkpoint whose last elapsed value did not
  advance is stale and does not count.
- [ ] The guest reports `network.service` active and `rnetd` alive in every
  sample; `rncli` status is available.
- [ ] RSS, descriptors and zombies stay within the recorded bounds; no OOM or
  kernel panic appears in the serial log.
- [ ] `reboot-test` prints `REBOOT OK` with zero failed checks.
- [ ] The soak CSV header uses `rnetd` (or the analyzer's historical `udhcpc`
  fallback) and the analyzer reports no unhealthy network samples.

The soak disables the hardware watchdog by passing `--watchdog /dev/null`:
a host stall longer than the watchdog interval must fail the host test, not
silently reset the guest and destroy a 24 h run. `reboot-test` is the gate that
exercises the watchdog paths.

## 5. Release action

Only after all boxes above are checked:

- [ ] Tag the tested commit.
- [ ] Publish the four image sizes and the test/coverage figures from this
  commit.
- [ ] Keep the 24 h soak serial log and CSV with the release record.
- [ ] Record any deferred item explicitly; do not treat a skipped gate as a
  pass.

## 6. Local verification log

These are development measurements, not a release candidate: the working tree
was not committed when they were recorded, and the 24 h soak row below is an
incomplete run rather than a pass.

| gate | result |
| --- | --- |
| `make check` | pass (0 failed) |
| `make coverage-check` | not rerun in this working tree; `cargo-llvm-cov` is not installed. Previous recorded result: overall **82.02%**, `rinit-core` **97.91%** |
| `make ARCH=arm64 size-check` | pass: `rnetd` 529,216 B, `rncli` 398,144 B |
| `make ARCH=arm32 size-check` | pass: `rnetd` 593,056 B, `rncli` 461,984 B |
| aarch64 `test-system` | pass: 115 assertions, including rnet socket ownership, config, runtime address/route, bridge, persistence and hotplug |
| armv7 `test-system` | pass: 115 assertions, same rnet checks |
| aarch64 `tools-test` | `TOOLS OK`, 102 checks, including live-FD `ls` regression |
| armv7 `tools-test` | `TOOLS OK`, 102 checks, including live-FD `ls` regression |
| `install-tools-test` | 17 passed, 0 failed |
| aarch64 `device-smoke` | not rerun in this working tree; historical pass |
| armv7 `device-smoke` | not rerun in this working tree; historical pass |
| `reboot-test` | not rerun in this working tree; historical pass |
| 60 s host soak | `SOAK OK`, 60 samples, 469 churns, 120/120 control commands |
| 120 s guest soak | `SOAK OK` and `Analyzer: OK`, 12 samples, FD 20 throughout, `sample_fail=0` |
| 24 h soak (`release-24h`, `a62642a`) | Runner reported `SOAK OK`: 8,640 samples, 17,280/17,280 control commands, 24 hourly checkpoints `ok`, RSS 2,736–3,028 kB, zero zombies. The original analyzer rejected 21 zero-FD samples caused by the replacement `ls`; the tool and sampler are now fixed, but a fresh 24 h run is required before this gate can pass |
| 72 h host soak | in progress during this working-tree verification |
