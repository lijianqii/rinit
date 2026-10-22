# ============================================================================
# rinit arm64/arm32 bring-up pipeline
#
#   make ARCH=arm64 run   download + build kernel & busybox + rinit, pack the
#                         initramfs, and boot it in qemu (default; ARCH=arm64)
#   make ARCH=arm32 run   the same for ARMv7 (qemu virt, Cortex-A15)
#   make kernel      build the Linux kernel Image/zImage only
#   make busybox     build busybox only
#   make initramfs   build rinit and assemble the initramfs
#   make initramfs-demo  the same image plus units/demo/ (what `make test-system` boots)
#   make qemu        alias for `make run`
#   make check       pre-commit gate: fmt + clippy + host tests (no cross toolchain)
#   make test        check + the qemu system test
#   make test-system boot the guest and assert the acceptance list (M0-M5)
#   make soak        long-run leak check: restart churn while watching RSS/fds
#   make parallel    20 services in parallel vs chained: measures the speedup
#   make gates       acceptance gates: binary size + test coverage
#   make size-check  the shipped binaries against plan §0.1's size budget
#   make coverage-check  plan §6's 80%/90% coverage thresholds, measured
#   make help        show this help
#   make clean       remove build outputs (keeps downloads)
#   make distclean   remove everything, including downloads
#
# ARCH selects the target family: arm64 (default) or arm32. The kernel's own
# spelling is KERNEL_ARCH; keeping the two separate lets `make ARCH=arm32 run`
# be the command-line switch while the existing aarch64 artifacts keep their
# paths. Sources come from the Tsinghua mirror first, with upstream fallbacks
# (see scripts/fetch.sh). Any step whose output already exists is skipped.
# ============================================================================

ARCH             ?= arm64
ifeq ($(ARCH),arm64)
  TARGET           ?= aarch64-unknown-linux-gnu
  CROSS_COMPILE    ?= aarch64-linux-gnu-
  KERNEL_ARCH      := arm64
  KERNEL_DEFCONFIG := defconfig
  KERNEL_IMAGE_REL := arch/arm64/boot/Image
  KERNEL_BOOT_TARGET := Image
  BUSYBOX_ARCH     ?= arm64
  QEMU_ARCH        := aarch64
else ifeq ($(ARCH),arm32)
  TARGET           ?= armv7-unknown-linux-gnueabihf
  CROSS_COMPILE    ?= arm-linux-gnueabihf-
  KERNEL_ARCH      := arm
  KERNEL_DEFCONFIG := multi_v7_defconfig
  KERNEL_IMAGE_REL := arch/arm/boot/zImage
  KERNEL_BOOT_TARGET := zImage
  BUSYBOX_ARCH     ?= arm
  QEMU_ARCH        := arm
else
  $(error ARCH must be arm64 or arm32)
endif

KERNEL_VERSION   ?= 6.6.158
# 1.36.1 is the latest patch release of the busybox 1.36 line.
BUSYBOX_VERSION  ?= 1.36.1

# Tsinghua mirror first; official upstream as fallback.
MIRROR           ?= https://mirrors.tuna.tsinghua.edu.cn
KERNEL_URLS      ?= $(MIRROR)/kernel/v6.x/linux-$(KERNEL_VERSION).tar.xz \
                    https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$(KERNEL_VERSION).tar.xz
BUSYBOX_URLS     ?= $(MIRROR)/busybox/busybox-$(BUSYBOX_VERSION).tar.bz2 \
                    https://busybox.net/downloads/busybox-$(BUSYBOX_VERSION).tar.bz2 \
                    https://busybox.net/downloads/busybox-$(BUSYBOX_VERSION).tar.gz

JOBS             ?= $(shell nproc 2>/dev/null || echo 2)
QEMU_MEM         ?= 128M

# Everything the guest's acceptance tests need the kernel to provide.
KERNEL_REQUIRED_OPTS := CGROUP_SCHED FAIR_GROUP_SCHED CFS_BANDWIDTH MEMCG \
                        CGROUP_PIDS I6300ESB_WDT WATCHDOG_SYSFS VIRTIO_NET \
                        VIRTIO_MMIO
ifeq ($(KERNEL_ARCH),arm)
KERNEL_REQUIRED_OPTS += ARCH_VIRT PCI_HOST_GENERIC
endif

BUILD_DIR        := build
DL_DIR           := $(BUILD_DIR)/dl
# arm64 keeps the original source/output paths; arm32 gets its own tree so
# switching ARCH cannot reuse a kernel or busybox built for the other word size.
SRC_DIR          := $(if $(filter arm64,$(ARCH)),$(BUILD_DIR)/src,$(BUILD_DIR)/$(ARCH)/src)
OUT_DIR          := $(if $(filter arm64,$(ARCH)),$(BUILD_DIR)/out,$(BUILD_DIR)/$(ARCH)/out)

KERNEL_TARBALL   := $(DL_DIR)/linux-$(KERNEL_VERSION).tar.xz
BUSYBOX_TARBALL  := $(DL_DIR)/busybox-$(BUSYBOX_VERSION).tar.bz2
KERNEL_SRC       := $(SRC_DIR)/linux-$(KERNEL_VERSION)
BUSYBOX_SRC      := $(SRC_DIR)/busybox-$(BUSYBOX_VERSION)
KERNEL_IMAGE     := $(KERNEL_SRC)/$(KERNEL_IMAGE_REL)
BUSYBOX_BIN      := $(BUSYBOX_SRC)/busybox
# Installed into the image as /etc/fstab and read by rinit's second mount wave,
# so a change to it has to repack the image just like a unit file does.
FSTAB            := fstab
INITRAMFS        := $(OUT_DIR)/initramfs-$(TARGET).cpio.gz
# The acceptance image: the same root plus units/demo/. `make run` boots the
# image above, `make test-system` this one - see the packing rules below.
INITRAMFS_DEMO   := $(OUT_DIR)/initramfs-$(TARGET)-demo.cpio.gz

SCRIPTS          := scripts
MAKE_KERNEL      := $(MAKE) -C $(KERNEL_SRC) ARCH=$(KERNEL_ARCH) CROSS_COMPILE=$(CROSS_COMPILE)
MAKE_BUSYBOX     := $(MAKE) -C $(BUSYBOX_SRC) ARCH=$(BUSYBOX_ARCH) CROSS_COMPILE=$(CROSS_COMPILE)

.DEFAULT_GOAL    := run
.PHONY: run kernel kernel-config-check busybox rinit initramfs initramfs-demo qemu \
        help clean distclean check test test-system soak parallel gates \
        size-check coverage-check

help:
	@sed -n '2,/^# ====/p' $(MAKEFILE_LIST) | sed '$$d' | sed 's/^# \{0,1\}//'

# --- rinit (static target binaries) ---------------------------------------
RINIT_BIN        := target/$(TARGET)/release/rinit
RCTL_BIN         := target/$(TARGET)/release/rctl
RINIT_SRCS       := $(shell find crates -name '*.rs' 2>/dev/null) Cargo.toml Cargo.lock

rinit: $(RINIT_BIN)

# Grouped target: one cargo invocation builds both binaries. Nothing happens
# when they already exist and are newer than the sources.
$(RINIT_BIN) $(RCTL_BIN) &: $(RINIT_SRCS)
	cargo build --release --target $(TARGET)
	@test -x $(RINIT_BIN) || { echo "rinit build failed"; exit 1; }
	@test -x $(RCTL_BIN) || { echo "rctl build failed"; exit 1; }

# --- Linux kernel ---------------------------------------------------------
$(KERNEL_TARBALL):
	$(SCRIPTS)/fetch.sh $@ $(KERNEL_URLS)

$(KERNEL_SRC)/.config: $(KERNEL_TARBALL)
	mkdir -p $(SRC_DIR)
	tar -C $(SRC_DIR) -xf $(KERNEL_TARBALL)
	$(MAKE_KERNEL) $(KERNEL_DEFCONFIG)
	# A defconfig deliberately leaves out pieces this guest needs. ARM's
	# `multi_v7_defconfig` has `ARCH_VIRT`/virtio but no cgroup controllers,
	# watchdog driver or generic PCI host; aarch64's defconfig has the cgroup
	# controllers but no watchdog. Turn on everything the acceptance tests use,
	# then let olddefconfig resolve dependencies. A symbol that still disappears
	# is caught by kernel-config-check below.
	@for opt in $(KERNEL_REQUIRED_OPTS); do \
	    sed -i "s/^# CONFIG_$$opt is not set$$/CONFIG_$$opt=y/" $(KERNEL_SRC)/.config; \
	done
	$(MAKE_KERNEL) olddefconfig
	# Kconfig evaluates dependencies in symbol order: a symbol enabled in the
	# first pass can be dropped before the symbol it depends on (FAIR_GROUP_SCHED
	# for CFS_BANDWIDTH on ARM) has been defaulted. Re-apply after the graph has
	# settled, then resolve once more.
	@for opt in $(KERNEL_REQUIRED_OPTS); do \
	    sed -i "s/^# CONFIG_$$opt is not set$$/CONFIG_$$opt=y/" $(KERNEL_SRC)/.config; \
	done
	$(MAKE_KERNEL) olddefconfig

# The symbols the guest's acceptance tests depend on, checked on every
# `make kernel` and not only when the config is generated. `olddefconfig` drops
# an unmet symbol without a word, so without this a defconfig change upstream
# would turn into a guest boot where a limit, or the watchdog, silently does not
# exist - which is the one failure mode these checks exist to prevent.
kernel-config-check: $(KERNEL_SRC)/.config
	@for opt in $(KERNEL_REQUIRED_OPTS); do \
	    grep -q "^CONFIG_$$opt=y$$" $(KERNEL_SRC)/.config || { \
	        echo "kernel config: CONFIG_$$opt is not set (ARCH=$(ARCH) needs it)" >&2; \
	        exit 1; }; \
	done

kernel: kernel-config-check $(KERNEL_IMAGE)

$(KERNEL_IMAGE): $(KERNEL_SRC)/.config
	$(MAKE_KERNEL) -j$(JOBS) $(KERNEL_BOOT_TARGET)
	@echo "kernel image: $@"

# --- busybox (static target) ----------------------------------------------
$(BUSYBOX_TARBALL):
	$(SCRIPTS)/fetch.sh $@ $(BUSYBOX_URLS)

$(BUSYBOX_SRC)/.config: $(BUSYBOX_TARBALL)
	mkdir -p $(SRC_DIR)
	tar -C $(SRC_DIR) -xf $(BUSYBOX_TARBALL)
	$(MAKE_BUSYBOX) defconfig
	# busybox has no olddefconfig target; silentoldconfig is the
	# non-interactive equivalent (conf -s, no prompts under make).
	$(MAKE_BUSYBOX) silentoldconfig
	# Two settings the generic busybox defconfig gets wrong for this
	# initramfs: static linking (the cpio has no ld.so/libc, /bin/sh must
	# be static) and the tc applet (networking/tc.c does not compile against
	# current kernel UAPI headers: struct tc_cbq_wrropt was removed upstream).
	# The cross prefix makes every busybox make in the tree (notably the
	# `install` step in build-initramfs.sh) compile for the selected target
	# by default.
	sed -i -e 's/^# CONFIG_STATIC is not set$$/CONFIG_STATIC=y/' \
	       -e 's/^CONFIG_TC=y$$/# CONFIG_TC is not set/' \
	       -e "s/^CONFIG_CROSS_COMPILER_PREFIX=\"\"$$/CONFIG_CROSS_COMPILER_PREFIX=\"$(CROSS_COMPILE)\"/" \
	       $(BUSYBOX_SRC)/.config

busybox: $(BUSYBOX_BIN)

$(BUSYBOX_BIN): $(BUSYBOX_SRC)/.config
	$(MAKE_BUSYBOX) -j$(JOBS)

# --- initramfs ------------------------------------------------------------
initramfs: $(INITRAMFS)
initramfs-demo: $(INITRAMFS_DEMO)

# Repacked when rinit, busybox, a unit file, or the packing script itself
# changes - the script decides what goes in the image, so editing it has to
# invalidate the image just like editing a unit does. The two unit lists are
# separate because the images are: the run image does not contain `units/demo/`,
# so a change there must not repack it.
BOOT_UNIT_FILES := $(wildcard units/*.service) $(wildcard units/*.target)
DEMO_UNIT_FILES := $(wildcard units/demo/*.service) $(wildcard units/demo/*.target)
INITRAMFS_SCRIPTS := $(SCRIPTS)/build-initramfs.sh $(SCRIPTS)/mkinitramfs.sh

# Two images, one difference: `units/demo/` is the §7.x acceptance set, and it is
# packed only into the one the system test boots. `make run` is the development
# loop - boot, get a console, look at a service - and fourteen units that exist
# to be started one at a time from the console are not part of that.
$(INITRAMFS): $(RINIT_BIN) $(RCTL_BIN) $(BUSYBOX_BIN) $(BOOT_UNIT_FILES) $(FSTAB) $(INITRAMFS_SCRIPTS)
	$(SCRIPTS)/build-initramfs.sh $(TARGET) --busybox-src $(BUSYBOX_SRC) --out $@

$(INITRAMFS_DEMO): $(RINIT_BIN) $(RCTL_BIN) $(BUSYBOX_BIN) $(BOOT_UNIT_FILES) \
                   $(DEMO_UNIT_FILES) $(FSTAB) $(INITRAMFS_SCRIPTS)
	$(SCRIPTS)/build-initramfs.sh $(TARGET) --busybox-src $(BUSYBOX_SRC) --demo-units --out $@

# The guest boots units/, so nothing is put on the kernel cmdline: any token
# there would override the unit directory and the console shell would never
# start. Everything is rebuilt by the file targets above whenever a source is
# newer, so there is no "skip the build" switch - running `make run` twice just
# boots the second time.
QEMU_CMD = $(SCRIPTS)/run-qemu.sh --arch $(QEMU_ARCH) --kernel $(KERNEL_IMAGE) \
	--initrd $(INITRAMFS) --mem $(QEMU_MEM)

qemu run: $(KERNEL_IMAGE) $(INITRAMFS)
	$(QEMU_CMD)

# --- gates ----------------------------------------------------------------
# `check` is the pre-commit gate (plan §4.1 "每个 commit 必跑"): formatting,
# lints and the host test suite. It needs no cross toolchain and no network.
check:
	cargo fmt --all --check
	cargo clippy --offline --all-targets -- -D warnings
	cargo test --offline --workspace

# `test` = `check` plus the qemu system test, which needs the target
# artifacts.
test: check
	$(MAKE) test-system

# `test-system` boots the guest and asserts the acceptance list (M0-M5,
# `scripts/system-test.sh`) on the serial console. Logs land in build/logs/.
# It boots the acceptance image: the §7.x units it drives from the console are
# in `units/demo/`, which `make run`'s image does not carry.
test-system: $(KERNEL_IMAGE) $(INITRAMFS_DEMO)
	$(SCRIPTS)/system-test.sh --arch $(QEMU_ARCH) --kernel $(KERNEL_IMAGE) --initrd $(INITRAMFS_DEMO)

# `soak` is the M4 long-run leak check: sustained restart churn plus a loop
# driving the control socket, watching RSS, descriptors and zombies. The default
# 60 seconds is a commit-time regression; plan §3 M4's acceptance run is
# `make soak SOAK_ARGS='--seconds 259200'` (72 hours) on the target.
SOAK_ARGS ?= --seconds 60
soak:
	$(SCRIPTS)/soak.sh $(SOAK_ARGS)

# --- acceptance gates -----------------------------------------------------
# `check` above is deliberately fast, and these are deliberately not part of it:
# both need artifacts the commit-time gate does not build, and the coverage run
# takes minutes rather than seconds. They are gates rather than tables in the
# README so that plan §6's two numeric items cannot rot: `make gates` is what a
# release, or the acceptance pass, runs.
#
# Plan §0.1's size budget: <=2 MB for a dynamic binary, <=4 MB for a static one.
# Both shipped binaries are static and are what the initramfs
# packs (`scripts/build-initramfs.sh`).
SIZE_BUDGET_STATIC := 4194304

size-check: rinit
	@fail=0; for b in $(RINIT_BIN) $(RCTL_BIN); do \
	    size=$$(stat -c%s "$$b"); \
	    printf 'size-check: %-46s %8d bytes (budget %d)\n' "$$b" "$$size" $(SIZE_BUDGET_STATIC); \
	    if [ "$$size" -gt $(SIZE_BUDGET_STATIC) ]; then \
	        echo "size-check: FAIL $$b is over the budget" >&2; fail=1; \
	    fi; \
	done; [ "$$fail" -eq 0 ] || exit 1; echo "size-check: OK"

# Plan §6's coverage thresholds, measured rather than quoted: 80% overall, 90%
# in rinit-core. Needs the `llvm-tools` component.
coverage-check:
	$(SCRIPTS)/coverage-check.sh

gates: size-check coverage-check

# --- parallel startup measurement (plan §6 acceptance) --------------------
# Runs the *host* rinit in `--dev` mode, so it needs no cross toolchain and no
# qemu: what is measured is the scheduler (the boot plan, the level walk, the
# spawn path), and the guest asserts the derived property instead.
HOST_RINIT := target/debug/rinit

$(HOST_RINIT): $(RINIT_SRCS)
	cargo build --offline

parallel: $(HOST_RINIT)
	$(SCRIPTS)/parallel-boot.sh --rinit $(HOST_RINIT) $(PARALLEL_ARGS)

# --- housekeeping ---------------------------------------------------------
clean:
	rm -rf $(BUILD_DIR) initramfs-*.cpio.gz

distclean: clean
	rm -rf target
