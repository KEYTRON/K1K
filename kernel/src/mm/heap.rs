//! The kernel heap: a window of virtual address space that grows into physical
//! memory as it is used.
//!
//! The old heap mapped a fixed 16 MiB up front, which had to be a number
//! guessed at boot — too small and the kernel runs out, too large and it wastes
//! address space and startup time on frames nobody asks for. Here the window is
//! a gibibyte of *address space* and nothing more: the allocator asks
//! [`FrameSupply`] for pages only when the free list cannot satisfy a request,
//! and the supplier takes them from the PMM, zeroes them and maps them at the
//! top of the window.
//!
//! The bookkeeping, the block list and `Vec`-friendly in-place growth all live
//! in `k1k-alloc`, shared with the ring-3 service heaps, so there is one
//! allocator to test and one set of tests.
//!
//! # Locking
//!
//! [`GlobalAlloc`] can be called from any context at any time, including from
//! an interrupt handler and on any CPU, so the heap is guarded twice: interrupts
//! off, which keeps an interrupt on *this* CPU from taking the lock it already
//! holds, and a spin lock for the other CPUs. The lock is only ever held for the
//! length of one block-list operation, and nothing in that section can block, so
//! waiting for it with interrupts off is safe.
//!
//! That has one consequence worth spelling out: the heap cannot call anything
//! that allocates. Growing it maps pages, and mapping allocates page tables from
//! the PMM — which is a different allocator, so it is fine, but the rule is the
//! reason the supplier takes no locks of its own beyond the PMM's.

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicU64, Ordering};

use k1k_alloc::{GlobalHeap, Region, Supply};
use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::instructions::interrupts;

use super::pmm;
use super::vmm::{self, Flags};
use crate::klog;

/// Start of the heap window. Everything the kernel heap ever uses lives here.
pub const HEAP_START: u64 = vmm::KERNEL_HEAP_START;

/// How much address space the heap may grow into. It is only a limit: pages
/// come out of the PMM as they are needed, so a kernel that allocates a
/// megabyte never maps a gigabyte.
const HEAP_RESERVE: usize = 1024 * 1024 * 1024;

/// The least the heap takes in one go. A mebibyte is a compromise: smaller bites
/// map more regions (and each one costs an IPI to announce), larger ones commit
/// memory a small kernel never asks for. It also keeps a service-sized workload
/// — a page table, a task struct, a few buffers — from walking the PMM once per
/// kilobyte.
const HEAP_GROW_MIN: usize = 1024 * 1024;

/// The most it will take in one go, so one huge request cannot make the kernel
/// map everything the PMM has left before the caller is ready for it.
const HEAP_GROW_MAX: usize = 16 * 1024 * 1024;

/// The range the supplier has mapped and the other CPUs have not been told
/// about. The allocator is single-threaded per call, so a pair of plain atomics
/// is enough to hand the range from the critical section to the code that can
/// afford to wait for acknowledgements.
struct PendingFlush {
    start: AtomicU64,
    end: AtomicU64,
}

static HEAP_TO_FLUSH: PendingFlush = PendingFlush {
    start: AtomicU64::new(0),
    end: AtomicU64::new(0),
};

/// Whether the window may be mapped at all.
///
/// The PMM and the page tables have to exist first, and `heap::init` runs after
/// both. Anything that allocated before that would be mapping on top of
/// structures that are not there yet, so the supplier refuses instead.
static HEAP_READY: AtomicU64 = AtomicU64::new(0);

/// Feeds the heap out of physical memory.
struct FrameSupply {
    top: usize,
    limit: usize,
    mapped: usize,
}

impl FrameSupply {
    const fn new() -> Self {
        Self {
            top: HEAP_START as usize,
            limit: (HEAP_START as usize).wrapping_add(HEAP_RESERVE),
            mapped: 0,
        }
    }
}

impl Supply for FrameSupply {
    fn reserve(&mut self, bytes: usize) {
        self.top += bytes;
        self.mapped += bytes;
    }

    fn supply(&mut self, want: usize) -> Option<Region> {
        if HEAP_READY.load(Ordering::Acquire) == 0 {
            return None;
        }
        let page = pmm::FRAME_SIZE as usize;
        let left = self.limit.checked_sub(self.top)?;
        if left < page {
            return None;
        }
        // Enough to satisfy the request, rounded to whole pages, within the
        // bounds of the window and the per-growth cap.
        let bytes = want
            .max(HEAP_GROW_MIN)
            .min(HEAP_GROW_MAX)
            .min(left)
            .next_multiple_of(page);
        let base = self.top;
        let pages = bytes / page;
        // No logging anywhere on this path: it runs with interrupts off inside
        // the allocator, and anything that formats could allocate.
        vmm::map_kernel_pages_deferred(
            VirtAddr::new(base as u64),
            pages,
            Flags::WRITABLE | Flags::NO_EXECUTE,
        )
        .ok()?;
        self.top += bytes;
        self.mapped += bytes;
        // The other CPUs will have to be told, but not from here: an IPI is
        // acknowledged by an interrupt handler, and interrupts are off. Only the
        // new pages need flushing — the ones before them were announced when
        // they were mapped.
        HEAP_TO_FLUSH.end.store(self.top as u64, Ordering::Release);
        HEAP_TO_FLUSH.start.store(base as u64, Ordering::Release);
        Some(Region { base, len: bytes })
    }
}

/// The kernel heap.
pub struct KernelHeap {
    heap: GlobalHeap<FrameSupply>,
    /// Held across every allocator call, with interrupts off.
    lock: Mutex<()>,
}

impl KernelHeap {
    const fn new() -> Self {
        Self {
            heap: GlobalHeap::new(FrameSupply::new()),
            lock: Mutex::new(()),
        }
    }

    /// Tell the CPUs that have not seen the newest pages about them.
    fn flush_growth(&self) {
        // Called with interrupts on. An allocation from an interrupt handler
        // leaves the range pending instead: the next one that can afford to
        // wait for acknowledgements flushes it.
        if !interrupts::are_enabled() {
            return;
        }
        let start = HEAP_TO_FLUSH.start.swap(0, Ordering::AcqRel);
        let end = HEAP_TO_FLUSH.end.swap(0, Ordering::AcqRel);
        if end != 0 {
            vmm::flush_shared_range(VirtAddr::new(start), VirtAddr::new(end));
        }
    }

    /// One allocator operation: the heap is off limits to everyone else for as
    /// long as it takes, interrupts included, and the pages the supplier mapped
    /// on the way are announced afterwards.
    fn with<R>(&self, f: impl FnOnce(&mut k1k_alloc::Heap<FrameSupply>) -> R) -> R {
        let out = interrupts::without_interrupts(|| {
            let _guard = self.lock.lock();
            // Safety: the lock above is the only thing that touches the heap,
            // and interrupts are off so nothing on this CPU can join in.
            unsafe { self.heap.with(f) }
        });
        self.flush_growth();
        out
    }
}

unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.with(|h| h.alloc(layout.size(), layout.align()))
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        // Safety: the caller guarantees `ptr` came from this allocator.
        self.with(|h| unsafe { h.dealloc(ptr) });
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Safety: the caller guarantees `ptr` came from this allocator with
        // `layout` as its current size.
        self.with(|h| unsafe { h.realloc(ptr, layout.size(), layout.align(), new_size) })
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        self.with(|h| {
            let p = h.alloc(layout.size(), layout.align());
            if !p.is_null() {
                // Safety: `h.alloc` just returned `layout.size()` bytes.
                unsafe { core::ptr::write_bytes(p, 0, layout.size()) };
            }
            p
        })
    }
}

#[global_allocator]
pub static HEAP: KernelHeap = KernelHeap::new();

/// Statistics for the boot log and the autotest.
pub fn stats() -> k1k_alloc::Stats {
    HEAP.with(|h| h.stats())
}

/// How much of the window is mapped before any other CPU is running.
///
/// Growing the heap installs new page-table levels, and a page-table change is
/// only visible to the other CPUs after a TLB shootdown — which cannot be sent
/// from inside the allocator, where interrupts are off and an IPI would never be
/// acknowledged. So the part of the window that the kernel is going to need
/// while it is still single-CPU is mapped here, and the CPUs come up onto a heap
/// that has room. Everything after this point grows with the flush the allocator
/// wrapper sends once interrupts are back on.
const HEAP_AT_BOOT: usize = 16 * 1024 * 1024;

pub fn init() {
    HEAP_READY.store(1, Ordering::Release);
    // One CPU, interrupts off: nothing to announce, and nothing that could be
    // looking at the kernel half of the page tables yet.
    let pages = HEAP_AT_BOOT / pmm::FRAME_SIZE as usize;
    let mapped = vmm::map_kernel_pages_deferred(
        VirtAddr::new(HEAP_START),
        pages,
        Flags::WRITABLE | Flags::NO_EXECUTE,
    );
    if let Err(e) = mapped {
        klog!(
            "heap",
            "could not map the first {} MiB: {e:?}",
            HEAP_AT_BOOT / 1024 / 1024
        );
    } else {
        // One CPU so far, so there is nothing to tell: the kernel half of the
        // page tables is not shared with anybody yet.
        vmm::flush_shared_range(
            VirtAddr::new(HEAP_START),
            VirtAddr::new(HEAP_START + HEAP_AT_BOOT as u64),
        );
        HEAP.with(|h| {
            if h.add_region(k1k_alloc::Region {
                base: HEAP_START as usize,
                len: HEAP_AT_BOOT,
            }) {
                h.reserve_supply(HEAP_AT_BOOT);
            }
        });
    }
    klog!(
        "heap",
        "window at {:#x}, up to {} MiB, {} KiB per growth",
        HEAP_START,
        HEAP_RESERVE / 1024 / 1024,
        HEAP_GROW_MIN / 1024
    );
}
