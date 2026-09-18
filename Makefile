MAKEFLAGS += -rR
.SUFFIXES:

PROFILE   ?= dev
QEMUFLAGS ?= -m 512M -smp 4
LIMINE     = third_party/limine
BUILD      = build
ISO        = $(BUILD)/k1k.iso
TEST_ISO   = $(BUILD)/k1k-test.iso
TEST_CONF  = $(BUILD)/limine-test.conf
DISK       = $(BUILD)/disk.img
DISK_MB   ?= 16

# Storage the ring-3 blk service drives: an NVMe controller with a raw image.
QEMU_DISK  = -drive file=$(DISK),if=none,format=raw,id=nvme0 \
             -device nvme,drive=nvme0,serial=K1K-NVME-0001

ifeq ($(PROFILE),release)
  CARGO_PROFILE_FLAG = --release
  KERNEL_ELF = kernel/target/x86_64-unknown-none/release/k1k
else
  CARGO_PROFILE_FLAG =
  KERNEL_ELF = kernel/target/x86_64-unknown-none/debug/k1k
endif

.PHONY: all user kernel iso test-iso disk run run-bios run-uefi test clean distclean limine fmt clippy

all: iso

$(LIMINE)/limine:
	@test -d $(LIMINE) || git clone --depth=1 --branch v9.x-binary https://github.com/limine-bootloader/limine.git $(LIMINE)
	$(MAKE) -s -C $(LIMINE)

limine: $(LIMINE)/limine

# Ring-3 services live in their own workspace; kernel/build.rs builds them
# automatically, this target is for working on them in isolation.
user:
	cd user && cargo build --release --workspace --target-dir $(USER_TARGET)

kernel:
	cd kernel && cargo build $(CARGO_PROFILE_FLAG)

iso: $(LIMINE)/limine kernel
	sh tools/mkiso.sh $(KERNEL_ELF) limine.conf $(ISO)

$(TEST_CONF): limine.conf
	@mkdir -p $(BUILD)
	sed 's/^\(\s*\)kaslr: no/\1kaslr: no\n\1cmdline: autotest/' limine.conf > $(TEST_CONF)

test-iso: $(LIMINE)/limine kernel $(TEST_CONF)
	sh tools/mkiso.sh $(KERNEL_ELF) $(TEST_CONF) $(TEST_ISO)

# FAT disk image: /SVC holds the services fs loads at boot (mtools required).
# Where the user workspace builds; override (with K1K_USER_TARGET_DIR for the
# kernel's build.rs) when the source tree is read-only.
USER_TARGET ?= $(CURDIR)/user/target
export K1K_USER_TARGET_DIR = $(USER_TARGET)
USER_BIN = $(USER_TARGET)/x86_64-unknown-none/release
DISK_SERVICES = hello flaky

$(DISK): user
	@mkdir -p $(BUILD)
	dd if=/dev/zero of=$(DISK) bs=1M count=$(DISK_MB) status=none
	mformat -i $(DISK) -v K1KDISK ::
	mmd -i $(DISK) ::/SVC
	for s in $(DISK_SERVICES); do mcopy -i $(DISK) $(USER_BIN)/$$s ::/SVC/$$(echo $$s | tr a-z A-Z).ELF; done
	printf 'K1K disk image v1 - FAT volume served by fs over blk\n' > $(BUILD)/README.TXT
	mcopy -i $(DISK) $(BUILD)/README.TXT ::/README.TXT
	mdir -i $(DISK) ::/SVC

disk: $(DISK)

run: run-bios

run-bios: iso $(DISK)
	qemu-system-x86_64 -M q35 -cdrom $(ISO) -boot d -serial stdio $(QEMU_DISK) $(QEMUFLAGS)

run-uefi: iso $(DISK)
	qemu-system-x86_64 -M q35 \
		-drive if=pflash,unit=0,format=raw,file=/usr/share/edk2-ovmf/OVMF_CODE.fd,readonly=on \
		-cdrom $(ISO) -serial stdio $(QEMU_DISK) $(QEMUFLAGS)

# Headless self-test: kernel boots with `autotest`, runs the services for a
# few seconds and exits QEMU with status 33 (success) via isa-debug-exit.
# The serial log must also show fs mounting the disk and spawning /SVC.
test: test-iso $(DISK)
	@rm -f $(BUILD)/serial.log
	@timeout 90 qemu-system-x86_64 -M q35 -cdrom $(TEST_ISO) -boot d \
		-display none -serial file:$(BUILD)/serial.log -no-reboot \
		-device isa-debug-exit,iobase=0xf4,iosize=0x04 $(QEMU_DISK) $(QEMUFLAGS); \
	status=$$?; cat $(BUILD)/serial.log; echo "qemu exit: $$status"; \
	test $$status -eq 33 \
		&& grep -q 'fs\] mounted' $(BUILD)/serial.log \
		&& grep -q 'fs\] spawned HELLO.ELF' $(BUILD)/serial.log

fmt:
	cd kernel && cargo fmt
	cd user && cargo fmt

clippy:
	cd kernel && cargo clippy
	cd user && cargo clippy --workspace

clean:
	cd kernel && cargo clean
	cd user && cargo clean
	rm -rf $(BUILD)

# The disk image embeds service binaries; rebuild it when they change.
.PHONY: $(DISK)

distclean: clean
	rm -rf $(LIMINE)
