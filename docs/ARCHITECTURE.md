# K1K architecture

Language: English | [Русский](ARCHITECTURE.ru.md)

## Design goals

1. **A small privileged core.** Only what must run in ring 0 does: scheduling,
   address spaces, IPC, capabilities, interrupt entry. A bug there is fatal
   (`kernel panic`), so the surface is kept small.
2. **Everything else is a service.** Drivers, file systems and higher-level
   subsystems run in ring 3 with their own address space. A crash kills that
   task only; the supervisor re-instantiates it. No reboot, no user action.
3. **Capabilities, not ambient authority.** A task addresses kernel objects only
   through slots in its capability table. Each slot carries a rights mask,
   checked on every syscall. There are no UIDs and no global namespaces to
   guess names in.
4. **Fast paths where they matter.** Message hand-off to a blocked receiver is
   direct (no queue copy), the syscall path is `syscall`/`sysret`, and the plan
   is shared-memory channels for bulk data rather than message copies.

## Boot

Limine (protocol base revision 3) loads the ELF at `0xffffffff80000000`, sets
up 4-level paging with a Higher Half Direct Map (HHDM) of all usable memory,
masks the PICs/IOAPIC, and jumps to `kmain` with interrupts disabled.

`kmain` order: serial → GDT/TSS → IDT → framebuffer console → PMM → VMM →
heap → scheduler → IPC → syscall MSRs → ACPI/APIC → interrupts on →
supervisor → services.

## Interrupts

`arch/x86_64/acpi.rs` takes the RSDP from Limine (a physical address under
base revision 3, so the tables are mapped into the HHDM on demand), walks the
RSDT or XSDT and parses the MADT: local APIC address, enabled CPUs, I/O APICs
and ISA interrupt-source overrides.

`arch/x86_64/apic.rs` enables the BSP's local APIC (spurious vector `0xFF`),
calibrates its timer against PIT channel 2 (20 ms one-shot, gate on port
`0x61`) and runs it in periodic mode at `TIMER_HZ` (1000). Every I/O APIC
redirection entry is masked first; `route_isa_irq` then programs the entry for
a legacy IRQ, applying polarity/trigger from a MADT override if present. The
legacy 8259 PICs are remapped away from the exception vectors and fully
masked.

Every IDT vector points at a generated asm stub (`trap_stubs.s`) that pushes
`(error code, vector)`, saves all general-purpose registers, does `swapgs` if
the trap came from ring 3, and calls `trap_dispatch(&mut TrapFrame)`
(`trap.rs`). The dispatcher routes exceptions (kill the user task or panic),
the timer and keyboard vectors, and acknowledges anything else. On the way
out the stub swaps GS back for ring-3 frames and `iretq`s.

## Memory

| Region | Address | Notes |
|--------|---------|-------|
| User image | from `0x0000_0000_0040_0000` | ELF `PT_LOAD` segments, R / RX / RW per segment flags |
| User stack | below `0x0000_7fff_ffff_0000` | 16 pages, NX |
| HHDM | `0xffff_8000_0000_0000` (Limine-provided) | physical memory direct map |
| Kernel heap | `0xffff_9000_0000_0000` | 16 MiB, mapped at init |
| Kernel image | `0xffffffff80000000` | |

- **PMM** (`mm/pmm.rs`): one bit per 4 KiB frame, bitmap stored in the largest
  usable region. `alloc_frame`, `alloc_zeroed_frame`, `free_frame`.
- **VMM** (`mm/vmm.rs`): the bootloader's page tables are kept as the kernel's.
  A user `AddressSpace` is a fresh PML4 whose entries 256..512 are copied from
  the kernel PML4, so the kernel half is shared and the user half is private.
  Dropping an `AddressSpace` frees every user-half table and frame.
- **Heap** (`mm/heap.rs`): `linked_list_allocator` over a fixed mapped window.

## Tasks and scheduling

Every task — kernel thread or ring-3 service — has a 64 KiB kernel stack.
Switching happens only between kernel stacks (`arch/x86_64/context.rs`):
callee-saved registers are pushed, `rsp` is saved into the task, the next
task's `rsp` is loaded, its registers popped, `ret`. A fresh task's stack is
pre-filled so that `ret` lands in `task_trampoline`, which enables interrupts
and calls the entry function.

The scheduler (`sched/mod.rs`) is round-robin with a 10-tick (10 ms) quantum
at 1000 Hz.
On each timer tick sleepers whose deadline passed become ready; if the current
quantum is exhausted and something is ready, `schedule()` runs inside the IRQ
handler (the interrupted frame stays on that task's kernel stack).

Task states: `Ready`, `Running`, `Blocked` (waiting on an endpoint),
`Sleeping`, `Dead` (waiting for the supervisor to reap it).

Ring 3 is entered from the task's kernel thread with `iretq`
(`context::enter_user`). On a syscall the CPU switches to the current task's
kernel stack (`PerCpu.kstack_top`, also mirrored into this CPU's `TSS.rsp0`
for interrupts), so nested traps and preemption inside syscalls just work.

## SMP and per-CPU state

The bootstrap processor discovers the others through the Limine MP response
and releases them one at a time (`smp.rs`). Before an AP starts, the BSP
allocates its `CpuTables` (GDT, TSS, IST stack) and `PerCpu` block; the AP
loads them, installs the shared IDT, programs its syscall MSRs, registers its
boot context as its idle task, starts its local APIC timer and enables
interrupts. From then on it is just another CPU pulling work from the run
queue.

`PerCpu` (`percpu.rs`) is reached through the GS base: in kernel mode
`GS_BASE` points at the block, while a task runs in ring 3 the bases are
swapped (`swapgs` in the syscall stub, the trap stubs and `enter_user`), so
user code cannot see or clobber the kernel pointer. `CR4.FSGSBASE` is kept
clear. The block holds the current task, the idle task, the kernel stack top
for syscall entry, the TSS pointer and a context-switch counter.

Cross-CPU correctness rests on two rules in `sched/mod.rs`: a task being
switched away from is requeued only by `finish_switch`, which runs on the new
context after the old stack is no longer in use; and `wake` marks a task
`Ready` but does not enqueue it while `on_cpu` is set — `finish_switch`
enqueues it then. The supervisor likewise reaps a dead task only once it has
left its CPU. `Endpoint::recv` registers, marks itself blocked and switches
away inside a single interrupts-off section, so a sender on another CPU cannot
lose the wake-up. Wall time advances only on the BSP's timer tick; every CPU's
tick drives its own preemption.

## Objects, capabilities, IPC

```
Task ── CapTable ── [slot] ── Capability { object, rights }
                                 object: Endpoint | Memory | Device | Control
                                 rights: SEND | RECV | GRANT | MAP_READ | MAP_WRITE | DMA | SPAWN
```

`Endpoint` (`ipc/mod.rs`) is a synchronous message channel carrying
`[u64; 4]` plus the sender id. `send` never blocks: if a receiver is blocked
on the endpoint the message is written straight into its inbox and it is
woken; otherwise it is queued (bounded, `EAGAIN` when full). `recv` blocks
until a message arrives.

A message may carry one capability (`send_cap`). The sender must hold `GRANT`
on it; the copy the receiver gets is `rights ∩ mask` and is inserted into the
receiver's table on `recv`, which reports the new slot in word 3 (`NO_CAP` =
`u64::MAX` when nothing was attached). This is the only way authority moves
between tasks — there is no global namespace to look objects up in.

`MemoryObject` (`obj/mod.rs`) is a set of physical frames. `mem_create`
allocates one (the creator gets `MAP_READ|MAP_WRITE|GRANT`), `mem_map` maps it
into the caller's address space at a kernel-chosen address above
`0x10_0000_0000` (read-only unless the capability has `MAP_WRITE`). Frames go
back to the PMM when the last capability and the last mapping are gone;
tearing down an address space unmaps shared pages without freeing them.
`mem_create_dma` allocates physically contiguous frames and adds the `DMA`
right, which unlocks `mem_phys` — the physical address a device needs.

`DeviceObject` wraps a PCI function found by the boot-time scan (`pci.rs`,
which also sizes the BARs and enables memory decoding + bus mastering when a
function is granted). `dev_info` describes it; `dev_map` maps a memory BAR
uncached into the caller's address space. Device pages are tracked like shared
pages so teardown never tries to free MMIO "frames".

`Control` is the kernel's own authority object; a capability to it with
`SPAWN` lets a task turn an ELF image held in a memory object into a new
supervised service (`spawn`). Only `fs` holds one.

## Drivers and user space in ring 3

The kernel contains no device protocol and no file system:

- `kbd`: the IRQ handler only pushes scancodes into an endpoint; decoding
  lives in the service, which holds the `RECV` capability.
- `blk`: an NVMe driver. It receives the controller as a `Device` capability,
  maps BAR0, allocates DMA pages for the admin and I/O queues, identifies the
  controller and namespace, and serves block reads to clients — polled
  completions for now (IRQ capabilities are the next step). A client attaches
  a DMA buffer (`REGISTER_BUF`, capability attached, page count in the high
  half of word 0) and a reply endpoint (`SET_REPLY`), then sends `READ lba
  count`; `blk` answers `status, blocks, block_size` after DMA-ing into the
  buffer. The protocol constants live in `k1k-rt::blkproto`.
- `fs`: read-only FAT12/16/32 (`user/svc/fs/src/fat.rs`) over that protocol
  with a 64 KiB shared DMA buffer. It is also init: it lists `/SVC`, reads
  each ELF into a memory object and calls `spawn`, which registers a dynamic
  service (image retained in kernel memory so the supervisor can restart it)
  and starts it.

Boot therefore looks like: Limine → kernel → embedded `kbd`/`blk`/`fs`
(and the `ping`/`pong` demo) → `fs` mounts the disk → the rest of user space
comes from `/SVC`. If a driver crashes, the supervisor restarts it and it
re-initialises its hardware.

The keyboard IRQ is the first "driver as a message source": the handler pushes
scancodes into a kernel-owned endpoint; the `kbd` service holds the only
`RECV` capability on it and is therefore the keyboard driver — scancode
decoding never runs in ring 0, and if `kbd` crashes the supervisor restarts it.

## Services and supervision

`service/mod.rs` keeps a table of `ServiceSpec { name, image, grants }`.
`spawn` builds an `AddressSpace`, loads the ELF image (`loader/mod.rs`:
static ELF64, each `PT_LOAD` mapped with permissions derived from `p_flags`,
addresses validated against the user range), maps a 64 KiB stack, creates the
task and inserts the granted capabilities. Services are built from the
`user/` Cargo workspace by `kernel/build.rs` and embedded with `include_bytes!`. `supervisor_main` runs as a
kernel thread: it reaps `Dead` tasks (freeing stack, address space,
capabilities) and, for tasks that belonged to a service, re-spawns them. After
three restarts a linear backoff (200 ms × n, capped at 3 s) is applied.

Faults in ring 3 (`#PF`, `#GP`, `#UD`, …) are routed by the IDT handlers to
`sched::on_user_fault`, which marks the task dead and schedules away. The same
exception in ring 0 is a kernel panic.

## Syscall ABI (x86_64)

`rax` = number, arguments in `rdi`, `rsi`, `rdx`, `r10`; result in `rax`.
`rcx` and `r11` are clobbered by the instruction itself; every other register
is preserved by the kernel. Negative results are errors. Rust programs use the
wrappers in `user/rt` (`k1k-rt`).

| # | Name | Arguments | Result |
|---|------|-----------|--------|
| 0 | `log` | `ptr, len` (≤ 4096) | 0 |
| 1 | `exit` | `code` | never returns |
| 2 | `yield` | — | 0 |
| 3 | `sleep` | `ms` | 0 |
| 4 | `send` | `slot, w0, w1, w2` | 0, `EPERM`, `EAGAIN` |
| 5 | `recv` | `slot, buf[4×u64]` (word 3 = received cap slot or `NO_CAP`) | sender task id, `EPERM`, `EFAULT` |
| 6 | `info` | `buf[2×u64]` → `uptime_ms, task_id` | 0 |
| 7 | `send_cap` | `ep_slot, cap_slot, rights_mask, w0` | 0, `EPERM`, `EAGAIN` |
| 8 | `cap_drop` | `slot` | 0, `EINVAL` |
| 9 | `mem_create` | `pages` (1..=1024) | new slot, `EINVAL`, `ENOMEM` |
| 10 | `mem_map` | `slot, writable` | base address, `EPERM`, `ENOMEM` |
| 11 | `ep_create` | — | new slot (`SEND|RECV|GRANT`), `ENOMEM` |
| 12 | `mem_create_dma` | `pages` (1..=64, contiguous) | new slot (`+DMA`), `EINVAL`, `ENOMEM` |
| 13 | `mem_phys` | `slot` | physical address, `EPERM`, `EINVAL` |
| 14 | `dev_info` | `slot, buf[14×u64]` | 0, `EPERM`, `EFAULT` |
| 15 | `dev_map` | `slot, bar` | base address, `EPERM`, `EINVAL`, `ENOMEM` |
| 16 | `spawn` | `ctl_slot, mem_slot, size, name_ptr \| len<<48` | new task id, `EPERM`, `EINVAL`, `EFAULT` |

Errors: `EPERM = -1`, `EAGAIN = -2`, `EFAULT = -3`, `EINVAL = -4`,
`ENOSYS = -5`, `ENOMEM = -6`.

User pointers are validated against the lower half and translated through the
task's own page tables before the kernel touches them.

## Testing

`make test` builds an ISO whose `limine.conf` passes `cmdline: autotest`. The
kernel runs the demo services for six seconds, prints a summary and exits QEMU
through `isa-debug-exit` with status 33 on success (`flaky` restarted at least
twice) or 35 on failure. The serial log lands in `build/serial.log`.
