# K1K roadmap

Stage: 1.0.0-alpha.3

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
- [x] File protocol (`open`/`read`/`stat`/`list` over a shared buffer) so
  services can read files themselves
- [x] Capabilities and launch arguments passed to spawned services, in
  manifest order, so a service finds its file server by name and not by slot
  number
- [x] A service manifest on disk instead of "everything in /SVC": only what
  `/SVC/MANIFEST.TXT` lists is started
- [x] A heap for every ring-3 task, with `k1k-rt` as its global allocator
  (host-tested against its own block list, `make test-heap`)
- [x] One allocator for the kernel and for services: the `k1k-alloc` crate,
  tested on the host with a fixed service heap and a growing kernel one

## Kernel: time, notifications, revocation
- [x] Asynchronous notifications: a `Notify` object is a counter of events with
  tasks waiting on it, so an event that arrives before the wait is not lost.
  `SIGNAL` and `WAIT` are separate rights; `irq_bind` can deliver an interrupt
  as a signal, which is what the ring-3 keyboard driver uses
- [x] IPIs: TLB shootdown (before an address space is freed, and after a change
  to the shared kernel half) and remote rescheduling (a woken task pulls an idle
  CPU out of `hlt` instead of waiting for the next tick)
- [x] HPET/TSC clocks: the TSC measured against the PIT and used only where the
  CPU says it is invariant, the HPET found in ACPI and on the PCI bus with its
  timers masked and its rate counted, and the tick counter as the fallback. The
  base is chosen at boot, the `time` syscall and `uptime_ms()` come from it, and
  every CPU's TSC reading is checked against every other. `make test-kvm` runs
  the autotest on the host's own TSC and HPET, which is the only place those
  paths can be trusted at all: TCG emulates neither
- [x] Capability revocation: a spawner is told which slot each of its grants
  landed in, `cap_revoke` takes it back, a revoked slot is a tombstone that is
  never reused, and tearing a task down strips its table. Withdrawing authority
  from another task needs `Control`, checked before the task is looked up
- [ ] A scheduler without a single global lock: per-CPU run queues with work
  stealing and per-CPU sleeper lists, keeping the task table for the slow paths
  only. Attempted in `~/git/K1K-notes/sched-percpu-runqueues.WIP.rs`; the run
  queue, the stealing, the sleeper lists, the switch handoff and the atomic task
  state are written there, but the boot still hung and the tree was left at a
  green commit rather than half-finished. Not kept: the per-CPU fields in
  `percpu.rs` and the two call sites in `smp.rs`/`main.rs`, so the next attempt
  starts from the scheduling logic in that file. The suspected cause of the hang
  is a task becoming runnable twice at once — queued *and* already being a CPU's
  idle pointer — which a per-task "already queued" assertion would pin down
  before any more rewriting
- [x] Returning the bootloader's memory to the PMM
- [x] A growing kernel heap: a 1 GiB window that starts unmapped and takes
  mebibytes from the PMM as they are needed, announcing the new pages to the
  other CPUs once interrupts are back on. The first 16 MiB of the window are
  mapped before the other CPUs start, because a page-table change is only
  visible after a TLB shootdown and the allocator cannot send one with
  interrupts off

## K1OS on K1K
- [x] K1OS boots on K1K (boot test in CI on every push)
- [ ] Service binaries delivered to `/SVC` as WARP packages
- [ ] A terminal and shell on top of K1K
- [ ] Networking
- [ ] NVIDIA GPU driver as a ring-3 service on top of the GSP firmware (after the nova driver in Linux), delivered by WARP

## Other architectures
K1K runs only on x86_64 today: all platform code (GDT/IDT, APIC, ACPI) is written for it.
- [ ] An `arch/` layer separated from the common kernel code
- [ ] aarch64 in QEMU (`virt`): GIC interrupt controller, ARM timer, boot through Limine over UEFI
- [ ] CI on aarch64
- [ ] Apple Silicon: the AIC interrupt controller, boot through m1n1 and U-Boot
- [ ] RISC-V (riscv64) — once there is real hardware to test on
