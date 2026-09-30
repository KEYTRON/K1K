//! Inter-processor interrupts.
//!
//! Two things need to reach another CPU: a TLB invalidation (somebody changed
//! or freed page tables this CPU may have cached) and a nudge to come back to
//! the scheduler (a task became runnable while this CPU sits in `hlt`).
//!
//! Both are sent as a broadcast and both are *synchronous*: the request is
//! published in every other CPU's own slot, the IPI goes out once, and the
//! sender waits until each of those CPUs has said it is done. That is what
//! makes a shootdown meaningful — when it returns, every CPU has dropped the
//! mapping, not merely been asked to. A reschedule request is handled the same
//! way for simplicity, and costs one interrupt per peer.
//!
//! The request travels *with* its number, so an acknowledgement can only refer
//! to the request that was actually taken. A shared counter that the sender
//! bumped before publishing could be acknowledged by a handler that found
//! nothing to do, and the sender would then wait for an acknowledgement that
//! never comes.

use core::sync::atomic::Ordering;
use x86_64::instructions::interrupts;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::PhysFrame;
use x86_64::{PhysAddr, VirtAddr};

use super::apic;
use super::percpu::{self, MAX_CPUS, PerCpu};
use crate::klog;
use crate::mm::pmm::FRAME_SIZE;

/// Vector used for everything the kernel sends to itself.
///
/// 0x21 is in the range the architecture sets aside for operating-system
/// interrupts: just above the timer at 0x20 and below the I/O APIC range this
/// kernel starts at 35. The 0xF0-0xFF block holds the spurious vector, and
/// firmware is entitled to treat that block as special.
pub const IPI_VECTOR: u8 = 0x21;

/// How long to wait for the peers before giving up on them.
///
/// An iteration is not a time. Under emulation, with more guest processors than
/// the host has cores, eight million of them is seconds during which this CPU
/// neither sleeps nor takes an interrupt — and a peer the host has not scheduled
/// yet needs about that long to come and say it has flushed.
const ACK_SPINS: u32 = 8_000_000;

/// Spins before the wait starts halting instead of spinning.
const SPIN_BEFORE_HALT: u32 = 4_096;

/// A request together with the number its sender is waiting for.
#[derive(Clone, Copy)]
pub struct Pending {
    pub seq: u32,
    pub req: Request,
}

/// What the peers are being asked to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Request {
    /// Invalidate everything the address space with this CR3 can reach.
    TlbAll { cr3: u64 },
    /// Invalidate a range of the shared kernel half, which is present in every
    /// address space and therefore flushed regardless of the loaded CR3.
    SharedRange { start: u64, end: u64 },
    /// Answer and do nothing else; used by [`selftest`].
    Ping,
}

/// Serialises broadcasts: they are rare (an address space is freed, a kernel
/// mapping changes) and one at a time keeps the acknowledgement bookkeeping
/// unambiguous.
static BROADCAST_LOCK: spin::Mutex<()> = spin::Mutex::new(());

/// Publish `req` to every CPU but this one, send the IPI, and wait for all of
/// them.
///
/// Interrupts stay **on** for the whole of this, and that is the point rather
/// than a detail.
///
/// A CPU that waits with interrupts off cannot take the interrupt that clears
/// its own APIC's delivery-status bit, so one long wait leaves the APIC busy:
/// the next shootdown this CPU wants to send cannot be handed to the APIC at
/// all, because `send_broadcast_ipi` will not queue a second command behind one
/// that has not been taken. Nor can this CPU answer anybody else's shootdown
/// while it spins. So a single slow flush turns into a machine-wide one, and
/// the kernel stops being able to bring its own new pages to the other
/// processors — which is what a growing heap needs, several times in a row, on
/// every boot.
///
/// With interrupts on, this CPU keeps sweeping its sleepers, keeps running its
/// own tasks, can answer a peer's shootdown while it waits, and — once the peers
/// have had their chance — can halt until the tick that arrives anyway, rather
/// than occupying a host core the peer it is waiting for has not been given.
///
/// The bookkeeping does not need interrupts off. Every shared slot is behind its
/// own lock and the sequence numbers are atomics, so an interrupt landing in the
/// middle of publishing cannot be seen half-done by the other side: the reader
/// either takes the whole request or none of it, and the sender's own view of
/// what it is waiting for does not change under it.
fn broadcast_and_wait(req: Request) -> bool {
    let _guard = BROADCAST_LOCK.lock();
    let me = percpu::cpu_id();
    let mut targets: [Option<(&'static PerCpu, u32)>; MAX_CPUS] = [None; MAX_CPUS];
    let mut peers = 0usize;
    for cpu in 0..percpu::count() {
        if cpu as u32 == me {
            continue;
        }
        let Some(pc) = percpu::by_id(cpu) else {
            continue;
        };
        let seq = pc.ipi_seq.fetch_add(1, Ordering::AcqRel) + 1;
        *pc.ipi_slot.lock() = Some(Pending { seq, req });
        targets[cpu] = Some((pc, seq));
        peers += 1;
    }
    if peers == 0 {
        return true;
    }
    for _ in 0..ACK_SPINS {
        if apic::send_broadcast_ipi(IPI_VECTOR) {
            break;
        }
        // The APIC holds one command at a time: an earlier IPI has not been
        // taken yet. Wait for it rather than dropping this one.
        core::hint::spin_loop();
    }
    let mut spins = 0u32;
    loop {
        if targets
            .iter()
            .flatten()
            .all(|(pc, seq)| pc.ipi_done.load(Ordering::Acquire) >= *seq)
        {
            return true;
        }
        spins += 1;
        if spins > ACK_SPINS {
            return false;
        }
        if spins < SPIN_BEFORE_HALT || !interrupts::are_enabled() {
            // Before the interrupts are on — the boot-time self-test runs before
            // that — halting here would be halting with nothing to wake it.
            core::hint::spin_loop();
        } else {
            x86_64::instructions::hlt();
        }
    }
}

/// The IPI entry point: do whatever this CPU was asked to do.
pub fn on_ipi() {
    let pc = percpu::get();
    // A reschedule is a flag, not an entry in the work slot, and it is taken
    // before the slot: the work in the slot is the thing its sender is waiting
    // to have acknowledged.
    let resched = pc.resched.swap(false, Ordering::AcqRel);
    // The slot is written by the sender under its own lock, and read here under
    // the reader's: a lock held across an interrupt on the same CPU would wait
    // for itself.
    let pending = interrupts::without_interrupts(|| pc.ipi_slot.lock().take());
    match pending {
        Some(Pending {
            seq,
            req: Request::TlbAll { cr3 },
        }) => {
            flush_cr3(cr3);
            pc.ipi_done.store(seq, Ordering::Release);
        }
        Some(Pending {
            seq,
            req: Request::SharedRange { start, end },
        }) => {
            flush_pages(start, end);
            pc.ipi_done.store(seq, Ordering::Release);
        }
        Some(Pending {
            seq,
            req: Request::Ping,
        }) => {
            pc.ipi_done.store(seq, Ordering::Release);
        }
        // Nothing pending: a duplicate, or a request already taken.
        None => {}
    }
    // Only where returning is an ordinary `ret`; see `sched::may_switch_here`.
    // Otherwise the request waits for this CPU's next syscall, which is where
    // it is acted on.
    if resched && crate::sched::may_switch_here() {
        crate::sched::schedule();
    }
    apic::eoi();
}

/// `invlpg` every page of `[start, end)` on this CPU.
fn flush_pages(start: u64, end: u64) {
    let mut addr = start & !(FRAME_SIZE - 1);
    while addr < end {
        unsafe { core::arch::asm!("invlpg [{addr}]", addr = in(reg) addr) };
        addr += FRAME_SIZE;
    }
}

/// Drop everything the address space with this CR3 can reach. Global (kernel)
/// entries survive a CR3 write, which is why kernel-half changes go through
/// [`flush_shared_range`] instead.
fn flush_cr3(cr3: u64) {
    let (frame, flags) = Cr3::read();
    if frame.start_address().as_u64() != cr3 {
        return;
    }
    unsafe { Cr3::write(PhysFrame::containing_address(PhysAddr::new(cr3)), flags) };
}

/// Flush a range of the shared kernel half on every CPU.
///
/// The kernel half is mapped with the global bit and shared by every address
/// space, so neither a CR3 reload nor a user-space shootdown would ever drop
/// those entries: they have to be invalidated by hand wherever they changed.
pub fn flush_shared_range(start: VirtAddr, end: VirtAddr) {
    let (start, end) = (start.as_u64(), end.as_u64());
    flush_pages(start, end);
    if !broadcast_and_wait(Request::SharedRange { start, end }) {
        klog!("smp", "not every CPU acknowledged a kernel-half flush");
    }
}

/// Drop `cr3` from every CPU's TLB. Call before the page tables it points at
/// are freed or handed to somebody else.
pub fn shootdown_all(cr3: PhysAddr) {
    flush_cr3(cr3.as_u64());
    if !broadcast_and_wait(Request::TlbAll { cr3: cr3.as_u64() }) {
        klog!("smp", "not every CPU acknowledged a TLB flush");
    }
}

/// Nudge one CPU to come back to the scheduler. Fire and forget: the target may
/// still be inside a critical section, and by the time it looks there may be
/// nothing left to do.
pub fn kick(cpu: usize) {
    // A reschedule is a *flag*, not an entry in the work slot. The slot holds
    // one request at a time, and what is in it is a shootdown its sender is
    // waiting to have acknowledged: a hint that only wants a CPU to look at its
    // run queue has no business taking that request's place, and no business
    // taking the lock that serialises them either — this runs on the path that
    // wakes a task.
    //
    // Swapping rather than storing means a CPU that has already been asked is
    // not asked again, so a run of wakes cannot turn into a run of IPIs.
    if let Some(pc) = percpu::by_id(cpu)
        && (pc.current == pc.idle_task || !pc.resched.swap(true, Ordering::AcqRel))
    {
        apic::send_broadcast_ipi(IPI_VECTOR);
    }
}

/// Ask every other CPU to answer. Run once the application processors are up:
/// if this fails, IPIs are not reaching them and every later shootdown would
/// silently do nothing.
pub fn selftest() -> bool {
    interrupts::without_interrupts(|| broadcast_and_wait(Request::Ping))
}

/// Requests published to and acknowledged by this CPU, for the boot summary.
pub fn stats(pc: &PerCpu) -> (u32, u32) {
    (
        pc.ipi_seq.load(Ordering::Relaxed),
        pc.ipi_done.load(Ordering::Relaxed),
    )
}
