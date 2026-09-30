# K1K — K1 Kernel

Language: English (primary) | [Русский](README.ru.md)

[![K1K CI](https://github.com/KEYTRON/K1K/actions/workflows/ci.yml/badge.svg)](https://github.com/KEYTRON/K1K/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/KEYTRON/K1K?include_prereleases&label=release)](https://github.com/KEYTRON/K1K/releases)
[![K1OS on K1K](https://github.com/KEYTRON/K1OS/actions/workflows/k1os-k1k.yml/badge.svg)](https://github.com/KEYTRON/K1OS/actions/workflows/k1os-k1k.yml)

| Where it boots | Status |
|----------------|--------|
| QEMU q35, BIOS (Limine), 4 CPUs, NVMe | CI boot test on every push |
| QEMU q35, UEFI (OVMF) | verified on every milestone |
| As the kernel of [K1OS](https://github.com/KEYTRON/K1OS) | CI boot test in the K1OS repo |

K1K is a from-scratch, hybrid, capability-based operating system kernel for
x86_64, written in Rust (`no_std`). It is **not** a Linux fork and not UNIX-like
by design: the goal is to take the best ideas from several families —

| From | Idea taken |
|------|------------|
| Linux | fast paths and pragmatism: one privileged core, no unnecessary layers between the kernel and the hardware |
| Windows NT / macOS XNU | hybrid structure: a small privileged core plus supervised subsystems |
| seL4 / Fuchsia (Zircon) | capabilities instead of UIDs/ACLs: a task can only touch objects it holds a handle to |
| MINIX 3 / QNX | self-healing: services run isolated in ring 3 and are restarted by a supervisor when they crash |
| Redox OS | Rust for memory safety in the kernel itself |

The kernel core owns scheduling, address spaces, IPC and capabilities.
Everything else — drivers, file systems, network, GUI — is meant to run as
isolated, restartable services.

## K1K and K1OS

K1K is the kernel of [K1OS](https://github.com/KEYTRON/K1OS) — the "1" is the
OS generation, not a version 0.x placeholder, hence `1.0.0-alpha` here. K1OS
today boots a Linux kernel with its own package manager,
[WARP](https://github.com/KEYTRON/WARP); the plan is to migrate K1OS onto K1K
step by step (boot → services → WARP-delivered user space) as the kernel grows
the drivers and the VFS it needs. The first step is in place: the kernel boots
with only its drivers and `fs` embedded and loads the rest of user space from
a FAT volume.

## What works today

- Boots via the [Limine](https://github.com/limine-bootloader/limine) protocol
  (BIOS and UEFI), logs to COM1 and to a framebuffer text console.
- GDT/TSS, IDT with exception handlers; kernel traps panic, user traps kill only
  the offending task.
- Physical memory (bitmap PMM over the Limine memory map), kernel page mapping,
  per-task user address spaces sharing the kernel half, a growing kernel
  heap.
- ACPI (RSDP → RSDT/XSDT → MADT), Local APIC timer calibrated against the
  PIT, I/O APIC routing with MADT interrupt overrides; legacy PICs disabled.
- Preemptive round-robin scheduler (LAPIC timer @ 1 kHz, 10 ms quantum),
  kernel threads, sleep/block/wake.
- SMP: application processors are brought up through the Limine MP protocol,
  each with its own GDT/TSS, per-CPU block (GS base, `swapgs` on every
  kernel entry/exit) and idle task; one shared run queue, tasks migrate freely.
- Capability tables (`object + rights`, slot-indexed) and synchronous message
  endpoints with direct hand-off to a blocked receiver. A message can carry a
  capability (rights ∩ mask, `GRANT` required) — the only way authority moves
  between tasks.
- Memory objects: shareable sets of frames that tasks create, hand over as
  capabilities and map into their own address space (`MAP_READ`/`MAP_WRITE`);
  DMA variants are physically contiguous and expose their physical address.
- Device capabilities: the kernel enumerates PCI, sizes the BARs and hands a
  function to a driver task, which maps the MMIO BARs itself. The kernel never
  touches the device.
- Interrupt and port capabilities: an interrupt object (ISA line through the
  I/O APIC, or a PCI function's MSI-X entry) delivers each interrupt as a
  message on an endpoint the driver chose; a port capability grants a range
  of x86 I/O ports. The kernel has no keyboard or disk code at all.
- Inter-processor interrupts: a broadcast with per-CPU acknowledgement, used to
  invalidate TLB entries before an address space is freed or a shared kernel
  mapping changes, and to pull an idle CPU back into the scheduler when a task
  becomes runnable. The boot test fails if any CPU does not acknowledge.
- The bootloader's memory goes back to the allocator once boot is done with it
  (11 MiB under QEMU, 55 MiB after OVMF): the command line is copied out first,
  and the test proves it by reading the copy afterwards.
- Connected endpoints: a pair of endpoints where sending on one half delivers
  to the other. A server keeps the receiving half and hands out the sending
  one, so authority over a channel can be delegated without ever widening
  rights — intersecting a `RECV` right with a `SEND` mask would leave nothing.
- Ring-3 tasks with `syscall`/`sysret`; 23 syscalls: `log`, `exit`, `yield`,
  `sleep`, `send`, `recv`, `info`, `send_cap`, `cap_drop`, `mem_create`,
  `mem_map`, `ep_create`, `mem_create_dma`, `mem_phys`, `dev_info`, `dev_map`,
  `spawn`, `irq_bind`, `irq_ack`, `dev_irq`, `port_in`, `port_out`,
  `spawn_desc`. All registers except `rax/rcx/r11` are preserved across a
  syscall.
- Every ring-3 task gets a private 4 MiB heap and a boot-info page: the
  supervisor maps both before the task can run and tells the runtime where the
  heap is, which launch arguments the spawner attached and which slot each
  granted capability ended up in.
- User space comes from disk: the kernel image embeds only the drivers and
  the file-system server; `fs` mounts the FAT volume, reads
  `/SVC/MANIFEST.TXT` and starts exactly the services listed there (a `Control`
  capability with the `SPAWN` right is the authority to do so). Dropping a file
  into `/SVC` no longer runs it. Crashed disk-loaded services are restarted
  from the retained image, with the same capabilities and arguments, like any
  other.
- A service starts with the capabilities its manifest line names: `spawn_desc`
  hands the new task a list of `(slot, rights)` pairs derived from the spawner's
  own table, so a service can only ever pass on authority that already exists,
  and never more than it holds.
- The file service is a real server: any service holding the `fs` capability
  can `open`, `read`, `stat` and `list` over a shared buffer, with the server
  tracking each client by task and replying only on the endpoint that client
  registered.
- Static ELF64 loader: services are ordinary Rust `no_std` programs built
  against the `k1k-rt` runtime crate (`user/`), with R/RX/RW segment
  permissions applied per `PT_LOAD`.
- A supervisor thread that reaps dead tasks and re-instantiates crashed services
  from their image (with backoff).
- Services embedded in the kernel image: `kbd` — the PS/2 keyboard driver in
  ring 3 (IRQ 1 arrives on its endpoint, scancodes are read through a port
  capability); `blk` — an NVMe driver in ring 3 (admin + I/O queues over DMA
  pages, MSI-X completion interrupts, polling fallback) serving block reads
  over IPC into a client-provided DMA buffer; `fs` — read-only FAT12/16/32 on
  top of `blk`, doubling as init;
  `ping`/`pong` — request/reply over endpoints plus a shared page that `pong`
  grants to `ping` as a capability.
- Services on the disk (`/SVC`): `hello`, which lists `/SVC` and reads
  `/README.TXT` through the file protocol using the capability its manifest line
  granted it, and `flaky`, which dereferences NULL every third iteration and is
  brought back without a reboot. A service that exits with code 0 is considered
  finished and not restarted.
- A heap for services: `k1k-rt` turns the per-task heap into a global allocator
  (first fit, free list in address order, in-place `realloc`), so services can
  use `Box`, `Vec`, `String` and `format!`. It is tested on the host against an
  independent reading of its own block list (`make test-heap`).
- Real clocks: a time base chosen at boot from the TSC (measured against the PIT,
  used only where the CPU vouches that it is invariant), the HPET (found in ACPI
  and on the PCI bus, its rate counted rather than decoded, its timers masked
  before the counter is switched on), or the tick counter as the fallback — never
  assumed. `uptime_ms()` and the `time` syscall come from it, and the ping/pong
  round trip is timed with it.
- Capability revocation: a spawner gets a receipt saying which slot each of its
  grants landed in, `cap_revoke` takes one back, and the slot stays dead
  afterwards — a revoked number never names a different object. A task may
  always revoke its own; taking authority out of *another* task needs `Control`,
  and the authority is checked before the task is looked up, so the answer does
  not leak which tasks exist. Tearing a task down uses the same primitive.
- Asynchronous notifications: a `Notify` object is a counter of things that
  happened with tasks waiting for the next one. A signal that arrives before the
  wait is still there afterwards, so a driver cannot miss an interrupt the way a
  full message queue loses one — and `SIGNAL` and `WAIT` are separate rights, so
  the waiting half of a relationship can be handed out on its own. The
  keyboard driver in ring 3 takes its interrupts this way.
- Sized for 512 logical processors — the biggest x86 part available today has
  256 cores and 512 threads. That means 32-bit local apic ids end to end, MADT
  entry type 9 parsed, and the id and the interrupt command going through the
  x2APIC MSRs when the CPU reports the feature rather than only claiming it in
  `IA32_APIC_BASE` (QEMU does the second without the first, and the MSR read is
  a #GP with no way back). What is *not* done: starting more than 255 of them,
  because the bootloader's processor tables name processors with eight-bit ids,
  and delivering an interrupt to a processor past 255, because an I/O APIC
  redirect and an MSI-X entry both have eight bits for the destination. Both are
  on the roadmap with the reasons, and the kernel logs the gap when it sees it
  rather than working around it quietly.
- Waiting for the other processors with interrupts **on**: a TLB shootdown has to
  ask every CPU to invalidate something and wait for it to say it has, and a CPU
  that waits with interrupts off cannot take the interrupt that clears its own
  APIC's delivery-status bit — so one long wait leaves the APIC busy, the next
  shootdown cannot be handed to it at all, and a CPU spinning like that can
  neither answer anybody else's request nor sweep its own sleepers. One slow
  flush becomes a machine-wide one, and the growing heap needs one on every boot,
  several times in a row, with every processor already running. While waiting,
  a CPU keeps sweeping its sleepers and can answer a peer, and once the peers
  have had their chance it halts until the tick that arrives anyway rather than
  holding a host core that the processor it waits for has not been given.
- One run queue per CPU: the scheduler gives every processor its own ready queue
  and its own list of sleeping tasks, so a switch from one task to the next does
  not go through a lock the whole machine shares; a CPU with nothing to do takes
  a task off a neighbour's queue instead. The task table is still there for
  spawning, waking, reaping and looking a task up by name, none of which are on
  the way from one task to the next. A task is in exactly one place at a time —
  one queue, one CPU, or one CPU's idle context — and that is one word per task
  with a compare-and-swap on every move, so a scheduler that put one task in two
  places would say so at the move that did it instead of letting two CPUs run it
  on one stack.
- Four rules the scheduler keeps, and what each one is worth. Waking a task that
  is still on another CPU leaves it to that CPU, which notices on the way out that
  the task is Ready and puts it back: queueing it from the waker as well is how a
  task ends up in two queues and gets run by two CPUs on one stack, and taking
  that rule away makes the placement check fire within seconds. Nothing that can
  wait for another CPU runs under a lock — a task is built before the table is
  locked, because a heap that grows announces its new pages and waits for the
  other CPUs to acknowledge — and every lock is taken with interrupts off, so a
  handler on this CPU cannot end up waiting for a lock its own thread holds. A
  task is not switched away from inside an interrupt taken while it was in user
  code, because the saved stack pointer would land in the middle of the trap
  frame; that tick waits for the task's next syscall instead, and about 20 of them
  do so in an eight-second run. The tick handler allocates nothing, sweeping the
  sleeper list in place instead: an allocation there is enough to make a run fail.
  The last three are hazards the single-queue scheduler had too and never hit at
  four processors — one global queue serialises exactly the races that make them
  matter — so they are argued from the code rather than from a reproduction.
- One allocator for both: the kernel heap and the service heaps are the same
  `k1k-alloc` crate. The kernel heap starts with nothing mapped and takes
  mebibytes from the physical allocator as it needs them, announces the new
  pages to the other CPUs once interrupts are back on, and is guarded by a spin
  lock and by interrupts off. The first 16 MiB of the window are mapped before
  the other CPUs start, so a page-table change never has to be announced from
  inside the allocator, where an IPI could not be acknowledged.

```
[superv] nvme 00:03.0 handed to service 'blk'
[   blk] nvme ready: model "QEMU NVMe Ctrl" serial "K1K-NVME-0001", 32768 blocks x 512 B = 16 MiB
[   blk] task 10 attached a 64 KiB DMA buffer at 0x1119000
[    fs] mounted Fat16 volume "K1KDISK": 32481 clusters x 512 B
[superv] spawned 'hello' from disk as task 13 (41 KiB ELF)
[ hello] hello service up (ring 3, task 13, Rust ELF)
[superv] service 'flaky' crashed (code -1) -> restarting (restart #1)
```

```
[ flaky] about to dereference NULL...
[ fault] task 6 'flaky' #PF addr=0x0 code=0x4 rip=0x40007d -> killed
[superv] service 'flaky' crashed (code -1) -> restarting (restart #1)
[superv] service 'flaky' back up as task 9 at 1220 ms
[ flaky] flaky service started
```

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and the
syscall ABI.

## Building

Requirements: Rust nightly (`rustup` picks it up from `rust-toolchain.toml`),
`xorriso`, `mtools` (FAT disk image), `qemu-system-x86_64`, `make`, `git`.

```sh
make            # build kernel + bootable ISO into build/k1k.iso
make run        # boot it in QEMU (BIOS), serial on stdio
make run-uefi   # boot with OVMF
make test       # headless self-test (-smp 4, NVMe disk): supervisor restarts,
                # SMP scheduling and the ring-3 disk read are all asserted
```

`make run`/`make test` attach a 16 MiB FAT16 disk (`build/disk.img`, built with
mtools: `/SVC/*.ELF` + `/README.TXT`) as an NVMe controller for the `blk`
service; `make disk` rebuilds just the image.

The first build clones the Limine binaries into `third_party/limine`. The
kernel's `build.rs` builds the `user/` workspace and embeds the service ELFs,
so `make` (or `cargo build` inside `kernel/`) is enough.

Interactive run: `make run`, then type in the QEMU window — the `kbd` service
echoes each line you finish with Enter into the log.

## Layout

```
kernel/           Rust kernel crate (x86_64-unknown-none, build-std)
  src/arch/x86_64 GDT, IDT, ACPI, LAPIC/IOAPIC, serial, context switch, syscall entry
  src/mm          pmm (frames), vmm (page tables / address spaces), heap
  src/sched       tasks and the scheduler
  src/obj         capabilities
  src/ipc         endpoints
  src/syscall     dispatcher + user memory access
  src/service     service specs and the supervisor
  src/loader      static ELF64 loader
  src/console     framebuffer console + logging macros
user/             ring-3 workspace: rt/ (k1k-rt runtime), svc/* (services), user.ld
tools/            ISO builder, font generator
limine.conf       bootloader configuration
```

## Roadmap (short)

- A file-service protocol (open/read over IPC) so spawned services can read
  files themselves; passing capabilities to spawned services; a service
  manifest on disk instead of "everything in /SVC".
- A scheduler without one global lock; WARP packages as the way service binaries
  reach `/SVC`.

## License

MIT OR Apache-2.0.
