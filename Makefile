# ============================================================================
# rinit aarch64 bring-up pipeline
#
#   make run         download + build kernel & busybox + rinit, pack the
#                    initramfs, and boot it in qemu (default goal)
#   make kernel      build the Linux kernel Image only
#   make busybox     build busybox only
#   make initramfs   build rinit and assemble the initramfs
#   make qemu        alias for `make run`
#   make check       pre-commit gate: fmt + clippy + host tests (no cross toolchain)
#   make test        check + the qemu system test
#   make test-system boot the guest and assert the M0/M1 acceptance list
#   make soak        long-run leak check: restart churn while watching RSS/fds
#   make help        show this help
#   make clean       remove build outputs (keeps downloads)
#   make distclean   remove everything, including downloads
#
# Everything targets aarch64. Sources come from the Tsinghua mirror first, with
# upstream fallbacks (see scripts/fetch.sh). Any step whose output already
# exists is skipped automatically, so re-running `make run` only boots.
# ============================================================================

ARCH             ?= arm64
TARGET           ?= aarch64-unknown-linux-gnu
CROSS_COMPILE    ?= aarch64-linux-gnu-

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

# busybox's own ARCH spelling (kernel-style); overridable if your tree differs.
BUSYBOX_ARCH     ?= arm64

JOBS             ?= $(shell nproc 2>/dev/null || echo 2)
QEMU_MEM         ?= 128M

BUILD_DIR        := build
DL_DIR           := $(BUILD_DIR)/dl
SRC_DIR          := $(BUILD_DIR)/src
OUT_DIR          := $(BUILD_DIR)/out

KERNEL_TARBALL   := $(DL_DIR)/linux-$(KERNEL_VERSION).tar.xz
BUSYBOX_TARBALL  := $(DL_DIR)/busybox-$(BUSYBOX_VERSION).tar.bz2
KERNEL_SRC       := $(SRC_DIR)/linux-$(KERNEL_VERSION)
BUSYBOX_SRC      := $(SRC_DIR)/busybox-$(BUSYBOX_VERSION)
KERNEL_IMAGE     := $(KERNEL_SRC)/arch/arm64/boot/Image
BUSYBOX_BIN      := $(BUSYBOX_SRC)/busybox
INITRAMFS        := $(OUT_DIR)/initramfs-$(TARGET).cpio.gz

SCRIPTS          := scripts
MAKE_KERNEL      := $(MAKE) -C $(KERNEL_SRC) ARCH=$(ARCH) CROSS_COMPILE=$(CROSS_COMPILE)
MAKE_BUSYBOX     := $(MAKE) -C $(BUSYBOX_SRC) ARCH=$(BUSYBOX_ARCH) CROSS_COMPILE=$(CROSS_COMPILE)

.DEFAULT_GOAL    := run
.PHONY: run kernel busybox rinit initramfs qemu help clean distclean \
        check test test-system soak

help:
	@sed -n '2,/^# ====/p' $(MAKEFILE_LIST) | sed '$$d' | sed 's/^# \{0,1\}//'

# --- rinit (static aarch64 binaries) --------------------------------------
RINIT_BIN        := target/$(TARGET)/release/rinit
RINIT_TEST_BIN   := target/$(TARGET)/release/rinit-testservice
RCTL_BIN         := target/$(TARGET)/release/rctl
RINIT_SRCS       := $(shell find crates -name '*.rs' 2>/dev/null) Cargo.toml Cargo.lock

rinit: $(RINIT_BIN)

# Grouped target: one cargo invocation builds all three binaries. Nothing
# happens when they already exist and are newer than the sources.
$(RINIT_BIN) $(RINIT_TEST_BIN) $(RCTL_BIN) &: $(RINIT_SRCS)
	cargo build --release --target $(TARGET)
	@test -x $(RINIT_TEST_BIN) || { echo "rinit build failed"; exit 1; }
	@test -x $(RCTL_BIN) || { echo "rctl build failed"; exit 1; }

# --- Linux kernel ---------------------------------------------------------
$(KERNEL_TARBALL):
	$(SCRIPTS)/fetch.sh $@ $(KERNEL_URLS)

$(KERNEL_SRC)/.config: $(KERNEL_TARBALL)
	mkdir -p $(SRC_DIR)
	tar -C $(SRC_DIR) -xf $(KERNEL_TARBALL)
	$(MAKE_KERNEL) defconfig
	$(MAKE_KERNEL) olddefconfig

kernel: $(KERNEL_IMAGE)

$(KERNEL_IMAGE): $(KERNEL_SRC)/.config
	$(MAKE_KERNEL) -j$(JOBS) Image
	@echo "kernel image: $@"

# --- busybox (static aarch64) --------------------------------------------
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
	# `install` step in build-initramfs.sh) compile for aarch64 by default.
	sed -i -e 's/^# CONFIG_STATIC is not set$$/CONFIG_STATIC=y/' \
	       -e 's/^CONFIG_TC=y$$/# CONFIG_TC is not set/' \
	       -e 's/^CONFIG_CROSS_COMPILER_PREFIX=""$$/CONFIG_CROSS_COMPILER_PREFIX="aarch64-linux-gnu-"/' \
	       $(BUSYBOX_SRC)/.config

busybox: $(BUSYBOX_BIN)

$(BUSYBOX_BIN): $(BUSYBOX_SRC)/.config
	$(MAKE_BUSYBOX) -j$(JOBS)

# --- initramfs ------------------------------------------------------------
initramfs: $(INITRAMFS)

# Repacked when rinit, busybox, a unit file, or the packing script itself
# changes - the script decides what goes in the image, so editing it has to
# invalidate the image just like editing a unit does.
UNIT_FILES := $(wildcard units/*.service) $(wildcard units/*.target)
INITRAMFS_SCRIPTS := $(SCRIPTS)/build-initramfs.sh $(SCRIPTS)/mkinitramfs.sh

$(INITRAMFS): $(RINIT_BIN) $(RCTL_BIN) $(BUSYBOX_BIN) $(UNIT_FILES) $(INITRAMFS_SCRIPTS)
	$(SCRIPTS)/build-initramfs.sh $(TARGET) --busybox-src $(BUSYBOX_SRC) --out $@

# The guest boots units/, so nothing is put on the kernel cmdline: any token
# there would override the unit directory and the console shell would never
# start. Everything is rebuilt by the file targets above whenever a source is
# newer, so there is no "skip the build" switch - running `make run` twice just
# boots the second time.
QEMU_CMD = $(SCRIPTS)/run-qemu.sh --arch aarch64 --kernel $(KERNEL_IMAGE) \
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

# `test` = `check` plus the qemu system test, which needs the aarch64
# artifacts.
test: check
	$(MAKE) test-system

# `test-system` boots the guest and asserts the acceptance list (M0-M5,
# `scripts/system-test.sh`) on the serial console. Logs land in build/logs/.
test-system: $(KERNEL_IMAGE) $(INITRAMFS)
	$(SCRIPTS)/system-test.sh --kernel $(KERNEL_IMAGE) --initrd $(INITRAMFS)

# `soak` is the M4 long-run leak check: sustained restart churn plus a loop
# driving the control socket, watching RSS, descriptors and zombies. The default
# 60 seconds is a commit-time regression; plan §3 M4's acceptance run is
# `make soak SOAK_ARGS='--seconds 259200'` (72 hours) on the target.
SOAK_ARGS ?= --seconds 60
soak:
	$(SCRIPTS)/soak.sh $(SOAK_ARGS)

# --- housekeeping ---------------------------------------------------------
clean:
	rm -rf $(BUILD_DIR) initramfs-*.cpio.gz

distclean: clean
	rm -rf target
