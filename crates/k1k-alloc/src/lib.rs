//! The K1K allocator, shared by the kernel heap and by ring-3 services.
//!
//! One algorithm, one set of tests, two users:
//!
//! - The kernel gives it a reserved virtual window and lets it grow, asking
//!   the physical allocator for more frames when the free list runs dry.
//! - A service gives it the range the supervisor mapped for it, and gets a
//!   global allocator out of it.
//!
//! Blocks form a doubly linked list in address order. `alloc` is a first fit
//! (`O(n)` over the blocks), `dealloc` merges with both neighbours in `O(1)`,
//! and `realloc` grows or shrinks in place whenever the neighbouring block
//! allows — which is what makes `Vec` and `String` growth cheap.
//!
//! Two details are easy to get wrong and both are load-bearing:
//!
//! - Block sizes are rounded up to [`ALIGN`], so bit 0 of the size word can
//!   carry the "free" flag without a size that is not a multiple of two
//!   becoming indistinguishable from a free block.
//! - A payload whose alignment exceeds [`ALIGN`] cannot start right after the
//!   header, so `payload - HEADER` is not the header. Every payload therefore
//!   keeps the distance back to its own header in the word below itself, and
//!   that is what lets `dealloc` find the block again.

#![no_std]

use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::ptr;

/// Payload alignment every user gets for free.
pub const ALIGN: usize = 16;

/// A contiguous run of memory the allocator may hand out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub base: usize,
    pub len: usize,
}

/// Where new regions come from when the free list cannot satisfy a request.
pub trait Supply {
    /// Return at least `want` bytes of fresh memory, or `None` when there is
    /// none left. The memory must be zeroed and stay valid for as long as the
    /// heap exists.
    fn supply(&mut self, want: usize) -> Option<Region>;

    /// Tell the supplier that the first `bytes` of what it hands out are already
    /// there, so one that fills a window from the bottom can start above them.
    /// A supplier with no window of its own ignores this.
    fn reserve(&mut self, _bytes: usize) {}

    /// How many regions this supplier has handed over. Used by the tests to
    /// check that growth actually happened.
    fn supplied(&self) -> usize {
        0
    }
}

/// A supplier that has nothing more to give: a service heap is the range the
/// supervisor mapped, and asking for more is a syscall, not a decision this
/// crate can make.
pub struct Fixed;

impl Supply for Fixed {
    fn supply(&mut self, _want: usize) -> Option<Region> {
        None
    }
}

/// Regions are tracked in an array, not a `Vec`: the allocator cannot use a
/// `Vec` before it has memory to put one in.
///
/// The kernel heap grows a mebibyte at a time into a gibibyte window, so the
/// limit has to be in the thousands rather than the dozens: a heap that ran out
/// of *slots* would look exactly like a heap that ran out of memory, and would
/// do it a few mebibytes in.
pub const MAX_REGIONS: usize = 1024;

const FREE: usize = 1;
const HEADER: usize = core::mem::size_of::<Header>();
const MIN_SPLIT: usize = HEADER + ALIGN;
/// Word below a payload holding the distance back to its header.
const BACK: usize = 8;

#[repr(C, align(16))]
struct Header {
    /// Total block size, header included, with [`FREE`] in bit 0.
    size_flags: usize,
    next: *mut Header,
    prev: *mut Header,
    /// Distance from a payload at `this + HEADER` back to this header. It has
    /// to be the *last* field: the payload of a block with no alignment slack
    /// starts right after the header, so the word below the payload is this
    /// one, and anything else here would be overwritten.
    back: usize,
}

/// Counters, for callers that want to report memory use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub in_use: usize,
    pub peak: usize,
    pub live: usize,
    pub allocs: usize,
    pub frees: usize,
    pub failures: usize,
    /// Regions in use and how many bytes they cover.
    pub regions: usize,
    pub region_bytes: usize,
}

pub struct Heap<S: Supply> {
    regions: [Region; MAX_REGIONS],
    n_regions: usize,
    head: *mut Header,
    supply: S,
    in_use: usize,
    peak: usize,
    live: usize,
    allocs: usize,
    frees: usize,
    failures: usize,
}

#[inline]
fn align_up(v: usize, to: usize) -> usize {
    (v + to - 1) & !(to - 1)
}

/// Block size needed for `size` bytes at alignment `align`: header, payload, and
/// enough slack to place the payload, rounded so the next block starts on an
/// [`ALIGN`] boundary.
#[inline]
fn block_for(size: usize, align: usize) -> usize {
    align_up(size + HEADER + (align - 1), ALIGN)
}

#[inline]
fn block_size(h: *mut Header) -> usize {
    unsafe { (*h).size_flags & !FREE }
}

#[inline]
fn is_free(h: *mut Header) -> bool {
    unsafe { (*h).size_flags & FREE != 0 }
}

/// The header of the block a payload belongs to.
#[inline]
fn header_of(payload: *mut u8) -> *mut Header {
    let back = unsafe { *payload.sub(BACK).cast::<usize>() };
    debug_assert!(back >= HEADER && back % ALIGN == 0);
    unsafe { payload.sub(back) as *mut Header }
}

#[inline]
fn stamp_back(payload: *mut u8, header: *mut Header) {
    unsafe { *payload.sub(BACK).cast::<usize>() = payload as usize - header as usize };
}

/// Put a free block of `size` at `at`, between `cur` and `after`.
///
/// If `after` is free as well it is absorbed: two free blocks side by side
/// would leave the heap refusing requests the free memory could serve, which is
/// how a first-fit allocator fills up while sitting on plenty of space.
unsafe fn link_free(cur: *mut Header, at: *mut Header, size: usize, mut after: *mut Header) {
    let mut size = size;
    if !after.is_null() && is_free(after) {
        size += block_size(after);
        after = unsafe { (*after).next };
    }
    unsafe {
        (*at).size_flags = size | FREE;
        (*at).next = after;
        (*at).prev = cur;
        if !after.is_null() {
            (*after).prev = at;
        }
        (*cur).next = at;
    }
}

impl<S: Supply> Heap<S> {
    pub const fn new(supply: S) -> Self {
        Self {
            regions: [Region { base: 0, len: 0 }; MAX_REGIONS],
            n_regions: 0,
            head: ptr::null_mut(),
            supply,
            in_use: 0,
            peak: 0,
            live: 0,
            allocs: 0,
            frees: 0,
            failures: 0,
        }
    }

    /// Hand the heap a region. Safe to call more than once: later regions come
    /// from the supplier.
    ///
    /// A region that starts exactly where the last block ends — which is what a
    /// bump allocator in one contiguous arena hands out — is folded into that
    /// block when the block is free, because a block reaching into the new
    /// region would have its header overwritten. A *live* tail cannot grow, so
    /// that case gets a block of its own at the start of the new region.
    pub fn add_region(&mut self, region: Region) -> bool {
        if region.len < MIN_SPLIT || region.base % ALIGN != 0 || self.n_regions >= MAX_REGIONS {
            return false;
        }
        if self.overlaps(region) {
            return false;
        }
        self.regions[self.n_regions] = region;
        self.n_regions += 1;

        let last = self.tail();
        if !last.is_null() && last as usize + block_size(last) == region.base && is_free(last) {
            let merged = block_size(last) + region.len;
            unsafe { (*last).size_flags = merged | FREE };
            return true;
        }
        let first = region.base as *mut Header;
        unsafe {
            (*first).size_flags = region.len | FREE;
            (*first).prev = last;
            (*first).next = ptr::null_mut();
            if last.is_null() {
                self.head = first;
            } else {
                (*last).next = first;
            }
        }
        true
    }

    /// Whether `region` shares memory with one we already hold.
    fn overlaps(&self, region: Region) -> bool {
        self.regions[..self.n_regions]
            .iter()
            .any(|r| region.base < r.base + r.len && r.base < region.base + region.len)
    }

    /// The last block of the list, or null when it is empty.
    fn tail(&self) -> *mut Header {
        if self.head.is_null() {
            return ptr::null_mut();
        }
        let mut cur = self.head;
        loop {
            let next = unsafe { (*cur).next };
            if next.is_null() {
                return cur;
            }
            cur = next;
        }
    }

    pub fn stats(&self) -> Stats {
        Stats {
            in_use: self.in_use,
            peak: self.peak,
            live: self.live,
            allocs: self.allocs,
            frees: self.frees,
            failures: self.failures,
            regions: self.n_regions,
            region_bytes: self.regions[..self.n_regions].iter().map(|r| r.len).sum(),
        }
    }

    /// Bytes the heap could still hand out.
    pub fn free_bytes(&self) -> usize {
        self.stats().region_bytes.saturating_sub(self.in_use)
    }

    /// How many blocks the list holds, free and used together. Diagnostics: a
    /// heap with free bytes but a single block that no longer looks free is a
    /// list that something has scribbled on, not a full heap.
    pub fn blocks(&self) -> usize {
        let mut n = 0;
        let mut cur = self.head;
        while !cur.is_null() {
            n += 1;
            unsafe { cur = (*cur).next };
            if n > 1 << 20 {
                break;
            }
        }
        n
    }

    /// Hand out the first `bytes` of the supplier's own memory straight away, by
    /// adding them as a region. What the kernel does for the part of the window
    /// it maps before anything else can run.
    pub fn reserve_supply(&mut self, bytes: usize) {
        self.supply.reserve(bytes);
    }

    /// The regions handed to this heap so far.
    pub fn regions(&self) -> &[Region] {
        &self.regions[..self.n_regions]
    }

    /// How many times the supplier was asked for more.
    pub fn supply_count(&self) -> usize {
        self.supply.supplied()
    }

    /// Allocate `size` bytes aligned to `align`, or null when the heap is
    /// empty and the supplier has nothing more.
    pub fn alloc(&mut self, size: usize, align: usize) -> *mut u8 {
        let align = align.max(ALIGN);
        if !align.is_power_of_two() {
            return ptr::null_mut();
        }
        let want = size.max(1);
        let need = block_for(want, align);
        match self.take_block(need) {
            Some(cur) => {
                let payload = align_up(cur as usize + HEADER, align) as *mut u8;
                stamp_back(payload, cur);
                self.in_use += block_size(cur);
                self.live += 1;
                self.allocs += 1;
                self.peak = self.peak.max(self.in_use);
                payload
            }
            None => {
                // Ask for more, then try again. A region that is too small to
                // be worth a block on its own is refused rather than kept.
                if let Some(region) = self.supply.supply(need)
                    && self.add_region(region)
                {
                    return self.alloc(want, align);
                }
                self.failures += 1;
                ptr::null_mut()
            }
        }
    }

    /// Return a payload from [`Heap::alloc`] with the same size and alignment.
    /// # Safety
    /// `ptr` must be a payload this heap returned and not yet freed.
    pub unsafe fn dealloc(&mut self, ptr: *mut u8) {
        if ptr.is_null() {
            return;
        }
        let cur = header_of(ptr);
        self.in_use -= block_size(cur);
        self.live = self.live.saturating_sub(1);
        self.frees += 1;
        self.give_block(cur);
    }

    /// Resize a payload in place where possible, moving it otherwise. The
    /// first `min(old, new)` bytes are preserved.
    ///
    /// # Safety
    /// `ptr` must be a live payload of this heap, `old_size` its current size.
    pub unsafe fn realloc(
        &mut self,
        ptr: *mut u8,
        old_size: usize,
        align: usize,
        new_size: usize,
    ) -> *mut u8 {
        // Safety: the caller guarantees `ptr` is a live payload of this heap.
        unsafe {
            if new_size == 0 {
                self.dealloc(ptr);
                return ptr::null_mut();
            }
            let align = align.max(ALIGN);
            let cur = header_of(ptr);
            let old_total = block_size(cur);
            // The payload does not move, so the block only has to reach from its
            // own header to the end of the new payload. Deriving the size from the
            // requested alignment instead would let a shrink cut the block below
            // data that was already handed out.
            let offset = ptr as usize - cur as usize;
            let need = align_up(offset + new_size.max(1), ALIGN);

            if need <= old_total {
                self.in_use -= self.split_tail(cur, need);
                return ptr;
            }

            // Needs more room: take over the following block when it is free and
            // big enough — but only as much of it as the new size needs. Swallowing
            // a whole free tail would leave the heap unable to satisfy anything
            // else, which is how a first-fit allocator fragments itself.
            let absorbed = {
                let next = (*cur).next;
                if next.is_null() || !is_free(next) {
                    0
                } else {
                    let extra = block_size(next);
                    if old_total + extra < need {
                        0
                    } else {
                        let after = (*next).next;
                        let take = need - old_total;
                        if extra - take >= MIN_SPLIT {
                            // Keep the rest of the neighbour as a free block.
                            (*cur).size_flags = need;
                            let tail = (cur as *mut u8).add(need) as *mut Header;
                            link_free(cur, tail, extra - take, after);
                            take
                        } else {
                            (*cur).size_flags = old_total + extra;
                            (*cur).next = after;
                            if !after.is_null() {
                                (*after).prev = cur;
                            }
                            extra
                        }
                    }
                }
            };
            if absorbed > 0 {
                self.in_use += absorbed;
                return ptr;
            }

            // Move: a bigger block, then copy and release the old one.
            let fresh = self.alloc(new_size, align);
            if fresh.is_null() {
                return ptr::null_mut();
            }
            ptr::copy_nonoverlapping(ptr, fresh, old_size.min(new_size));
            self.dealloc(ptr);
            fresh
        }
    }

    /// Take a block out of the free list, splitting off a free tail when it is
    /// big enough to be useful.
    fn take_block(&mut self, need: usize) -> Option<*mut Header> {
        let mut cur = self.head;
        while !cur.is_null() {
            if is_free(cur) && block_size(cur) >= need {
                let total = block_size(cur);
                if total - need >= MIN_SPLIT {
                    unsafe {
                        let after = (*cur).next;
                        (*cur).size_flags = need;
                        let tail = (cur as *mut u8).add(need) as *mut Header;
                        link_free(cur, tail, total - need, after);
                    }
                } else {
                    unsafe { (*cur).size_flags = total };
                }
                return Some(cur);
            }
            cur = unsafe { (*cur).next };
        }
        None
    }

    /// Return a block to the free list, merging it with free neighbours.
    fn give_block(&mut self, cur: *mut Header) {
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

    /// Shrink `cur` to `keep`, handing the tail back when it can become a block
    /// of its own. Returns how many bytes went back to the free list.
    ///
    /// # Safety
    /// `cur` must be the header of a live allocated block, and `keep` must be
    /// no larger than it.
    unsafe fn split_tail(&mut self, cur: *mut Header, keep: usize) -> usize {
        let total = block_size(cur);
        if total < keep + MIN_SPLIT {
            return 0;
        }
        let give = total - keep;
        unsafe {
            let after = (*cur).next;
            (*cur).size_flags = keep;
            let tail = (cur as *mut u8).add(keep) as *mut Header;
            link_free(cur, tail, give, after);
        }
        give
    }
}

/// A `GlobalAlloc` in front of a [`Heap`], with no locking of its own.
///
/// Both users exclude re-entry themselves, and the exclusion differs: the
/// kernel heap wraps every call in `without_interrupts` *and* holds a spin lock,
/// because kernel allocations happen on every CPU at once; a service task is
/// never on two CPUs at once, so it needs nothing. The heap lives in an
/// `UnsafeCell` and the exclusion is the caller's promise, not this type's
/// business — a lock here would be a lock the kernel takes from an interrupt,
/// and one nobody could take twice.
pub struct GlobalHeap<S: Supply>(UnsafeCell<Heap<S>>);

// Safety: see above — the caller keeps the heap single-threaded, and `Sync` is
// what lets it live in a `static`.
unsafe impl<S: Supply> Sync for GlobalHeap<S> {}
unsafe impl<S: Supply> Send for GlobalHeap<S> {}

impl<S: Supply> GlobalHeap<S> {
    pub const fn new(supply: S) -> Self {
        GlobalHeap(UnsafeCell::new(Heap::new(supply)))
    }

    /// Borrow the heap directly. Safety: nothing else may be using it.
    pub unsafe fn with<R>(&self, f: impl FnOnce(&mut Heap<S>) -> R) -> R {
        f(unsafe { &mut *self.0.get() })
    }
}

unsafe impl<S: Supply> core::alloc::GlobalAlloc for GlobalHeap<S> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { self.with(|h| h.alloc(layout.size(), layout.align())) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        unsafe { self.with(|h| h.dealloc(ptr)) };
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { self.alloc(layout) };
        if !p.is_null() {
            unsafe { ptr::write_bytes(p, 0, layout.size()) };
        }
        p
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        unsafe { self.with(|h| h.realloc(ptr, layout.size(), layout.align(), new_size)) }
    }
}
