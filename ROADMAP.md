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
- [x] A scheduler without a single global lock: one run queue per CPU, work
  stealing with a bounded number of queues tried, per-CPU sleeper lists, and the
  task table kept for the slow paths only. A task is in exactly one place at a
  time and that is one compare-and-swap per move, so a task that would end up
  in two is caught at the transition that did it. Waking a task that is still
  on another CPU leaves it to that CPU, nothing that can wait for another CPU
  runs under a lock, and a task is never suspended inside a trap taken from
  ring 3 — a tick that arrives while a task is in user code waits for its next
  syscall
- [x] Returning the bootloader's memory to the PMM
- [x] A growing kernel heap: a 1 GiB window that starts unmapped and takes
  mebibytes from the PMM as they are needed, announcing the new pages to the
  other CPUs once interrupts are back on. The first 16 MiB of the window are
  mapped before the other CPUs start, because a page-table change is only
  visible after a TLB shootdown and the allocator cannot send one with
  interrupts off

- [ ] Preempting ring-3 code at an arbitrary point, rather than at a syscall:
  resuming a task that was interrupted in user code means unwinding the trap
  frame (`ret_from_user` in Linux terms) instead of a plain `ret`

## K1OS on K1K
- [x] K1OS boots on K1K (boot test in CI on every push)
- [ ] Service binaries delivered to `/SVC` as WARP packages
- [ ] A terminal and shell on top of K1K
- [ ] Networking
- [ ] NVIDIA GPU driver as a ring-3 service on top of the GSP firmware (after the nova driver in Linux), delivered by WARP

## Scale
- [x] A ceiling worth the name: arrays sized for 512 logical processors (the
  largest x86 part available has 256 cores and 512 threads), 32-bit apic ids end
  to end, MADT entry type 9 parsed, and the id and interrupt command through the
  x2APIC MSRs when the CPU reports the feature. Verified at 4, 8 and 16
  processors under QEMU and on the host's own processor under KVM
- [ ] More than 255 logical processors actually starting: the bootloader's
  processor tables name processors with eight-bit ids, so the wake-up needs to
  be done by hand (write the 32-bit id, INIT, two SIPIs) and validated on a
  socket that has one
- [ ] Interrupts addressed to a processor past 255: I/O APIC redirects and
  MSI-X entries have eight bits for the destination, so those ids need either
  the compatibility path spelled out or a different delivery route
- [ ] Boot at that size: starting 511 processors one INIT/SIPI at a time takes
  minutes, a per-CPU 32 KiB double-fault stack is 16 MiB of address space, and
  every TLB shootdown waits on 512 acknowledgements

## Other architectures
K1K runs only on x86_64 today: all platform code (GDT/IDT, APIC, ACPI) is written for it.
- [ ] An `arch/` layer separated from the common kernel code
- [ ] aarch64 in QEMU (`virt`): GIC interrupt controller, ARM timer, boot through Limine over UEFI
- [ ] CI on aarch64
- [ ] Apple Silicon: the AIC interrupt controller, boot through m1n1 and U-Boot
- [ ] RISC-V (riscv64) — once there is real hardware to test on
