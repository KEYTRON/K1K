MAKEFLAGS += -rR
.SUFFIXES:

PROFILE   ?= dev
QEMUFLAGS ?= -m 512M -smp 1
LIMINE     = third_party/limine
BUILD      = build
ISO        = $(BUILD)/k1k.iso
TEST_ISO   = $(BUILD)/k1k-test.iso
TEST_CONF  = $(BUILD)/limine-test.conf

ifeq ($(PROFILE),release)
  CARGO_PROFILE_FLAG = --release
  KERNEL_ELF = kernel/target/x86_64-unknown-none/release/k1k
else
  CARGO_PROFILE_FLAG =
  KERNEL_ELF = kernel/target/x86_64-unknown-none/debug/k1k
endif

.PHONY: all user kernel iso run run-bios run-uefi test clean distclean limine fmt clippy

all: iso

$(LIMINE)/limine:
	@test -d $(LIMINE) || git clone --depth=1 --branch v9.x-binary https://github.com/limine-bootloader/limine.git $(LIMINE)
	$(MAKE) -s -C $(LIMINE)

limine: $(LIMINE)/limine

# Ring-3 services live in their own workspace; kernel/build.rs builds them
# automatically, this target is for working on them in isolation.
user:
	cd user && cargo build --release --workspace

kernel:
	cd kernel && cargo build $(CARGO_PROFILE_FLAG)

iso: $(LIMINE)/limine kernel
	sh tools/mkiso.sh $(KERNEL_ELF) limine.conf $(ISO)

$(TEST_CONF): limine.conf
	@mkdir -p $(BUILD)
	sed 's/^\(\s*\)kaslr: no/\1kaslr: no\n\1cmdline: autotest/' limine.conf > $(TEST_CONF)

test-iso: $(LIMINE)/limine kernel $(TEST_CONF)
	sh tools/mkiso.sh $(KERNEL_ELF) $(TEST_CONF) $(TEST_ISO)

run: run-bios

run-bios: iso
	qemu-system-x86_64 -M q35 -cdrom $(ISO) -boot d -serial stdio $(QEMUFLAGS)

run-uefi: iso
	qemu-system-x86_64 -M q35 \
		-drive if=pflash,unit=0,format=raw,file=/usr/share/edk2-ovmf/OVMF_CODE.fd,readonly=on \
		-cdrom $(ISO) -serial stdio $(QEMUFLAGS)

# Headless self-test: kernel boots with `autotest`, runs the demo services for
# a few seconds and exits QEMU with status 33 (success) via isa-debug-exit.
test: test-iso
	@rm -f $(BUILD)/serial.log
	@timeout 60 qemu-system-x86_64 -M q35 -cdrom $(TEST_ISO) -boot d \
		-display none -serial file:$(BUILD)/serial.log -no-reboot \
		-device isa-debug-exit,iobase=0xf4,iosize=0x04 $(QEMUFLAGS); \
	status=$$?; cat $(BUILD)/serial.log; echo "qemu exit: $$status"; test $$status -eq 33

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

distclean: clean
	rm -rf $(LIMINE)
