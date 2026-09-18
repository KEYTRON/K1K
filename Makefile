MAKEFLAGS += -rR
.SUFFIXES:

PROFILE   ?= dev
QEMUFLAGS ?= -m 512M -smp 1
LIMINE     = third_party/limine
BUILD      = build
ISO        = $(BUILD)/k1k.iso

ifeq ($(PROFILE),release)
  CARGO_PROFILE_FLAG = --release
  KERNEL_ELF = kernel/target/x86_64-unknown-none/release/k1k
else
  CARGO_PROFILE_FLAG =
  KERNEL_ELF = kernel/target/x86_64-unknown-none/debug/k1k
endif

.PHONY: all kernel iso run run-bios run-uefi test clean distclean limine fmt clippy

all: iso

$(LIMINE)/limine:
	@test -d $(LIMINE) || git clone --depth=1 --branch v9.x-binary https://github.com/limine-bootloader/limine.git $(LIMINE)
	$(MAKE) -C $(LIMINE)

limine: $(LIMINE)/limine

kernel:
	cd kernel && cargo build $(CARGO_PROFILE_FLAG)

iso: $(LIMINE)/limine kernel
	rm -rf $(BUILD)/iso_root
	mkdir -p $(BUILD)/iso_root/boot/limine $(BUILD)/iso_root/EFI/BOOT
	cp $(KERNEL_ELF) $(BUILD)/iso_root/boot/k1k
	cp limine.conf $(LIMINE)/limine-bios.sys $(LIMINE)/limine-bios-cd.bin $(LIMINE)/limine-uefi-cd.bin $(BUILD)/iso_root/boot/limine/
	cp $(LIMINE)/BOOTX64.EFI $(LIMINE)/BOOTIA32.EFI $(BUILD)/iso_root/EFI/BOOT/
	xorriso -as mkisofs -quiet -b boot/limine/limine-bios-cd.bin \
		-no-emul-boot -boot-load-size 4 -boot-info-table \
		--efi-boot boot/limine/limine-uefi-cd.bin \
		-efi-boot-part --efi-boot-image --protective-msdos-label \
		$(BUILD)/iso_root -o $(ISO)
	$(LIMINE)/limine bios-install $(ISO) 2>/dev/null
	rm -rf $(BUILD)/iso_root

run: run-bios

run-bios: iso
	qemu-system-x86_64 -M q35 -cdrom $(ISO) -boot d -serial stdio $(QEMUFLAGS)

run-uefi: iso
	qemu-system-x86_64 -M q35 \
		-drive if=pflash,unit=0,format=raw,file=/usr/share/edk2-ovmf/OVMF_CODE.fd,readonly=on \
		-cdrom $(ISO) -serial stdio $(QEMUFLAGS)

# Headless smoke test: boots, captures serial log, exits via isa-debug-exit.
test: iso
	@mkdir -p $(BUILD)
	-timeout 30 qemu-system-x86_64 -M q35 -cdrom $(ISO) -boot d \
		-display none -serial file:$(BUILD)/serial.log -no-reboot \
		-device isa-debug-exit,iobase=0xf4,iosize=0x04 $(QEMUFLAGS); \
	echo "qemu exit: $$?"
	@cat $(BUILD)/serial.log

fmt:
	cd kernel && cargo fmt

clippy:
	cd kernel && cargo clippy

clean:
	cd kernel && cargo clean
	rm -rf $(BUILD)

distclean: clean
	rm -rf $(LIMINE)
