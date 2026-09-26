//! The per-service heap.
//!
//! The supervisor maps a private heap range into every ring-3 task and writes
//! its bounds into the boot info page; this allocator carves allocations out of
//! that range. A task is never running on two CPUs at once and the kernel never
//! touches a user heap, so the bookkeeping needs no locking.
//!
//! Blocks form a doubly linked list in address order. `alloc` is a first fit
//! (`O(n)` over the blocks), `dealloc` merges with the neighbours in `O(1)`,
//! and `realloc` grows or shrinks in place whenever the neighbouring block is
//! free — which is what makes `Vec` and `String` growth cheap.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;

use crate::boot_info;

/// Payload alignment every service gets for free.
const ALIGN: usize = 16;
const HEADER: usize = core::mem::size_of::<Header>();
/// A tail too small to become a block of its own stays inside its predecessor.
const MIN_SPLIT: usize = HEADER + ALIGN;
/// Where a payload keeps the distance back to its own header. An allocation
/// with an alignment above [`ALIGN`] cannot start right after the header, so
/// `payload - HEADER` would not be the header; this word makes every payload
/// findable from the pointer alone.
const BACK: usize = 8;
/// Bit 0 of `size_flags`: the block is free. Block sizes are always rounded up
/// to a multiple of [`ALIGN`], which keeps that bit clear in every allocated
/// block and keeps the next block's header aligned.
const FREE: usize = 1;

#[repr(C, align(16))]
struct Header {
    /// Total block size, header included, with [`FREE`] in bit 0.
    size_flags: usize,
    next: *mut Header,
    prev: *mut Header,
}

struct Heap {
    /// Address-ordered block list, empty until the first allocation.
    head: *mut Header,
    in_use: usize,
    peak: usize,
    live: usize,
    allocs: usize,
    frees: usize,
    failures: usize,
}

struct HeapCell(UnsafeCell<Heap>);

// Safety: a service's address space runs on one CPU at a time and the kernel
// never dereferences a user heap, so nothing else can observe this cell.
unsafe impl Sync for HeapCell {}

static HEAP: HeapCell = HeapCell(UnsafeCell::new(Heap {
    head: ptr::null_mut(),
    in_use: 0,
    peak: 0,
    live: 0,
    allocs: 0,
    frees: 0,
    failures: 0,
}));

#[inline]
fn align_up(v: usize, to: usize) -> usize {
    (v + to - 1) & !(to - 1)
}

/// Block size needed for `size` bytes at alignment `align`: header, payload,
/// and enough slack to place the payload, rounded up so the next block starts
/// on an [`ALIGN`] boundary.
#[inline]
fn block_for(size: usize, align: usize) -> usize {
    align_up(size + HEADER + (align - 1), ALIGN)
}

/// The header of the block a payload belongs to.
#[inline]
fn header_of(payload: *mut u8) -> *mut Header {
    let back = unsafe { *(payload.sub(BACK) as *const usize) };
    debug_assert!(back >= HEADER && back % ALIGN == 0);
    unsafe { payload.sub(back) as *mut Header }
}

/// Remember, right below a payload, how far back its header is.
#[inline]
fn stamp_back(payload: *mut u8, header: *mut Header) {
    unsafe { *(payload.sub(BACK) as *mut usize) = payload as usize - header as usize };
}

#[inline]
fn block_size(h: *mut Header) -> usize {
    unsafe { (*h).size_flags & !FREE }
}

#[inline]
fn is_free(h: *mut Header) -> bool {
    unsafe { (*h).size_flags & FREE != 0 }
}

/// Carve the heap into a single free block on first use. Nothing to do when the
/// boot info page is missing or was written by a newer kernel: the allocator
/// then fails every request, which surfaces as a panic in the service rather
/// than as silent corruption.
fn init(heap: &mut Heap) {
    if !heap.head.is_null() {
        return;
    }
    let Some(info) = boot_info() else {
        return;
    };
    let base = info.heap_base as usize;
    let size = info.heap_size as usize;
    if size < MIN_SPLIT || !base.is_multiple_of(ALIGN) {
        return;
    }
    let head = base as *mut Header;
    unsafe {
        (*head).size_flags = size | FREE;
        (*head).next = ptr::null_mut();
        (*head).prev = ptr::null_mut();
    }
    heap.head = head;
}

/// Take a block out of the free list, splitting off a free tail when it is big
/// enough to be useful. Returns the block header, or null when nothing fits.
fn take_block(heap: &mut Heap, need: usize) -> *mut Header {
    init(heap);
    let mut cur = heap.head;
    while !cur.is_null() {
        if is_free(cur) && block_size(cur) >= need {
            let total = block_size(cur);
            if total - need >= MIN_SPLIT {
                let after = unsafe { (*cur).next };
                unsafe {
                    let tail = (cur as *mut u8).add(need) as *mut Header;
                    (*tail).size_flags = (total - need) | FREE;
                    (*tail).next = after;
                    (*tail).prev = cur;
                    if !after.is_null() {
                        (*after).prev = tail;
                    }
                    (*cur).size_flags = need;
                    (*cur).next = tail;
                }
            } else {
                unsafe { (*cur).size_flags = total };
            }
            return cur;
        }
        cur = unsafe { (*cur).next };
    }
    ptr::null_mut()
}

/// Return a block to the free list, merging it with free neighbours.
fn give_block(cur: *mut Header) {
    unsafe {
        let total = block_size(cur);
        (*cur).size_flags = total | FREE;
        let next = (*cur).next;
        if !next.is_null() && is_free(next) {
            (*cur).size_flags = (total + block_size(next)) | FREE;
            (*cur).next = (*next).next;
            if !(*cur).next.is_null() {
                (*(*cur).next).prev = cur;
            }
        }
        let prev = (*cur).prev;
        if !prev.is_null() && is_free(prev) {
            (*prev).size_flags = (block_size(prev) + block_size(cur)) | FREE;
            (*prev).next = (*cur).next;
            if !(*cur).next.is_null() {
                (*(*cur).next).prev = prev;
            }
        }
    }
}

/// Split `cur`, leaving a free tail behind when it is worth it. Returns the
/// number of bytes handed back to the free list.
fn split_tail(cur: *mut Header, keep: usize) -> usize {
    let total = block_size(cur);
    if total < keep + MIN_SPLIT {
        return 0;
    }
    unsafe {
        let give = total - keep;
        let after = (*cur).next;
        let tail = (cur as *mut u8).add(keep) as *mut Header;
        (*cur).size_flags = keep;
        (*tail).size_flags = give | FREE;
        (*tail).next = after;
        (*tail).prev = cur;
        if !after.is_null() {
            (*after).prev = tail;
        }
        (*cur).next = tail;
        give
    }
}

pub struct K1kHeap;

/// Report a failed request once, without allocating: `log_bytes` is a plain
/// syscall. Silence would leave a service that ran out of memory with nothing
/// but "allocation of N bytes failed" and no idea how much it had.
fn report_exhaustion(heap: &mut Heap, want: usize) {
    if heap.failures > 3 {
        return;
    }
    let mut msg = [0u8; 160];
    let mut at = 0;
    let put = |msg: &mut [u8], at: &mut usize, s: &[u8]| {
        if *at + s.len() <= msg.len() {
            msg[*at..*at + s.len()].copy_from_slice(s);
            *at += s.len();
        }
    };
    put(&mut msg, &mut at, b"heap: allocation of ");
    at = put_num(&mut msg, at, want);
    put(&mut msg, &mut at, b" bytes failed (");
    at = put_num(&mut msg, at, heap.in_use);
    put(&mut msg, &mut at, b" B in use, peak ");
    at = put_num(&mut msg, at, heap.peak);
    put(&mut msg, &mut at, b" B, failures ");
    at = put_num(&mut msg, at, heap.failures);
    put(&mut msg, &mut at, b")\n");
    crate::log_bytes(&msg[..at]);
}

/// Write `v` in decimal at `at`; returns the offset after the digits.
fn put_num(buf: &mut [u8], at: usize, v: usize) -> usize {
    let mut digits = [0u8; 20];
    let mut n = 0;
    let mut x = v;
    loop {
        digits[n] = b'0' + (x % 10) as u8;
        x /= 10;
        n += 1;
        if x == 0 {
            break;
        }
    }
    let mut at = at;
    for i in (0..n).rev() {
        if at < buf.len() {
            buf[at] = digits[i];
            at += 1;
        }
    }
    at
}

impl K1kHeap {
    fn with<R>(&self, f: impl FnOnce(&mut Heap) -> R) -> R {
        // Safety: see the `Sync` impl above.
        f(unsafe { &mut *HEAP.0.get() })
    }
}

// Safety: `alloc` returns either null or a block from the task's own heap
// range, aligned to at least `layout.align()` and at least `layout.size()`
// bytes long; `dealloc` accepts exactly what `alloc` returned.
unsafe impl GlobalAlloc for K1kHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let align = layout.align().max(ALIGN);
        if !align.is_power_of_two() {
            return ptr::null_mut();
        }
        let need = block_for(layout.size().max(1), align);
        self.with(|heap| {
            let cur = take_block(heap, need);
            if cur.is_null() {
                heap.failures += 1;
                report_exhaustion(heap, layout.size());
                return ptr::null_mut();
            }
            let payload = align_up(cur as usize + HEADER, align) as *mut u8;
            stamp_back(payload, cur);
            heap.in_use += block_size(cur);
            heap.live += 1;
            heap.allocs += 1;
            heap.peak = heap.peak.max(heap.in_use);
            payload
        })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        if ptr.is_null() {
            return;
        }
        let cur = header_of(ptr);
        self.with(|heap| {
            heap.in_use -= block_size(cur);
            heap.live = heap.live.saturating_sub(1);
            heap.frees += 1;
            give_block(cur);
        });
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { self.alloc(layout) };
        if !p.is_null() {
            unsafe { ptr::write_bytes(p, 0, layout.size()) };
        }
        p
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size == 0 {
            unsafe { self.dealloc(ptr, layout) };
            return ptr::null_mut();
        }
        let align = layout.align().max(ALIGN);
        let cur = header_of(ptr);
        let old_total = block_size(cur);
        // The payload does not move, so the block only has to reach from its
        // own header to the end of the new payload. Deriving the size from the
        // requested alignment instead would let a shrink cut the block below
        // data that was already handed out.
        let offset = ptr as usize - cur as usize;
        let need = align_up(offset + new_size.max(1), ALIGN);

        if need <= old_total {
            // Fits already: hand the tail back when it can become a block.
            self.with(|heap| heap.in_use -= split_tail(cur, need));
            return ptr;
        }

        // Needs more room: take over the following block when it is free and
        // big enough — but only as much of it as the new size needs. Swallowing
        // a whole free tail would leave the heap unable to satisfy anything
        // else, which is how a first-fit allocator fragments itself.
        let absorbed = self.with(|_| {
            let next = unsafe { (*cur).next };
            if next.is_null() || !is_free(next) {
                return 0;
            }
            let extra = block_size(next);
            if old_total + extra < need {
                return 0;
            }
            let after = unsafe { (*next).next };
            let want = need - old_total;
            unsafe {
                if extra - want >= MIN_SPLIT {
                    // Keep the rest as a free block.
                    (*cur).size_flags = need;
                    let tail = (cur as *mut u8).add(need) as *mut Header;
                    (*tail).size_flags = (extra - want) | FREE;
                    (*tail).next = after;
                    (*tail).prev = cur;
                    if !after.is_null() {
                        (*after).prev = tail;
                    }
                    (*cur).next = tail;
                    want
                } else {
                    (*cur).size_flags = old_total + extra;
                    (*cur).next = after;
                    if !after.is_null() {
                        (*after).prev = cur;
                    }
                    extra
                }
            }
        });
        if absorbed > 0 {
            self.with(|heap| heap.in_use += absorbed);
            return ptr;
        }

        let fresh = unsafe { self.alloc(Layout::from_size_align_unchecked(new_size, align)) };
        if fresh.is_null() {
            return ptr::null_mut();
        }
        unsafe { ptr::copy_nonoverlapping(ptr, fresh, layout.size().min(new_size)) };
        unsafe { self.dealloc(ptr, layout) };
        fresh
    }
}

#[global_allocator]
static ALLOCATOR: K1kHeap = K1kHeap;

/// Heap counters, for services that want to report their memory use. All
/// zeroes mean the service never allocated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeapStats {
    pub in_use: usize,
    pub peak: usize,
    pub live: usize,
    pub allocs: usize,
    pub frees: usize,
    pub failures: usize,
}

pub fn heap_stats() -> HeapStats {
    let heap = HEAP.0.get();
    unsafe {
        HeapStats {
            in_use: (*heap).in_use,
            peak: (*heap).peak,
            live: (*heap).live,
            allocs: (*heap).allocs,
            frees: (*heap).frees,
            failures: (*heap).failures,
        }
    }
}

/// Bytes still available in the task's heap, or 0 if the allocator never ran.
pub fn heap_free() -> usize {
    let Some(info) = boot_info() else {
        return 0;
    };
    let heap = HEAP.0.get();
    let used = unsafe { (*heap).in_use };
    (info.heap_size as usize).saturating_sub(used)
}
