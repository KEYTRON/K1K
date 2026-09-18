# K1K — K1 Kernel

Language: English (primary) | [Русский](README.ru.md)

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
the drivers and the VFS it needs.

## What works today

- Boots via the [Limine](https://github.com/limine-bootloader/limine) protocol
  (BIOS and UEFI), logs to COM1 and to a framebuffer text console.
- GDT/TSS, IDT with exception handlers; kernel traps panic, user traps kill only
  the offending task.
- Physical memory (bitmap PMM over the Limine memory map), kernel page mapping,
  per-task user address spaces sharing the kernel half, kernel heap.
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
- Ring-3 tasks with `syscall`/`sysret`; 16 syscalls: `log`, `exit`, `yield`,
  `sleep`, `send`, `recv`, `info`, `send_cap`, `cap_drop`, `mem_create`,
  `mem_map`, `ep_create`, `mem_create_dma`, `mem_phys`, `dev_info`, `dev_map`.
  All registers except `rax/rcx/r11` are preserved across a syscall.
- Static ELF64 loader: services are ordinary Rust `no_std` programs built
  against the `k1k-rt` runtime crate (`user/`), with R/RX/RW segment
  permissions applied per `PT_LOAD`.
- A supervisor thread that reaps dead tasks and re-instantiates crashed services
  from their image (with backoff).
- Services shipped in the image: `kbd` — the PS/2 keyboard driver running in
  ring 3 (the kernel only forwards scancodes into an endpoint); `blk` — an
  NVMe driver in ring 3 (admin + I/O queues over DMA pages, polled
  completions) that identifies the controller and reads the disk; `hello`;
  `ping`/`pong` — request/reply over endpoints plus a shared page that `pong`
  allocates and grants to `ping` as a capability; and `flaky`, which
  dereferences NULL every third iteration and is brought back without a reboot.
  A service that exits with code 0 is considered finished and not restarted.

```
[superv] nvme 00:03.0 handed to service 'blk'
[   blk] nvme ready: model "QEMU NVMe Ctrl" serial "K1K-NVME-0001", 32768 blocks x 512 B = 16 MiB
[   blk] sector 0: "K1K disk image v1 - read by the ring-3 nvme driver"
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
`xorriso`, `qemu-system-x86_64`, `make`, `git`.

```sh
make            # build kernel + bootable ISO into build/k1k.iso
make run        # boot it in QEMU (BIOS), serial on stdio
make run-uefi   # boot with OVMF
make test       # headless self-test (-smp 4, NVMe disk): supervisor restarts,
                # SMP scheduling and the ring-3 disk read are all asserted
```

`make run`/`make test` attach a 16 MiB raw disk (`build/disk.img`) as an NVMe
controller for the `blk` service.

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

- HPET/TSC clock, inter-processor interrupts (TLB shootdown, remote reschedule).
- IRQ capabilities (interrupt → endpoint) so drivers can stop polling;
  asynchronous notifications.
- A file system on top of `blk` (FAT read-only first), a `fs` server, and
  spawning services from disk — the path to booting K1OS user space.
- Capability revocation; an allocator for `k1k-rt`.

## License

MIT OR Apache-2.0.
