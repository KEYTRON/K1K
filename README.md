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

## What works today

- Boots via the [Limine](https://github.com/limine-bootloader/limine) protocol
  (BIOS and UEFI), logs to COM1 and to a framebuffer text console.
- GDT/TSS, IDT with exception handlers; kernel traps panic, user traps kill only
  the offending task.
- Physical memory (bitmap PMM over the Limine memory map), kernel page mapping,
  per-task user address spaces sharing the kernel half, kernel heap.
- Preemptive round-robin scheduler (PIT @ 200 Hz), kernel threads, sleep/block/wake.
- Capability tables (`object + rights`, slot-indexed) and synchronous message
  endpoints with direct hand-off to a blocked receiver.
- Ring-3 tasks with `syscall`/`sysret`; syscalls: `log`, `exit`, `yield`,
  `sleep`, `send`, `recv`, `info`.
- A supervisor thread that reaps dead tasks and re-instantiates crashed services
  from their image (with backoff).
- Demo services (flat nasm binaries embedded at build time): `hello`,
  `ping`/`pong` over IPC endpoints, and `flaky` — which dereferences NULL every
  third iteration and is brought back without a reboot.

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
`nasm`, `xorriso`, `qemu-system-x86_64`, `make`, `git`.

```sh
make            # build kernel + bootable ISO into build/k1k.iso
make run        # boot it in QEMU (BIOS), serial on stdio
make run-uefi   # boot with OVMF
make test       # headless self-test: exits 0 when the supervisor restarted `flaky`
```

The first build clones the Limine binaries into `third_party/limine`.

## Layout

```
kernel/           Rust kernel crate (x86_64-unknown-none, build-std)
  src/arch/x86_64 GDT, IDT, PIC/PIT, serial, context switch, syscall entry
  src/mm          pmm (frames), vmm (page tables / address spaces), heap
  src/sched       tasks and the scheduler
  src/obj         capabilities
  src/ipc         endpoints
  src/syscall     dispatcher + user memory access
  src/service     service specs and the supervisor
  src/console     framebuffer console + logging macros
user/             ring-3 programs (nasm) and the ABI header user/lib/k1k.inc
tools/            ISO builder, font generator
limine.conf       bootloader configuration
```

## Roadmap (short)

- APIC/IOAPIC + HPET, SMP bring-up.
- ELF loader for services, a real user-space runtime.
- Shared-memory IPC for bulk data; asynchronous notifications.
- Move drivers out of the kernel: keyboard is already just an IRQ → endpoint;
  next PCI, AHCI/virtio, a VFS server.
- Capability derivation/revocation syscalls.

## License

MIT OR Apache-2.0.
