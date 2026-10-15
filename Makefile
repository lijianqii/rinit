# ============================================================================
# rinit aarch64 bring-up pipeline
#
#   make run         download + build kernel & busybox + rinit, pack the
#                    initramfs, and boot it in qemu (default goal)
#   make run SKIP_BUILD=1   boot existing artifacts; never download or compile
#   make kernel      build the Linux kernel Image only
#   make busybox     build busybox only
#   make initramfs   build rinit and assemble the initramfs
#   make qemu        boot artifacts, building them first if missing
#   make check       pre-commit gate: fmt + clippy + host tests (no cross toolchain)
#   make test        check + the qemu system test
#   make test-system boot the guest and assert the M0 acceptance list
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

# Service appended to the kernel cmdline, overriding the unit directory.
#
# Empty by default: rinit boots units/ (which starts shell.service, so you get a
# root shell on the console). Setting this is the M0 bring-up path and it
# *replaces* the unit directory entirely, which is easy to mistake for "the
# new units did not make it into the image" when they actually did.
#   - SERVICE=/sbin/rinit-testservice --ticks 6 : prove fork/exec/reap only
#   - SERVICE=/bin/sh                           : plain busybox shell, no rinit units
SERVICE          ?=

# `make run SKIP_BUILD=1` boots what is already built and never downloads,
# extracts or compiles anything (it fails fast if an artifact is missing).
SKIP_BUILD       ?= 0

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
.PHONY: run kernel busybox rinit initramfs qemu check-artifacts help clean distclean \
        check test test-system

help:
	@sed -n '2,/^# ====/p' $(MAKEFILE_LIST) | sed '$$d' | sed 's/^# \{0,1\}//'

# --- rinit (static aarch64 binaries) --------------------------------------
RINIT_BIN        := target/$(TARGET)/release/rinit
RINIT_TEST_BIN   := target/$(TARGET)/release/rinit-testservice
RINIT_SRCS       := $(shell find crates -name '*.rs' 2>/dev/null) Cargo.toml Cargo.lock

rinit: $(RINIT_BIN)

# Grouped target: one cargo invocation builds both binaries. Nothing happens
# when they already exist and are newer than the sources.
$(RINIT_BIN) $(RINIT_TEST_BIN) &: $(RINIT_SRCS)
	cargo build --release --target $(TARGET)
	@test -x $(RINIT_TEST_BIN) || { echo "rinit build failed"; exit 1; }

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

# Repacked only when the rinit binaries or busybox change.
UNIT_FILES := $(wildcard units/*.service) $(wildcard units/*.target)

$(INITRAMFS): $(RINIT_BIN) $(RINIT_TEST_BIN) $(BUSYBOX_BIN) $(UNIT_FILES)
	$(SCRIPTS)/build-initramfs.sh $(TARGET) --busybox-src $(BUSYBOX_SRC) --out $@

# --- boot -----------------------------------------------------------------
# `--service` is passed only when SERVICE is set: passing it empty would put an
# empty token on the kernel cmdline, which rinit would reject as an empty
# program path.
QEMU_CMD = $(SCRIPTS)/run-qemu.sh --arch aarch64 --kernel $(KERNEL_IMAGE) \
	--initrd $(INITRAMFS) --mem $(QEMU_MEM) \
	$(if $(strip $(SERVICE)),--service "$(SERVICE)")

qemu: $(KERNEL_IMAGE) $(INITRAMFS)
	$(QEMU_CMD)

ifeq ($(strip $(SKIP_BUILD)),1)
# Boot only: dependencies are checked, not built.
run: check-artifacts
	$(QEMU_CMD)
else
run: qemu
endif

check-artifacts:
	@missing=0; \
	for f in $(KERNEL_IMAGE) $(INITRAMFS); do \
	  if [ ! -e "$$f" ]; then echo "error: missing $$f" >&2; missing=1; fi; \
	done; \
	if [ $$missing -ne 0 ]; then \
	  echo "hint: run 'make run' once (without SKIP_BUILD=1) to build the artifacts." >&2; \
	  exit 1; \
	fi

# --- gates ----------------------------------------------------------------
# `check` is the pre-commit gate (plan §4.1 "每个 commit 必跑"): formatting,
# lints and the host test suite. It needs no cross toolchain and no network.
check:
	cargo fmt --all --check
	cargo clippy --offline --all-targets -- -D warnings
	cargo test --offline --workspace

# `test` = `check` plus the qemu system test, which needs the aarch64
# artifacts. Builds them first unless SKIP_BUILD=1.
test: check
	$(MAKE) test-system SKIP_BUILD=$(SKIP_BUILD)

# `test-system` boots the guest and asserts the M0 acceptance list on the
# serial console. Logs land in build/logs/.
test-system:
ifeq ($(strip $(SKIP_BUILD)),1)
	$(SCRIPTS)/system-test.sh --kernel $(KERNEL_IMAGE) --initrd $(INITRAMFS)
else
	$(KERNEL_IMAGE) $(INITRAMFS)
	$(SCRIPTS)/system-test.sh --kernel $(KERNEL_IMAGE) --initrd $(INITRAMFS)
endif

# --- housekeeping ---------------------------------------------------------
clean:
	rm -rf $(BUILD_DIR) initramfs-*.cpio.gz

distclean: clean
	rm -rf target
