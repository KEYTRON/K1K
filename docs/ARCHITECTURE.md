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
masked. Vectors 32..254 that nothing claimed land in `unexpected_irq`, which
just acknowledges them.

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
kernel stack (`CURRENT_KSTACK_TOP`, also mirrored into `TSS.rsp0` for
interrupts), so nested traps and preemption inside syscalls just work.

## Objects, capabilities, IPC

```
Task ── CapTable ── [slot] ── Capability { object: Endpoint, rights: SEND|RECV }
```

`Endpoint` (`ipc/mod.rs`) is a synchronous message channel carrying
`[u64; 4]` plus the sender id. `send` never blocks: if a receiver is blocked
on the endpoint the message is written straight into its inbox and it is
woken; otherwise it is queued (bounded, `EAGAIN` when full). `recv` blocks
until a message arrives.

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
| 5 | `recv` | `slot, buf[4×u64]` | sender task id, `EPERM`, `EFAULT` |
| 6 | `info` | `buf[2×u64]` → `uptime_ms, task_id` | 0 |

Errors: `EPERM = -1`, `EAGAIN = -2`, `EFAULT = -3`, `EINVAL = -4` (reserved),
`ENOSYS = -5`.

User pointers are validated against the lower half and translated through the
task's own page tables before the kernel touches them.

## Testing

`make test` builds an ISO whose `limine.conf` passes `cmdline: autotest`. The
kernel runs the demo services for six seconds, prints a summary and exits QEMU
through `isa-debug-exit` with status 33 on success (`flaky` restarted at least
twice) or 35 on failure. The serial log lands in `build/serial.log`.
