MAKEFLAGS += -rR
.SUFFIXES:

PROFILE   ?= dev
# `+invtsc` and `+rdtscp` are what the emulated TSC actually does — a fixed rate
# and the same reading on every core — but the default CPU model does not
# advertise them, and the kernel only trusts a clock the CPU vouches for. The
# fallback (counting timer interrupts) is the one worth not testing by default.
QEMUFLAGS ?= -m 512M -smp 4 -cpu qemu64,+invtsc,+rdtscp
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

.PHONY: all user kernel iso test-iso disk run run-bios run-uefi test test-heap clean distclean limine fmt clippy

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

# FAT disk image: /SVC holds the service images and the manifest that says
# which of them to start, with what capabilities (mtools required).
# Where the user workspace builds; override (with K1K_USER_TARGET_DIR for the
# kernel's build.rs) when the source tree is read-only.
USER_TARGET ?= $(CURDIR)/user/target
export K1K_USER_TARGET_DIR = $(USER_TARGET)
USER_BIN = $(USER_TARGET)/x86_64-unknown-none/release
# name:image:capabilities — the manifest fs reads at boot. Only what is listed
# here is started; an image dropped into /SVC without a manifest line stays
# inert. A capability of "-" means none.
DISK_SERVICES = \
	hello:HELLO.ELF:fs \
	flaky:FLAKY.EOF:-

$(DISK): user
	@mkdir -p $(BUILD)
	dd if=/dev/zero of=$(DISK) bs=1M count=$(DISK_MB) status=none
	mformat -i $(DISK) -v K1KDISK ::
	mmd -i $(DISK) ::/SVC
	printf 'K1K disk image v1 - FAT volume served by fs over blk\n' > $(BUILD)/README.TXT
	mcopy -i $(DISK) $(BUILD)/README.TXT ::/README.TXT
	printf '# K1K service manifest: name image [capabilities]\n' > $(BUILD)/MANIFEST.TXT
	for s in $(DISK_SERVICES); do \
		name=$${s%%:*}; rest=$${s#*:}; img=$${rest%%:*}; cap=$${rest#*:}; \
		printf '%s %s' "$$name" "$$img" >> $(BUILD)/MANIFEST.TXT; \
		test "$$cap" = "-" || printf ' %s' "$$cap" >> $(BUILD)/MANIFEST.TXT; \
		printf '\n' >> $(BUILD)/MANIFEST.TXT; \
		mcopy -i $(DISK) $(USER_BIN)/$$name ::/SVC/$$img || exit 1; \
	done; \
	mcopy -i $(DISK) $(BUILD)/MANIFEST.TXT ::/SVC/MANIFEST.TXT
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
# The serial log must show fs mounting the disk, starting the manifest and
# serving a real file read to a service that got no memory address of its own.
test: test-iso $(DISK)
	@rm -f $(BUILD)/serial.log
	@timeout 90 qemu-system-x86_64 -M q35 -cdrom $(TEST_ISO) -boot d \
		-display none -serial file:$(BUILD)/serial.log -no-reboot \
		-device isa-debug-exit,iobase=0xf4,iosize=0x04 $(QEMU_DISK) $(QEMUFLAGS); \
	status=$$?; cat $(BUILD)/serial.log; echo "qemu exit: $$status"; \
	test $$status -eq 33 \
		&& grep -q 'fs\] mounted' $(BUILD)/serial.log \
		&& grep -q 'notify\] notification semantics ok' $(BUILD)/serial.log \
		&& grep -q 'kbd\] keyboard driver online (ring 3, IRQ 1 via notification)' $(BUILD)/serial.log \
		&& grep -q "started 'hello' from /SVC/HELLO.ELF" $(BUILD)/serial.log \
		&& grep -q "started 'flaky' from /SVC/FLAKY.EOF" $(BUILD)/serial.log \
		&& grep -q 'fs\] serving files' $(BUILD)/serial.log \
		&& grep -q 'hello\] /SVC holds' $(BUILD)/serial.log \
		&& grep -q 'hello\] /README.TXT:' $(BUILD)/serial.log

# The same autotest under KVM, where the machine is the host: the TSC is the
# host's own, and the HPET is the chipset's real one. Two things have to be asked
# for explicitly, because a filtered CPUID view hides both: `+invtsc` (the host
# kernel does not promise an invariant TSC to its guests, whatever the hardware
# does) and `+rdtscp` (without which there is no processor id to sample with).
# TCG emulates neither faithfully — it refuses to claim an invariant TSC at all,
# and its HPET answers every register with the same constant — so the paths that
# matter are only exercised here. Skipped, not failed, where KVM is missing.
test-kvm: test-iso $(DISK)
	@if [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then \
		echo "kvm not available; skipping the hardware-clock test"; exit 0; \
	fi; \
	rm -f $(BUILD)/serial-kvm.log; \
	timeout 90 qemu-system-x86_64 -M q35 -accel kvm -cpu host,+invtsc,+rdtscp \
		-cdrom $(TEST_ISO) -boot d \
		-display none -serial file:$(BUILD)/serial-kvm.log -no-reboot \
		-device isa-debug-exit,iobase=0xf4,iosize=0x04 $(QEMU_DISK) -m 512M -smp 4; \
	status=$$?; cat $(BUILD)/serial-kvm.log; echo "qemu exit: $$status"; \
	test $$status -eq 33 \
		&& grep -q 'clock\] TSC [0-9]* MHz' $(BUILD)/serial-kvm.log \
		&& grep -q 'cpu(s) sampled the TSC' $(BUILD)/serial-kvm.log

# The allocator behind both the kernel heap and the service heap is plain logic
# over byte ranges, so it can be tested on the host: the harness keeps the
# system allocator and drives the crate directly, auditing the block list after
# every operation, with and without the supplier handing out more memory.
# `make test-heap`.
test-heap:
	@mkdir -p $(BUILD)/heaptest
	cp crates/k1k-alloc/src/lib.rs $(BUILD)/heaptest/k1k_alloc.rs
	cp tools/heap_test.rs $(BUILD)/heaptest/test.rs
	cd $(BUILD)/heaptest && rustc -O --edition 2024 -o test test.rs
	$(BUILD)/heaptest/test

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
