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
the timer vector (32), and hands every other vector to `irq::on_vector`, which
forwards it to the bound endpoint (if any) and acknowledges it. Vector layout:
32 timer, 34..49 ISA IRQs 0..15, 64..127 MSI-X, 255 spurious. On the way out
the stub swaps GS back for ring-3 frames and `iretq`s.

## Memory

| Region | Address | Notes |
|--------|---------|-------|
| User image | from `0x0000_0000_0040_0000` | ELF `PT_LOAD` segments, R / RX / RW per segment flags |
| User stack | below `0x0000_7fff_ffff_0000` | 16 pages, NX |
| HHDM | `0xffff_8000_0000_0000` (Limine-provided) | physical memory direct map |
| Kernel heap | `0xffff_9000_0000_0000` | 1 GiB window, mapped as it grows |
| Kernel image | `0xffffffff80000000` | |

- **PMM** (`mm/pmm.rs`): one bit per 4 KiB frame, bitmap stored in the largest
  usable region. `alloc_frame`, `alloc_zeroed_frame`, `free_frame`.
- **VMM** (`mm/vmm.rs`): the bootloader's page tables are kept as the kernel's.
  A user `AddressSpace` is a fresh PML4 whose entries 256..512 are copied from
  the kernel PML4, so the kernel half is shared and the user half is private.
  Dropping an `AddressSpace` frees every user-half table and frame.
- **Heap** (`mm/heap.rs`): the shared `k1k-alloc` block allocator over a
  1 GiB *window* of address space that holds nothing at boot. When the free list
  cannot serve a request, `FrameSupply` takes at least a mebibyte from the PMM,
  zeroes it and maps it at the top of the window, so the kernel commits the
  memory it actually uses. Two details make that safe:
  - Growth maps pages, which allocates page tables from the PMM — a different
    allocator, so it cannot re-enter the heap. The rule that follows from this
    is that nothing on the heap's critical section may allocate.
  - The other CPUs are not told about the new pages from inside that critical
    section, because an IPI is acknowledged by an interrupt handler and
    interrupts are off. The supplier publishes the range and the allocator
    wrapper announces it once interrupts are back on, which is the only point
    where waiting for acknowledgements is allowed. A request from an interrupt
    handler leaves the range pending for the next allocation that can flush it.
  The heap is guarded by a spin lock *and* by interrupts off, because kernel
  allocations happen on every CPU at once; the lock is never held across
  anything that can block.

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

## Inter-processor interrupts

`arch/x86_64/ipi.rs`. Two things need to reach another CPU: a TLB invalidation
and a nudge back into the scheduler. Both are sent as one broadcast and both are
synchronous — the request is published in every other CPU's per-CPU slot, and
the sender waits until each of them has reported back. A shootdown that only
*asked* would be worth nothing: the point is that when it returns, every CPU has
dropped the mapping.

Delivery is logical with the "all processors except self" shorthand
(`ICR = vector | (2 << 18) | (1 << 11)`). A directed physical IPI is the obvious
alternative and it does not work here: QEMU's local APIC accepts the command and
then drops it for every destination except the sender, so a shootdown would look
like it had worked while doing nothing. Each request carries its own number, so
an acknowledgement can only ever refer to the request that was actually taken.

Requests the kernel sends today:

- `TlbAll { cr3 }` before an address space is freed (`AddressSpace::drop`): the
  frames behind the tables go straight back to the allocator, and a stale entry
  on any CPU would be a stale entry onto somebody else's memory. The supervisor
  already refuses to reap a task that is still on a CPU, so this is the second
  line of defence rather than the only one.
- `SharedRange { start, end }` after a change to the shared kernel half. Those
  mappings carry the global bit, so a CR3 reload would never drop them.
- `Resched` when a task becomes runnable and some other CPU is sitting in `hlt`
  with an empty run queue: the target goes back through the scheduler, which is
  what a timer tick does.
- `Ping`, once, right after the application processors come up. If that does not
  come back from every CPU, IPIs are not reaching them and every later shootdown
  would be silently useless, so the boot log says so out loud.

The boot summary prints, per CPU, how many IPIs it was sent and how many it
acknowledged, and the autotest fails if those differ.

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

Lock discipline: every spinlock that an interrupt handler may take — the
scheduler, endpoints, the interrupt table, the console — and every lock that
code holding one of those may take — the kernel heap and the PMM — is only
ever held with interrupts disabled. Otherwise CPU A could hold the heap with
interrupts on, take a timer tick, and spin on the scheduler lock held by CPU
B, which is itself waiting for the heap. The heap allocator wrapper and the
PMM entry points disable interrupts for exactly that reason, and the tick
handler wakes sleepers without allocating.

## Objects, capabilities, IPC

```
Task ── CapTable ── [slot] ── Capability { object, rights }
                                 object: Endpoint | Notify | Memory | Device | Irq | Port | Control
                                 rights: SEND | RECV | GRANT | MAP_READ | MAP_WRITE | DMA
                                         | SPAWN | SIGNAL | WAIT
```

`Endpoint` (`ipc/mod.rs`) is a synchronous message channel carrying
`[u64; 4]` plus the sender id. `send` never blocks: if a receiver is blocked
on the endpoint the message is written straight into its inbox and it is
woken; otherwise it is queued (bounded, `EAGAIN` when full). `recv` blocks
until a message arrives.

`Notify` (`notify.rs`) is the asynchronous half: a counter of things that
happened, with tasks waiting for the next one. It exists because an endpoint is
the wrong shape for "the device did a thing". A message has to be allocated,
queued and freed on a path that cannot afford it, and a queue that is full loses
the event silently — which is what the `dropped` counter on an interrupt object
was apologising for. A notification has no payload to lose: `signal(n)` records
`n` and wakes up to `n` waiters, and a waiter takes signals off the counter
whenever it gets round to it, so an event that arrives before the wait is still
there afterwards. `wait` comes in three shapes — one signal (`One`, blocking),
one signal or `EAGAIN` (`Poll`), and every pending signal at once (`All`, which
is what a driver that is going to drain the device anyway wants: one wake-up for
whatever number of completions arrived). `SIGNAL` and `WAIT` are separate
rights, so the waiting half of a relationship can be handed out without the
other; the `notify` service is handed two capabilities on one object and checks
exactly that.

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

`IrqObject` (`arch/x86_64/irq.rs`) stands for one interrupt vector: either a
legacy ISA line routed through the I/O APIC (`irq::isa`) or a PCI function's
MSI-X entry 0 pointed at a fresh vector (`irq::msix`, obtained by a driver
with `dev_irq`). `irq_bind` attaches a target, and which kind it is decides
what an interrupt becomes: an **endpoint** gets a message `[vector, count]`,
which is what a driver that has to know *which* interrupt it was needs, while a
**notification** gets a signal, which is what a driver that only has to drain
the device wants — nothing is allocated on the interrupt path and an interrupt
that arrives before the driver is waiting is not lost. Both are sent from the
trap path after the APIC EOI. Level-triggered lines are masked at the I/O APIC
until the driver calls `irq_ack`. `Port` capabilities grant a range of x86 I/O ports for
`port_in`/`port_out` (the 8042 keyboard controller lives at `0x60..0x64`).

`Control` is the kernel's own authority object; a capability to it with
`SPAWN` lets a task turn an ELF image held in a memory object into a new
supervised service (`spawn`). Only `fs` holds one.

Two halves of a channel are two objects. `Endpoint::new_pair` returns a
connected pair: `send` on one half delivers into the other half's queue, and
each half is a capability of its own. This is what makes "let a client talk to
the file service" expressible — `fs` holds the receiving half, the manifest can
hand the sending half to a service, and no right ever has to grow: intersecting
`RECV` with a `SEND` mask would otherwise yield an empty right and a client
that cannot send.

## Drivers and user space in ring 3

The kernel contains no device protocol and no file system:

- `kbd`: holds the interrupt object for ISA IRQ 1 and a port capability for
  the 8042. It creates an endpoint, binds the interrupt to it, and on each
  message drains the controller's output buffer through `port_in` and decodes
  scancode set 1. Nothing about keyboards exists in ring 0.
- `blk`: an NVMe driver. It receives the controller as a `Device` capability,
  maps BAR0, asks for its MSI-X interrupt (`dev_irq`) and binds it to an
  endpoint *before* enabling the controller, allocates DMA pages for the admin
  and I/O queues (created with interrupts enabled on vector 0), identifies the
  controller and namespace, and serves block reads — sleeping on the interrupt
  endpoint while a command is in flight, with a polling fallback if no
  interrupt object is available. A client attaches
  a DMA buffer (`REGISTER_BUF`, capability attached, page count in the high
  half of word 0) and a reply endpoint (`SET_REPLY`), then sends `READ lba
  count`; `blk` answers `status, blocks, block_size` after DMA-ing into the
  buffer. The protocol constants live in `k1k-rt::blkproto`.
- `fs`: read-only FAT12/16/32 (`user/svc/fs/src/fat.rs`) over that protocol
  with a 64 KiB shared DMA buffer. It is also init: it reads
  `/SVC/MANIFEST.TXT` and starts exactly what the manifest lists.

Boot therefore looks like: Limine → kernel → embedded `kbd`/`blk`/`fs`
(and the `ping`/`pong` demo) → `fs` mounts the disk → the rest of user space
comes from the manifest. If a driver crashes, the supervisor restarts it and it
re-initialises its hardware.

### The service manifest

`/SVC/MANIFEST.TXT` is the list of what runs, one service per line:

```text
# name   image          capabilities
hello    HELLO.ELF      fs
flaky    FLAKY.EOF
console  CONSOLE.ELF    fs,control
```

`#` starts a comment, a bad line is reported and skipped rather than stopping
the boot, and a service is started only if it is listed — a file dropped into
`/SVC` stays inert. Image names are paths on the volume; a relative name
resolves under `/SVC`.

Capability names are resolved by `fs` against capabilities it holds itself
(`fs` = the calling half of the file channel, `control` = the spawn authority),
so a manifest can only hand out authority that already exists in the system, and
a typo is an error rather than a silent absence of access. Each granted
capability becomes a slot in the new task numbered in manifest order; the
supervisor passes the mapping to the service in the `caps=` launch argument and
`k1k_rt::granted("fs")` looks it up, so a service never hard-codes a slot
number.

### The file protocol

A client holding the `fs` capability talks to `fs` over the shared buffer it
registers; paths, listings and file contents are bytes in that buffer, so no
service has to trust a pointer and the kernel stays out of the file business.
The wire form is one message each way:

```text
client → fs   w0 = opcode, w1 = bytes written in the request block, w2 = result bytes wanted
fs → client   w0 = status, w1 = arg0, w2 = arg1
```

`k1k-rt` provides both ends: `file::FileClient` (`open`, `read`, `read_all`,
`stat`, `list`, `close`) and `server::Server`, which tracks clients by task and
replies only on the endpoint a client registered. The server's two handshakes —
`CONNECT` (reply endpoint) and `REGISTER_BUF` (shared buffer) — are
independent, so a client may send them in either order.

A third service that wants files therefore needs nothing but its manifest
line: `hello` lists `/SVC`, stats and reads `/README.TXT`, and survives every
failure it can hit (missing capability, no file service, a path that is not
there).

The keyboard IRQ is the first "driver as a message source": the handler pushes
scancodes into a kernel-owned endpoint; the `kbd` service holds the only
`RECV` capability on it and is therefore the keyboard driver — scancode
decoding never runs in ring 0, and if `kbd` crashes the supervisor restarts it.

## Services and supervision

`service/mod.rs` keeps a table of
`ServiceSpec { name, image, grants, dynamic, args }`. `spawn(idx)` builds an
`AddressSpace`, loads the ELF image (`loader/mod.rs`: static ELF64, each
`PT_LOAD` mapped with permissions derived from `p_flags`, addresses validated
against the user range), maps a 64 KiB stack, a 4 MiB heap
(`USER_HEAP_BASE`) and one boot-info page, inserts the granted capabilities and
publishes the task.

The task id is reserved before any of that (`sched::reserve_task_id`), so the
boot-info page can carry the final id and be in place before any CPU can enter
the task — a service must never observe a half-built address space. The page
holds the heap and stack bounds and the launch arguments; `k1k-rt` reads it to
find its heap and its arguments, and its allocator initialises itself from it on
first use, with no setup call in user code.

`services`, the image and the arguments are kept for the lifetime of the kernel,
so a restart re-creates the task with exactly the authority it was given the
first time. Services are built from the `user/` Cargo workspace by
`kernel/build.rs` and embedded with `include_bytes!`. `supervisor_main` runs as
a kernel thread: it reaps `Dead` tasks (freeing stack, address space,
capabilities) and, for tasks that belonged to a service, re-spawns them. After
three restarts a linear backoff (200 ms × n, capped at 3 s) is applied.

`spawn_desc` (syscall 22) is `spawn` plus a list of capabilities and an
argument blob. The descriptor lives in the caller's address space:

```text
word 0  ctl_slot   1  mem_slot    2  size       3  name_ptr
word 4  name_len   5  n_grants    6  arg_ptr    7  arg_len
then n_grants × { slot, rights }
```

Each grant is derived from the caller's own table — `GRANT` required, rights
intersected — so authority is never widened, only handed on. The new task's
capability slots are numbered in the order given, which is what the `caps=`
argument reports.

### The service heap

`k1k-rt` provides the global allocator for every ring-3 program, and it is the
same code as the kernel heap: the `k1k-alloc` crate, first fit over a free list
kept in address order, doubly linked so `dealloc` merges with both neighbours in
constant time, and a `realloc` that grows or shrinks in place whenever the
neighbouring block allows. A service gets one fixed range and a supplier that
has nothing more to give; the kernel heap gets a window and a supplier that
takes pages from the PMM. Two details matter more than they look:

- Block sizes are rounded up to 16 bytes so the "free" flag can live in bit 0
  of the size word; a size that was not a multiple of two would make the flag
  and the size indistinguishable.
- A payload with an alignment above 16 cannot start immediately after the
  header, so `payload - header` is not the header. Each payload therefore keeps
  the distance back to its header in the word below itself, which is what lets
  `dealloc` find the block again.

`make test-heap` compiles that crate straight into a host test and audits the
block list after every single operation — sizes, links, tiling, overlap, the
back-links every live payload must resolve to, the accounting, and that no two
free blocks sit next to each other (a missed coalesce shows up as a heap that
turns down requests it has the memory for). It runs a short churn with the audit
on every step, a long one with it every thousand steps, and a workload shaped
like a real service: fill a 4 MiB heap with 1 KiB blocks, free the bottom half
and ask for a chunk that spans it, then real `Vec`s doubling into the tens of
kilobytes. It is part of CI.

A service that cannot get memory says so with its heap's numbers — region size,
free bytes, block count — rather than a bare allocation failure, because a heap
that ran out of room and a heap whose list is broken need different fixes.

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
| 17 | `irq_bind` | `irq_slot, target_slot` (endpoint or notification) | 0, `EPERM` |
| 18 | `irq_ack` | `irq_slot` | 0, `EPERM` |
| 19 | `dev_irq` | `dev_slot` | new irq slot (`RECV|GRANT`), `EPERM`, `EINVAL` |
| 20 | `port_in` | `port_slot, offset, width` | value, `EPERM`, `EINVAL` |
| 21 | `port_out` | `port_slot, offset, width, value` | 0, `EPERM`, `EINVAL` |
| 22 | `spawn_desc` | `ptr` to the descriptor above (≤ 8 grants, ≤ 1024 argument bytes) | new task id, `EPERM`, `EINVAL`, `EFAULT` |
| 23 | `notify_create` | — | new slot (`SIGNAL|WAIT|GRANT`), `ENOMEM` |
| 24 | `notify_wait` | `slot, mode` (0 one, 1 poll, 2 all) | signals taken, `EAGAIN` from a poll, `EPERM`, `EINVAL` |
| 25 | `notify_signal` | `slot, count` | waiters woken, `EPERM`, `EINVAL` |

Errors: `EPERM = -1`, `EAGAIN = -2`, `EFAULT = -3`, `EINVAL = -4`,
`ENOSYS = -5`, `ENOMEM = -6`.

User pointers are validated against the lower half and translated through the
task's own page tables before the kernel touches them.

## The bootloader's memory

Limine marks the memory it no longer needs as reclaimable — 11 MiB under QEMU,
55 MiB after OVMF — and the kernel used to keep all of it. Giving it back is
only safe once boot is done with Limine, so `boot::init` copies the command line
into the kernel and `pmm::init` records the reclaimable ranges, and
`pmm::reclaim_bootloader` frees them after the application processors are up
(they read Limine's structures as they come up). ACPI is parsed into owned
structures and the framebuffer console copied its geometry before that point, so
nothing reads a response afterwards.

For the reclaim to be worth anything the bitmap has to reach those frames, which
sit *above* RAM; it is therefore sized from the highest usable or reclaimable
address, with a budget of an eighth of the largest usable region. Past that the
extra memory is kept and the log says so. The autotest reads the command line
*after* the reclaim and fails if the reclaim did not happen, so a dangling
pointer to a Limine response cannot slip through unnoticed.

## Testing

`make test` builds an ISO whose `limine.conf` passes `cmdline: autotest`. The
kernel runs the services for eight seconds (`autotest=<seconds>` changes that),
prints a summary and exits QEMU through `isa-debug-exit` with status 33 on
success or 35 on failure. The log must show `fs` mounting the volume, starting
both services from the manifest, serving files, and `hello` reading
`/README.TXT` through the protocol — a boot that starts services but cannot
serve a file is a failure, not a pass. The serial log lands in
`build/serial.log`.

`make test-heap` is the allocator test described above; CI runs both, on BIOS
and through the K1OS boot test on UEFI.
