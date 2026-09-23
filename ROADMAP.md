# K1K roadmap

Stage: 1.0.0-alpha.1

Stages go in order: `[x]` is done, `[ ]` is planned. The current stage is the first unfinished one.

## Boot and the base kernel
- [x] Boot via Limine (BIOS and UEFI), logging to COM1 and the framebuffer
- [x] GDT/TSS, IDT; an exception in ring 3 kills only the offending task
- [x] Physical and virtual memory, separate address spaces, kernel heap
- [x] ACPI, Local APIC timer, routing through the I/O APIC
- [x] Preemptive scheduler (round-robin, 10 ms quantum)
- [x] SMP: per-CPU blocks via GS base and `swapgs`

## Capabilities and ring-3 drivers
- [x] Capability tables and synchronous IPC endpoints
- [x] Capability transfer in messages (rights ∩ mask)
- [x] Memory objects and DMA memory
- [x] Capabilities for PCI devices, interrupts (ISA and MSI-X) and I/O ports
- [x] PS/2 keyboard driver in ring 3
- [x] NVMe driver in ring 3 with MSI-X interrupts

## Services from disk
- [x] Ring 3 via `syscall`/`sysret`, 22 syscalls
- [x] ELF64 loader and the `k1k-rt` runtime for Rust services
- [x] A supervisor restarts crashed services (with backoff)
- [x] Read-only FAT file server on top of NVMe
- [x] Services are loaded from `/SVC` on disk via `spawn`

## Files and service startup
- [ ] File protocol (open/read over IPC) so services can read files themselves
- [ ] Passing capabilities to spawned services
- [ ] A service manifest on disk instead of "everything in /SVC"
- [ ] An allocator for `k1k-rt`

## Kernel: time, notifications, revocation
- [ ] Asynchronous notifications
- [ ] IPIs: TLB shootdown and remote rescheduling
- [ ] HPET/TSC clocks
- [ ] Capability revocation
- [ ] A scheduler without a single global lock
- [ ] A growing kernel heap and returning bootloader memory to the PMM

## K1OS on K1K
- [x] K1OS boots on K1K (boot test in CI on every push)
- [ ] Service binaries delivered to `/SVC` as WARP packages
- [ ] A terminal and shell on top of K1K
- [ ] Networking

## Other architectures
K1K runs only on x86_64 today: all platform code (GDT/IDT, APIC, ACPI) is written for it.
- [ ] An `arch/` layer separated from the common kernel code
- [ ] aarch64 in QEMU (`virt`): GIC interrupt controller, ARM timer, boot through Limine over UEFI
- [ ] CI on aarch64
- [ ] Apple Silicon: the AIC interrupt controller, boot through m1n1 and U-Boot
- [ ] RISC-V (riscv64) — once there is real hardware to test on
