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

/// How long to wait for the peers, in iterations. A CPU inside a long
/// interrupts-off section delays its answer rather than losing it, so this only
/// has to outlast the longest such section.
const ACK_SPINS: u32 = 8_000_000;

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
    /// Come back to the scheduler: there is work to pick up.
    Resched,
    /// Answer and do nothing else; used by [`selftest`].
    Ping,
}

/// Serialises broadcasts: they are rare (an address space is freed, a kernel
/// mapping changes) and one at a time keeps the acknowledgement bookkeeping
/// unambiguous.
static BROADCAST_LOCK: spin::Mutex<()> = spin::Mutex::new(());

/// Publish `req` to every CPU but this one, send the IPI, and wait for all of
/// them. Must be called with interrupts already disabled, so the CPU the caller
/// is on cannot change underneath the bookkeeping.
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
    for _ in 0..ACK_SPINS {
        if targets
            .iter()
            .flatten()
            .all(|(pc, seq)| pc.ipi_done.load(Ordering::Acquire) >= *seq)
        {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// The IPI entry point: do whatever this CPU was asked to do.
pub fn on_ipi() {
    let pc = percpu::get();
    let pending = pc.ipi_slot.lock().take();
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
            req: Request::Resched,
        }) => {
            pc.resched.store(true, Ordering::Release);
            pc.ipi_done.store(seq, Ordering::Release);
            // Leaving through the scheduler is what a timer tick does, so it is
            // safe wherever we were interrupted.
            crate::sched::schedule();
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
    interrupts::without_interrupts(|| {
        flush_pages(start, end);
        if !broadcast_and_wait(Request::SharedRange { start, end }) {
            klog!("smp", "not every CPU acknowledged a kernel-half flush");
        }
    });
}

/// Drop `cr3` from every CPU's TLB. Call before the page tables it points at
/// are freed or handed to somebody else.
pub fn shootdown_all(cr3: PhysAddr) {
    interrupts::without_interrupts(|| {
        flush_cr3(cr3.as_u64());
        if !broadcast_and_wait(Request::TlbAll { cr3: cr3.as_u64() }) {
            klog!("smp", "not every CPU acknowledged a TLB flush");
        }
    });
}

/// Nudge one CPU to come back to the scheduler. Fire and forget: the target may
/// still be inside a critical section, and by the time it looks there may be
/// nothing left to do.
pub fn kick(cpu: usize) {
    if let Some(pc) = percpu::by_id(cpu)
        && (pc.current == pc.idle_task || pc.resched.load(Ordering::Relaxed))
    {
        let seq = pc.ipi_seq.fetch_add(1, Ordering::AcqRel) + 1;
        *pc.ipi_slot.lock() = Some(Pending {
            seq,
            req: Request::Resched,
        });
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
