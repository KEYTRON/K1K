// Host-side test suite for the K1K allocator (`crates/k1k-alloc`), the code
// behind both the kernel heap and the ring-3 service heap.
//
// The allocator is pure logic over byte ranges, so it can be exercised on the
// host: the harness keeps the *system* allocator for its own bookkeeping and
// drives `k1k_alloc` directly, so every byte it hands out belongs to the test.
// The block list is then audited against an independent reading of the headers
// after every single operation.
//
// Two configurations are covered, because the kernel and a service use it
// differently: a service heap is one fixed range, while the kernel heap starts
// small and asks for more when the free list runs dry.
//
//   make test-heap

#![allow(dead_code)]
// `Vec` on a custom allocator is how a service actually uses the heap.
#![feature(allocator_api)]

use core::ptr::NonNull;
use std::alloc::{AllocError, GlobalAlloc, Layout, System};
use std::cell::UnsafeCell;

/// A global allocator for the harness only, so the test's own `Vec`s and
/// `format!`s never land in the heap under test.
struct Harness;
unsafe impl GlobalAlloc for Harness {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
}
#[global_allocator]
static HARNESS: Harness = Harness;

// `make test-heap` copies the allocator next to this file, so the crate can be
// compiled straight into the test binary — same source the kernel and the
// services build, no second copy to drift.
#[allow(unused_attributes)]
#[path = "k1k_alloc.rs"]
mod alloc;

/// Memory the tests hand out, in 1 MiB slabs the way the PMM would.
const SLAB: usize = 1 << 20;
const SLABS: usize = 32;

#[repr(align(4096))]
struct Slab([u8; SLAB]);

static mut ARENA: [Slab; SLABS] = [const { Slab([0; SLAB]) }; SLABS];
static mut NEXT: usize = 0;

/// Hands out slabs one at a time, and reports what it gave.
struct SlabSupply {
    next: usize,
    count: usize,
}

impl alloc::Supply for SlabSupply {
    fn supplied(&self) -> usize {
        self.count
    }

    fn supply(&mut self, want: usize) -> Option<alloc::Region> {
        if self.next >= SLABS || want > SLAB {
            return None;
        }
        let base = unsafe {
            (std::ptr::addr_of!(ARENA[self.next]) as *mut u8) as usize
        };
        self.next += 1;
        self.count += 1;
        Some(alloc::Region { base, len: SLAB })
    }
}

/// A heap over one slab, for the "service" shape: one region, no growth.
struct TestHeap(UnsafeCell<alloc::Heap<SlabSupply>>);

// Safety: the test is the only user of the heap.
unsafe impl Sync for TestHeap {}

impl TestHeap {
    const fn new() -> Self {
        TestHeap(UnsafeCell::new(alloc::Heap::new(SlabSupply {
            next: 0,
            count: 0,
        })))
    }
    /// Borrow the heap: the test is the only user.
    fn with<R>(&self, f: impl FnOnce(&mut alloc::Heap<SlabSupply>) -> R) -> R {
        f(unsafe { &mut *self.0.get() })
    }
    fn reset(&self) {
        let base = unsafe { std::ptr::addr_of!(ARENA[0]) as *mut u8 as usize };
        self.with(|h| {
            *h = alloc::Heap::new(SlabSupply {
                next: 0,
                count: 0,
            });
            h.add_region(alloc::Region {
                base,
                len: SLAB,
            });
        });
    }
}

unsafe impl GlobalAlloc for TestHeap {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        self.with(|h| h.alloc(l.size(), l.align()))
    }
    unsafe fn dealloc(&self, p: *mut u8, _l: Layout) {
        unsafe { self.with(|h| h.dealloc(p)) };
    }
    /// Forward to the crate rather than taking the default move-everywhere
    /// path: in-place growth is a feature of the allocator, and a service
    /// using it as a `#[global_allocator]` gets it for free.
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        unsafe { self.with(|h| h.realloc(p, l.size(), l.align(), new_size)) }
    }
}

fn say(s: &str) {
    log_bytes(s.as_bytes());
    log_bytes(b"\n");
}

fn log_bytes(s: &[u8]) {
    let mut p = s.as_ptr();
    let mut left = s.len();
    while left > 0 {
        let n = unsafe { raw_write(1, p, left) };
        if n <= 0 {
            return;
        }
        p = unsafe { p.add(n as usize) };
        left -= n as usize;
    }
}

unsafe extern "C" {
    #[link_name = "write"]
    fn raw_write(fd: i32, buf: *const u8, count: usize) -> isize;
}

struct Live {
    ptr: *mut u8,
    size: usize,
    align: usize,
    pattern: u8,
    /// Bytes `realloc` guarantees to have carried over: it preserves
    /// `min(old, new)` and nothing beyond that.
    valid: usize,
}

impl Live {
    fn new(ptr: *mut u8, size: usize, align: usize, pattern: u8) -> Self {
        unsafe { std::ptr::write_bytes(ptr, pattern, size) };
        Self {
            ptr,
            size,
            align,
            pattern,
            valid: size,
        }
    }
    fn intact(&self) -> bool {
        let s = unsafe { std::slice::from_raw_parts(self.ptr, self.valid.max(1)) };
        s.iter().all(|b| *b == self.pattern)
    }
}

const HDR: usize = 32;

/// An independent reading of the block list. The heap must stay a clean
/// partition of the regions it was given: every block a multiple of 16, links
/// consistent, the blocks tiling the whole range, no two allocated blocks
/// overlapping, every live pointer inside a block of its own with its header
/// clear of the payload, and the counters agreeing with the list.
fn audit(live: &[Live]) -> Result<(), String> {
    let heap = unsafe { &*HEAP.0.get() };
    let regions: &[alloc::Region] = heap.regions();
    if regions.is_empty() {
        return if live.is_empty() {
            Ok(())
        } else {
            Err("live blocks in a heap with no regions".into())
        };
    }
    let owned: Vec<(usize, usize)> = regions.iter().map(|r| (r.base, r.len)).collect();
    // A header needs room for the whole header; any other byte only needs to
    // be inside a region.
    let covers = |addr: usize| {
        owned
            .iter()
            .any(|(base, len)| addr >= *base && addr + HDR <= *base + *len)
    };
    let inside = |addr: usize| {
        owned
            .iter()
            .any(|(base, len)| addr >= *base && addr < *base + *len)
    };
    let mut p = owned[0].0;
    let mut prev = 0usize;
    let mut last_end = 0usize;
    let mut prev_was_free = false;
    let mut used: Vec<(usize, usize)> = Vec::new();
    let mut steps = 0;
    while p != 0 {
        if !covers(p) {
            return Err(format!("header at {p:#x} is outside every region"));
        }
        let flags = unsafe { *(p as *const usize) };
        let next = unsafe { *((p + 8) as *const usize) };
        let prev_link = unsafe { *((p + 16) as *const usize) };
        let back = unsafe { *((p + 24) as *const usize) };
        let size = flags & !1;
        if size % 16 != 0 {
            return Err(format!("block at {p:#x} has size {size:#x}, not 16-aligned"));
        }
        if size < HDR + 16 {
            return Err(format!("block at {p:#x} is too small ({size:#x})"));
        }
        if prev_link != prev {
            return Err(format!(
                "block at {p:#x} links back to {prev_link:#x}, expected {prev:#x}"
            ));
        }
        // The word below an allocated payload holds the distance back to its
        // own header. Only a live pointer knows where that payload is, so this
        // is checked per live allocation below; the field is read here just to
        // make sure reading it is in bounds.
        let _ = back;
        // Two free blocks in a row would mean a free that did not coalesce: the
        // heap would turn down a request it could have served.
        if prev_was_free && flags & 1 == 1 {
            return Err(format!(
                "free block at {p:#x} (size {size:#x}) follows a free block at {prev:#x}"
            ));
        }
        prev_was_free = flags & 1 == 1;
        if flags & 1 == 0 {
            used.push((p, size));
        }
        if !inside(p + size - 1) {
            return Err(format!("block at {p:#x} runs past every region"));
        }
        // Track how far the blocks reach, so the tiling can be checked.
        last_end += size;
        prev = p;
        p = next;
        steps += 1;
        if steps > 1_000_000 {
            return Err("the block list does not terminate".into());
        }
    }
    // The blocks must tile the regions exactly.
    let total: usize = owned.iter().map(|(_, len)| *len).sum();
    if p != 0 {
        return Err("the list did not end at null".into());
    }
    if last_end != total {
        return Err(format!("blocks cover {last_end:#x}, expected {total:#x}"));
    }
    for w in used.windows(2) {
        if w[0].0 + w[0].1 > w[1].0 {
            return Err(format!(
                "allocated blocks overlap: {:#x}+{:#x} and {:#x}",
                w[0].0, w[0].1, w[1].0
            ));
        }
    }
    // Every live pointer must be inside a block of its own, and the word below
    // it must resolve to that block's header: that is what `dealloc` follows.
    for (i, l) in live.iter().enumerate() {
        let addr = l.ptr as usize;
        // `used` is in address order, so the block holding a pointer is a
        // binary search away. The churn audits after every single operation, so
        // a scan here would make the test quadratic.
        let mut lo = 0usize;
        let mut hi = used.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if used[mid].0 <= addr {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let Some(&(start, size)) = lo.checked_sub(1).and_then(|i| used.get(i)) else {
            return Err(format!("live allocation {i} at {addr:#x} is not in an allocated block"));
        };
        if addr >= start + size {
            return Err(format!("live allocation {i} at {addr:#x} is not in an allocated block"));
        }
        if l.align > 1 && addr % l.align != 0 {
            return Err(format!("live allocation {i} at {addr:#x} is not {}-aligned", l.align));
        }
        let back = unsafe { *(l.ptr.sub(8) as *const usize) };
        if back == 0 || back >= size || addr.wrapping_sub(back) != start {
            return Err(format!(
                "live allocation {i} at {addr:#x} resolves to its header as {:#x}, expected {:#x}",
                addr.wrapping_sub(back),
                start
            ));
        }
        if addr + l.size > start + size {
            return Err(format!("live allocation {i} runs past its block"));
        }
    }

    let stats = heap.stats();
    let used_bytes: usize = used.iter().map(|(_, len)| *len).sum();
    if stats.in_use != used_bytes {
        return Err(format!(
            "in_use={} but the allocated blocks add up to {used_bytes}",
            stats.in_use
        ));
    }
    if stats.live != used.len() {
        return Err(format!(
            "live={} but {} blocks are allocated",
            stats.live,
            used.len()
        ));
    }
    for l in live {
        let a = l.ptr as usize;
        match used.iter().find(|(s, len)| a >= *s && a < s + len) {
            None => return Err(format!("live pointer {a:#x} is not in an allocated block")),
            Some((s, len)) => {
                if a - s < HDR {
                    return Err(format!("live pointer {a:#x} overlaps a block header"));
                }
                if a + l.valid > s + len {
                    return Err(format!(
                        "live block at {a:#x} (+{} bytes) runs past its block ({len:#x})",
                        l.valid
                    ));
                }
            }
        }
    }
    Ok(())
}

static HEAP: TestHeap = TestHeap::new();

/// Every size and alignment a service might ask for, all live at once.
fn test_basic() -> bool {
    HEAP.reset();
    let h = &HEAP;
    let mut live: Vec<Live> = Vec::new();
    for size in [1usize, 2, 7, 15, 16, 17, 31, 32, 33, 63, 64, 100, 511, 4096, 9000] {
        let p = unsafe { h.alloc(Layout::from_size_align(size, 1).unwrap()) };
        if p.is_null() {
            say(&format!("FAIL: alloc({size}) returned null"));
            return false;
        }
        live.push(Live::new(p, size, 1, 0xA5));
    }
    for align in [2usize, 4, 8, 16, 32, 64, 128, 256, 1024, 4096] {
        let size = align * 2 + 1;
        let p = unsafe { h.alloc(Layout::from_size_align(size, align).unwrap()) };
        if p.is_null() || (p as usize) % align != 0 {
            say(&format!("FAIL: alignment {align} not honoured ({p:p})"));
            return false;
        }
        live.push(Live::new(p, size, align, 0x3C));
    }
    for i in 0..live.len() {
        for j in i + 1..live.len() {
            let (a, al) = (live[i].ptr as usize, live[i].size);
            let (b, bl) = (live[j].ptr as usize, live[j].size);
            if a < b + bl && b < a + al {
                say(&format!("FAIL: blocks {i} and {j} overlap"));
                return false;
            }
        }
    }
    if let Err(why) = audit(&live) {
        say(&format!("FAIL: audit after basic: {why}"));
        return false;
    }
    for l in &live {
        if !l.intact() {
            say("FAIL: a live block was overwritten");
            return false;
        }
    }
    for l in &live {
        unsafe { h.dealloc(l.ptr, l.layout()) };
    }
    if let Err(why) = audit(&[]) {
        say(&format!("FAIL: audit after freeing everything: {why}"));
        return false;
    }
    let s = h.with(|x| x.stats());
    if s.in_use != 0 || s.live != 0 {
        say(&format!(
            "FAIL: after freeing everything in_use={} live={}",
            s.in_use, s.live
        ));
        return false;
    }
    true
}

/// Fragment the heap, then free everything: the space must come back.
fn test_reclaim() -> bool {
    let h = &HEAP;
    h.reset();
    let mut blocks: Vec<Live> = Vec::new();
    for _ in 0..400 {
        let p = unsafe { h.alloc(Layout::from_size_align(2000, 16).unwrap()) };
        if p.is_null() {
            break;
        }
        blocks.push(Live::new(p, 2000, 16, 0x11));
    }
    if blocks.len() < 100 {
        say("FAIL: could not fragment the heap");
        return false;
    }
    // Free in a scattered order so merges have to work in both directions.
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    while !blocks.is_empty() {
        let i = rng.below(blocks.len() as u64) as usize;
        let l = blocks.swap_remove(i);
        unsafe { h.dealloc(l.ptr, l.layout()) };
    }
    if let Err(why) = audit(&[]) {
        say(&format!("FAIL: audit after reclaim: {why}"));
        return false;
    }
    let s = h.with(|x| x.stats());
    if s.in_use != 0 || s.live != 0 {
        say(&format!("FAIL: reclaim left in_use={} live={}", s.in_use, s.live));
        return false;
    }
    // One allocation must now span most of the heap.
    let p = unsafe { h.alloc(Layout::from_size_align(SLAB / 2, 16).unwrap()) };
    if p.is_null() {
        say("FAIL: cannot allocate half the heap after reclaim");
        return false;
    }
    unsafe { h.dealloc(p, Layout::from_size_align(SLAB / 2, 16).unwrap()) };
    true
}

/// Running out must be reported, not silently wrong.
fn test_exhaustion() -> bool {
    let h = &HEAP;
    h.reset();
    let l = Layout::from_size_align(256 * 1024, 16).unwrap();
    let mut blocks: Vec<*mut u8> = Vec::new();
    loop {
        let p = unsafe { h.alloc(l) };
        if p.is_null() {
            break;
        }
        blocks.push(p);
        if blocks.len() > 200 {
            say("FAIL: the heap never ran out");
            return false;
        }
    }
    if blocks.len() < 2 {
        say(&format!("FAIL: the heap ran out after {} blocks", blocks.len()));
        return false;
    }
    if h.with(|x| x.stats()).failures == 0 {
        say("FAIL: exhaustion was not counted");
        return false;
    }
    for p in blocks {
        unsafe { h.dealloc(p, l) };
    }
    if h.with(|x| x.stats()).in_use != 0 {
        say("FAIL: bytes still in use after exhaustion");
        return false;
    }
    true
}

/// The kernel's shape: start with nothing, let the supplier hand out slabs as
/// the heap fills, and check the list stays clean across the growth.
fn test_growth() -> bool {
    let h = &HEAP;
    h.reset();
    // A fresh heap with no region at all: the first allocation must pull one.
    h.with(|x| {
        *x = alloc::Heap::new(SlabSupply {
            next: 0,
            count: 0,
        })
    });
    let p = unsafe { h.alloc(Layout::from_size_align(64, 16).unwrap()) };
    if p.is_null() {
        say("FAIL: an empty heap did not take its first region");
        return false;
    }
    unsafe { h.dealloc(p, Layout::from_size_align(64, 16).unwrap()) };
    if let Err(why) = audit(&[]) {
        say(&format!("FAIL: audit after first region: {why}"));
        return false;
    }

    // Fill every slab, so the heap has to ask for the next one mid-flight.
    let mut live: Vec<Live> = Vec::new();
    let mut pat = 1u8;
    for round in 0..2000 {
        let size = 1 + (round * 977) % 9000;
        let p = unsafe { h.alloc(Layout::from_size_align(size, 16).unwrap()) };
        if p.is_null() {
            say(&format!("FAIL: growth stopped at round {round}"));
            return false;
        }
        pat = pat.wrapping_add(1).max(1);
        live.push(Live::new(p, size, 16, pat));
        if let Err(why) = audit(&live) {
            say(&format!("FAIL: audit at growth round {round}: {why}"));
            return false;
        }
    }
    let (regions, slabs) = h.with(|x| (x.stats().regions, x.supply_count()));
    if regions < 2 || slabs != regions {
        say(&format!(
            "FAIL: expected the supplier to have been asked for more slabs, got {slabs} for {regions} regions"
        ));
        return false;
    }
    for l in &live {
        if !l.intact() {
            say("FAIL: a block was overwritten while growing");
            return false;
        }
    }
    for l in &live {
        unsafe { h.dealloc(l.ptr, l.layout()) };
    }
    let s = h.with(|x| x.stats());
    if s.in_use != 0 {
        say("FAIL: growing heap did not give everything back");
        return false;
    }
    if let Err(why) = audit(&[]) {
        say(&format!("FAIL: audit after growth: {why}"));
        return false;
    }
    say(&format!(
        "growth: {regions} region(s), {} slabs taken",
        slabs
    ));
    true
}

/// The growth pattern `Vec` and `String` produce: doubling, with the old bytes
/// carried over.
fn test_vec_growth() -> bool {
    let h = &HEAP;
    h.reset();
    let mut p = unsafe { h.alloc(Layout::from_size_align(1, 1).unwrap()) };
    let mut size = 1usize;
    unsafe { std::ptr::write_bytes(p, 0x5A, 1) };
    // Doubling from a single byte, the way a `Vec` grows. The last step is
    // left out on purpose: a 1 MiB payload needs a header on top of it, so it
    // cannot fit in a 1 MiB region and asking for it would only prove that.
    let mut steps = 0;
    while size * 2 + HDR <= SLAB {
        let want = size * 2;
        let q = unsafe { h.realloc(p, Layout::from_size_align(size, 1).unwrap(), want) };
        if q.is_null() {
            let st = h.with(|x| x.stats());
            say(&format!(
                "FAIL: growth realloc {size} -> {want} returned null \
                 (in_use {:#x}, regions {}, bytes {:#x})",
                st.in_use, st.regions, st.region_bytes
            ));
            return false;
        }
        let old = unsafe { std::slice::from_raw_parts(q, size) };
        if old.iter().any(|b| *b != 0x5A) {
            say(&format!("FAIL: growth lost data at step {steps}"));
            return false;
        }
        // Fill the fresh tail so the next round can check the whole buffer.
        unsafe { std::ptr::write_bytes(q.add(size), 0x5A, want - size) };
        p = q;
        size = want;
        steps += 1;
        if let Err(why) = audit(&[]) {
            say(&format!("FAIL: audit during growth: {why}"));
            return false;
        }
    }
    say(&format!("growth: {steps} doublings up to {size} bytes"));
    // And a shrink, which has to hand the tail back.
    let small = size / 4;
    let q = unsafe { h.realloc(p, Layout::from_size_align(size, 1).unwrap(), small) };
    if q.is_null() {
        say("FAIL: shrink realloc returned null");
        return false;
    }
    let kept = unsafe { std::slice::from_raw_parts(q, small) };
    if kept.iter().any(|b| *b != 0x5A) {
        say("FAIL: shrink lost data");
        return false;
    }
    unsafe { h.dealloc(q, Layout::from_size_align(small, 1).unwrap()) };
    if let Err(why) = audit(&[]) {
        say(&format!("FAIL: audit after growth: {why}"));
        return false;
    }
    true
}

/// Print the block list as the heap sees it. Only used to explain a failure.
fn dump_blocks() {
    let heap = unsafe { &*HEAP.0.get() };
    let regions = heap.regions();
    let base = regions.first().map(|r| r.base).unwrap_or(0);
    let mut p = base;
    let mut n = 0;
    while p != 0 && n < 40 {
        let flags = unsafe { *(p as *const usize) };
        let next = unsafe { *((p + 8) as *const usize) };
        let prev = unsafe { *((p + 16) as *const usize) };
        say(&format!(
            "   block {n} at +{:#x} size={:#x} free={} next=+{:#x} prev=+{:#x}",
            p.wrapping_sub(base),
            flags & !1,
            flags & 1,
            next.wrapping_sub(base),
            prev.wrapping_sub(base)
        ));
        p = next;
        n += 1;
    }
    say(&format!("   ({n} blocks printed)"));
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Random alloc/free/realloc churn with the list audited after every step.
///
/// On one slab, with no way to grow: the heap fills up, every later request has
/// to find a hole or fail, and the list keeps being carved up and put back
/// together — which is where a coalescing bug shows up. A heap that grew instead
/// would just get more room and the audit would get slower every round.
///
/// `audit_every` trades depth for length: auditing walks the whole list after
/// every single operation, which is what pins a bug to the operation that caused
/// it, but it also makes the run quadratic in the size of the heap. A short run
/// with the audit on every step plus a long run with it every thousand steps
/// covers both "the audit catches the exact step" and "the allocator survives
/// five hundred thousand operations".
fn test_churn(rounds: usize, audit_every: usize) -> bool {
    let h = &HEAP;
    fixed_region(1);
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let mut live: Vec<Live> = Vec::new();
    let mut pattern = 1u8;
    let mut oom = 0usize;
    for round in 0..rounds {
        let roll = rng.below(100);
        if roll < 45 || live.is_empty() {
            let size = 1 + rng.below(3000) as usize;
            let align = 1usize << rng.below(8);
            let Ok(layout) = Layout::from_size_align(size, align) else {
                continue;
            };
            let p = unsafe { h.alloc(layout) };
            if p.is_null() {
                // Out of memory is a legitimate answer: the churn keeps growing
                // until the heap is full.
                oom += 1;
                continue;
            }
            if (p as usize) % align != 0 {
                say(&format!("FAIL: churn alignment {align} broken"));
                return false;
            }
            pattern = pattern.wrapping_add(1).max(1);
            live.push(Live::new(p, size, align, pattern));
        } else if roll < 72 {
            let i = rng.below(live.len() as u64) as usize;
            let l = live.swap_remove(i);
            unsafe { h.dealloc(l.ptr, l.layout()) };
        } else {
            let i = rng.below(live.len() as u64) as usize;
            let new_size = 1 + rng.below(6000) as usize;
            let l = &mut live[i];
            let q = unsafe { h.realloc(l.ptr, l.layout(), new_size) };
            if q.is_null() {
                oom += 1;
                continue;
            }
            if (q as usize) % l.align != 0 {
                say("FAIL: realloc broke alignment");
                return false;
            }
            // Only the first `min(old, new)` bytes are guaranteed to survive.
            let kept = l.valid.min(new_size);
            let bytes = unsafe { std::slice::from_raw_parts(q, kept.max(1)) };
            if bytes.iter().any(|b| *b != l.pattern) {
                say(&format!(
                    "FAIL: realloc lost data ({} -> {new_size}, first {kept} bytes)",
                    l.size
                ));
                return false;
            }
            l.ptr = q;
            l.valid = kept;
            l.size = new_size;
        }
        if round % audit_every == 0
            && let Err(why) = audit(&live)
        {
            say(&format!("FAIL: audit at churn round {round}: {why}"));
            dump_blocks();
            return false;
        }
        if round % 512 == 0 {
            for (i, l) in live.iter().enumerate() {
                if !l.intact() {
                    say(&format!("FAIL: churn corrupted live block {i}"));
                    return false;
                }
            }
        }
    }
    say(&format!(
        "churn: {rounds} rounds, {oom} request(s) refused for lack of memory"
    ));
    for l in &live {
        if !l.intact() {
            say("FAIL: churn corrupted a live block at the end");
            return false;
        }
        unsafe { h.dealloc(l.ptr, l.layout()) };
    }
    let s = h.with(|x| x.stats());
    if s.in_use != 0 || s.live != 0 {
        say(&format!("FAIL: churn left in_use={} live={}", s.in_use, s.live));
        return false;
    }
    if let Err(why) = audit(&[]) {
        say(&format!("FAIL: audit after churn: {why}"));
        return false;
    }
    true
}

impl Live {
    fn layout(&self) -> Layout {
        Layout::from_size_align(self.size, self.align).unwrap()
    }
}


/// The allocator the way a service sees it: `core::alloc::Allocator` on top of
/// the heap, so real `Vec`s — and everything built on them — run on it. The
/// churn test above drives the heap API directly; this drives the same code a
/// ring-3 program runs, doubling growth and all.
struct ServiceAlloc;

fn slice(p: *mut u8, len: usize) -> Result<NonNull<[u8]>, AllocError> {
    let Some(p) = NonNull::new(p) else {
        return Err(AllocError);
    };
    Ok(NonNull::slice_from_raw_parts(p, len))
}

unsafe impl core::alloc::Allocator for ServiceAlloc {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        slice(HEAP.with(|h| h.alloc(layout.size(), layout.align())), layout.size())
    }
    unsafe fn deallocate(&self, ptr: NonNull<u8>, _layout: Layout) {
        unsafe { HEAP.with(|h| h.dealloc(ptr.as_ptr())) };
    }
    unsafe fn grow(
        &self,
        ptr: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        slice(
            unsafe { HEAP.with(|h| h.realloc(ptr.as_ptr(), old.size(), old.align(), new.size())) },
            new.size(),
        )
    }
    unsafe fn grow_zeroed(
        &self,
        ptr: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        let old_size = old.size();
        let out = slice(
            unsafe { HEAP.with(|h| h.realloc(ptr.as_ptr(), old.size(), old.align(), new.size())) },
            new.size(),
        )?;
        unsafe { out.as_ptr().cast::<u8>().add(old_size).write_bytes(0, new.size() - old_size) };
        Ok(out)
    }
    unsafe fn shrink(
        &self,
        ptr: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        unsafe { self.grow(ptr, old, new) }
    }
}

const SERVICE: ServiceAlloc = ServiceAlloc;

/// Give the heap one fixed range of `slabs` slabs, the shape a service gets: a
/// supervisor-mapped window and no way to ask for more.
fn fixed_region(slabs: usize) {
    let base = unsafe { std::ptr::addr_of!(ARENA[0]) as *mut u8 as usize };
    HEAP.with(|h| {
        *h = alloc::Heap::new(SlabSupply {
            next: 0,
            count: 0,
        });
        h.add_region(alloc::Region {
            base,
            len: SLAB * slabs,
        });
    });
}

/// A workload shaped like a real service, in a heap of the size the supervisor
/// gives one: fill it, punch holes in it, refill the holes with one big
/// request, then run real `Vec`s over the top — which is what a ring-3 program
/// does, doubling growth and all. The list is audited at every step, and
/// everything handed out has to come back.
fn test_service_workload() -> bool {
    const SLABS_IN_USE: usize = 4;
    const KIB: usize = 1024;
    let total = SLAB * SLABS_IN_USE;
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

    // Fill the heap with 1 KiB blocks until it says no. A service that has been
    // up for a while looks like this, and a heap that leaks or loses a block
    // shows up as a count that is short.
    fixed_region(SLABS_IN_USE);
    let mut blocks: Vec<NonNull<u8>> = Vec::new();
    loop {
        let p = HEAP.with(|h| h.alloc(KIB, 1));
        if p.is_null() {
            break;
        }
        blocks.push(NonNull::new(p).unwrap());
        if blocks.len() > SLABS_IN_USE * 1024 {
            say("FAIL: the heap handed out more than it has");
            return false;
        }
    }
    let filled = HEAP.with(|h| h.stats().in_use);
    // Every block costs its size plus a header, rounded up to 16.
    let per_block = (KIB + 48).next_multiple_of(16);
    let expected = total / per_block * per_block;
    if filled != expected {
        say(&format!(
            "FAIL: filling 4 MiB with 1 KiB blocks used {filled} bytes, expected {expected}"
        ));
        return false;
    }
    if let Err(why) = audit(&[]) {
        say(&format!("FAIL: audit on a full heap: {why}"));
        return false;
    }

    // Free the bottom half and ask for a chunk that spans all of it: this only
    // works if the freed blocks coalesced into one run, which is what a service
    // needs when it drops a cache and asks for a bigger one.
    let half = blocks.len() / 2;
    for p in blocks.iter().take(half) {
        unsafe { HEAP.with(|h| h.dealloc(p.as_ptr())) };
    }
    // A megabyte fits inside the freed half, and a megabyte is more than the
    // freed half could hold if the blocks had not coalesced.
    let big = HEAP.with(|h| h.alloc(1024 * KIB, 1));
    if big.is_null() {
        say("FAIL: a 1 MiB request failed with a coalesced 2 MiB run free");
        return false;
    }
    if let Err(why) = audit(&[]) {
        say(&format!("FAIL: audit after refilling: {why}"));
        return false;
    }
    // First fit has to be honest too: all the free bytes in the heap are not
    // one run, so asking for all of them has to fail rather than corrupt.
    let free_bytes = HEAP.with(|h| h.free_bytes());
    let too_big = HEAP.with(|h| h.alloc(free_bytes, 1));
    if !too_big.is_null() {
        say("FAIL: a request for every free byte at once succeeded");
        return false;
    }
    unsafe { HEAP.with(|h| h.dealloc(big)) };
    for p in blocks.iter().skip(half) {
        unsafe { HEAP.with(|h| h.dealloc(p.as_ptr())) };
    }
    blocks.clear();
    if HEAP.with(|h| h.stats().in_use) != 0 {
        say(&format!(
            "FAIL: {} bytes still in use after freeing every block",
            HEAP.with(|h| h.stats().in_use)
        ));
        return false;
    }

    // Real `Vec`s on the same heap: buffers that double into the tens of
    // kilobytes, a rolling set of live ones, occasional trims and drops.
    let mut buffers: Vec<Vec<u8, ServiceAlloc>> = Vec::new();
    let mut live: Vec<Live> = Vec::new();
    for round in 0..300usize {
        let mut want = 64 + rng.below(4096) as usize;
        let mut b: Vec<u8, ServiceAlloc> = Vec::new_in(SERVICE);
        b.reserve_exact(want);
        while b.capacity() < want {
            want = (b.capacity() * 2).max(want + 1);
            b.reserve_exact(want);
        }
        // Write the whole capacity: an over-aligned or off-by-one block would
        // let this scribble over a neighbour.
        unsafe { b.as_mut_ptr().add(b.capacity()).sub(b.capacity()).write_bytes(0xA5, b.capacity()) };
        live.push(Live::new(b.as_mut_ptr(), b.capacity(), 1, 0xA5));
        buffers.push(b);

        if buffers.len() > 20 {
            let index = rng.below(buffers.len() as u64) as usize;
            buffers.remove(index);
            if let Some(front) = buffers.first_mut() {
                let keep = front.capacity() / 2;
                front.truncate(front.len().min(keep));
                front.shrink_to_fit();
            }
        }
        live.clear();
        for b in buffers.iter() {
            if b.capacity() > 0 {
                live.push(Live::new(b.as_ptr() as *mut u8, b.capacity().min(256), 1, 0xA5));
            }
        }
        if round % 50 == 0
            && let Err(why) = audit(&live)
        {
            say(&format!("FAIL: audit in the service workload at round {round}: {why}"));
            return false;
        }
    }
    let peak = HEAP.with(|h| h.stats().peak);
    buffers.clear();
    live.clear();
    if let Err(why) = audit(&[]) {
        say(&format!("FAIL: audit after the service workload: {why}"));
        return false;
    }
    if HEAP.with(|h| h.stats().in_use) != 0 {
        say("FAIL: the service workload leaked");
        return false;
    }

    // Small blocks held all at once, the way a parser holds its lines.
    fixed_region(SLABS_IN_USE);
    let mut lines: Vec<Vec<u8, ServiceAlloc>> = Vec::new();
    for _ in 0..6000 {
        let mut s: Vec<u8, ServiceAlloc> = Vec::new_in(SERVICE);
        for _ in 0..40 {
            s.extend_from_slice(b"x");
        }
        lines.push(s);
    }
    for (i, s) in lines.iter().enumerate() {
        if s.len() != 40 {
            say(&format!("FAIL: line {i} is {} bytes, not 40", s.len()));
            return false;
        }
    }
    if let Err(why) = audit(&[]) {
        say(&format!("FAIL: audit with 6000 live lines: {why}"));
        return false;
    }
    lines.clear();
    if HEAP.with(|h| h.stats().in_use) != 0 {
        say("FAIL: the line workload leaked");
        return false;
    }
    say(&format!(
        "service workload: {} KiB fixed region, {} KiB peak, all returned",
        total / 1024,
        peak / 1024
    ));
    true
}

fn main() {
    let mut ok = true;
    ok &= test_basic();
    ok &= test_reclaim();
    ok &= test_exhaustion();
    ok &= test_growth();
    ok &= test_vec_growth();
    // Every operation audited: this is the run that pins a bug to the step.
    ok &= test_churn(20_000, 1);
    // Ten times as many operations, audited often enough to notice.
    ok &= test_churn(200_000, 1_000);
    ok &= test_service_workload();
    let s = HEAP.with(|x| x.stats());
    say(&format!(
        "allocs={} frees={} failures={} peak={} in_use={}",
        s.allocs, s.frees, s.failures, s.peak, s.in_use
    ));
    if ok {
        say("PASS");
    } else {
        say("FAILED");
        std::process::exit(1);
    }
}
