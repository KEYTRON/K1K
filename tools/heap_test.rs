// Host-side test suite for the K1K service allocator (`user/rt/src/heap.rs`).
//
// The allocator is pure logic over a byte range, so it can be exercised on the
// host: this harness keeps the *system* allocator for its own bookkeeping and
// drives `K1kHeap` through direct `GlobalAlloc` calls, so every byte the
// allocator hands out belongs to the test. The list is then audited against an
// independent reading of the block headers after every single operation.
//
// `make test-heap` copies this file next to a copy of `user/rt/src/heap.rs` that
// has its `#[global_allocator]` removed (two global allocators in one binary is
// an error) and builds it with plain rustc — no cargo, no test harness, so the
// allocator under test owns every byte it hands out.

#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};

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

pub const HEAP_SIZE: usize = 8 << 20;
/// `#[repr(C, align(16))]` over three `usize` fields.
const HDR: usize = 32;

pub struct BootInfo {
    magic: u64,
    layout: u64,
    task_id: u32,
    _pad: u32,
    heap_base: u64,
    heap_size: u64,
    stack_top: u64,
    arg_len: u64,
}

#[repr(align(4096))]
struct HeapMem([u8; HEAP_SIZE]);

static mut HEAP_MEM: HeapMem = HeapMem([0; HEAP_SIZE]);
static mut INFO: BootInfo = BootInfo {
    magic: 0x3148_4F4F_425A_314B,
    layout: 1,
    task_id: 1,
    _pad: 0,
    heap_base: 0,
    heap_size: 0,
    stack_top: 0,
    arg_len: 0,
};

/// Fills the stub in on first use: the address of a static is only known at run
/// time, and this way the allocator works even if something allocates before
/// `main`.
pub fn boot_info() -> Option<&'static BootInfo> {
    let info = unsafe { &mut *std::ptr::addr_of_mut!(INFO) };
    info.heap_base = std::ptr::addr_of!(HEAP_MEM) as u64;
    info.heap_size = HEAP_SIZE as u64;
    Some(&*info)
}

pub fn log_bytes(s: &[u8]) {
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

#[path = "heap_under_test.rs"]
mod heap;

fn say(s: &str) {
    log_bytes(s.as_bytes());
    log_bytes(b"\n");
}

fn base() -> usize {
    std::ptr::addr_of!(HEAP_MEM) as usize
}

fn word(p: usize, i: usize) -> usize {
    unsafe { *(p as *const usize).add(i) }
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
    fn layout(&self) -> Layout {
        Layout::from_size_align(self.size, self.align).unwrap()
    }
}

/// An independent reading of the block list. The heap must stay a clean
/// partition: every block 16-aligned in size, links consistent, the blocks
/// tiling the whole range, no two allocated blocks overlapping, and every live
/// pointer inside a block of its own with its header clear of the payload.
fn audit(live: &[Live]) -> Result<(), String> {
    let b = base();
    let end = b + HEAP_SIZE;
    if word(b, 0) == 0 && word(b, 1) == 0 && word(b, 2) == 0 {
        // The allocator has not run yet: the heap is one untouched range.
        return if live.is_empty() {
            Ok(())
        } else {
            Err("live blocks in a heap the allocator never touched".into())
        };
    }
    let mut p = b;
    let mut prev = 0usize;
    let mut covered = 0usize;
    let mut used: Vec<(usize, usize)> = Vec::new();
    let mut steps = 0;
    while p != 0 {
        if p < b || p + HDR > end {
            return Err(format!("header at +{:#x} is outside the heap", p.wrapping_sub(b)));
        }
        let flags = word(p, 0);
        let size = flags & !1;
        if size % 16 != 0 {
            return Err(format!(
                "block at +{:#x} has size {size:#x}, not a multiple of 16",
                p - b
            ));
        }
        if size < HDR + 16 {
            return Err(format!("block at +{:#x} is too small ({size:#x})", p - b));
        }
        if word(p, 2) != prev {
            return Err(format!(
                "block at +{:#x} points back to +{:#x}, expected +{:#x}",
                p - b,
                word(p, 2).wrapping_sub(b),
                prev.wrapping_sub(b)
            ));
        }
        if flags & 1 == 0 {
            used.push((p, size));
        }
        prev = p;
        p = word(p, 1);
        covered += size;
        steps += 1;
        if steps > 1_000_000 {
            return Err("the block list does not terminate".into());
        }
    }
    if covered != HEAP_SIZE {
        return Err(format!("blocks cover {covered:#x}, expected {HEAP_SIZE:#x}"));
    }
    // The bookkeeping must agree with the list: `in_use` is the sum of the
    // allocated blocks, and `live` counts them.
    let used_bytes: usize = used.iter().map(|(_, len)| *len).sum();
    let stats = heap::heap_stats();
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
    for w in used.windows(2) {
        if w[0].0 + w[0].1 > w[1].0 {
            return Err(format!(
                "allocated blocks overlap: +{:#x}+{:#x} and +{:#x}",
                w[0].0 - b,
                w[0].1,
                w[1].0 - b
            ));
        }
    }
    for l in live {
        let a = l.ptr as usize;
        match used.iter().find(|(s, len)| a >= *s && a < s + len) {
            None => return Err(format!("live pointer +{:#x} is not in an allocated block", a - b)),
            Some((start, len)) => {
                if a - start < HDR {
                    return Err(format!("live pointer +{:#x} overlaps a block header", a - b));
                }
                if a + l.valid > start + len {
                    return Err(format!(
                        "live block at +{:#x} (+{} bytes) runs past its block (+{:#x}, {:#x})",
                        a - b,
                        l.valid,
                        start - b,
                        len
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Every size and alignment a service might ask for, all live at once.
fn test_basic(h: &heap::K1kHeap) -> bool {
    let mut live: Vec<Live> = Vec::new();
    for size in [1usize, 2, 7, 15, 16, 17, 31, 32, 33, 63, 64, 100, 511, 4096, 9000] {
        let l = Layout::from_size_align(size, 1).unwrap();
        let p = unsafe { h.alloc(l) };
        if p.is_null() {
            say(&format!("FAIL: alloc({size}) returned null"));
            return false;
        }
        live.push(Live::new(p, size, 1, 0xA5));
    }
    for align in [2usize, 4, 8, 16, 32, 64, 128, 256, 1024, 4096] {
        let size = align * 2 + 1;
        let l = Layout::from_size_align(size, align).unwrap();
        let p = unsafe { h.alloc(l) };
        if p.is_null() || (p as usize) % align != 0 {
            say(&format!(
                "FAIL: alignment {align} not honoured (got {p:p}, {} bytes)",
                p as usize
            ));
            return false;
        }
        live.push(Live::new(p, size, align, 0x3C));
    }
    for i in 0..live.len() {
        for j in i + 1..live.len() {
            let (a, al) = (live[i].ptr as usize, live[i].size);
            let (c, cl) = (live[j].ptr as usize, live[j].size);
            if a < c + cl && c < a + al {
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
    let s = heap::heap_stats();
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
fn test_reclaim(h: &heap::K1kHeap) -> bool {
    let mut blocks: Vec<Live> = Vec::new();
    for _ in 0..400 {
        let l = Layout::from_size_align(2000, 16).unwrap();
        let p = unsafe { h.alloc(l) };
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
    let s = heap::heap_stats();
    if s.in_use != 0 || s.live != 0 {
        say(&format!(
            "FAIL: reclaim left in_use={} live={}",
            s.in_use, s.live
        ));
        return false;
    }
    // One allocation must now span half the heap.
    let l = Layout::from_size_align(HEAP_SIZE / 2, 16).unwrap();
    let p = unsafe { h.alloc(l) };
    if p.is_null() {
        say("FAIL: cannot allocate half the heap after reclaim");
        return false;
    }
    unsafe { h.dealloc(p, l) };
    true
}

/// Running out must be reported, not silently wrong.
fn test_exhaustion(h: &heap::K1kHeap) -> bool {
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
    if blocks.len() < 8 {
        say(&format!("FAIL: the heap ran out after {} blocks", blocks.len()));
        return false;
    }
    if heap::heap_stats().failures == 0 {
        say("FAIL: exhaustion was not counted");
        return false;
    }
    // A request bigger than the whole heap must fail cleanly.
    let huge = Layout::from_size_align(HEAP_SIZE * 2, 16).unwrap();
    if !unsafe { h.alloc(huge) }.is_null() {
        say("FAIL: an impossible request succeeded");
        return false;
    }
    for p in blocks {
        unsafe { h.dealloc(p, l) };
    }
    let s = heap::heap_stats();
    if s.in_use != 0 {
        say(&format!("FAIL: {} bytes still in use", s.in_use));
        return false;
    }
    true
}

/// The growth pattern `Vec` and `String` produce: doubling, with the old bytes
/// carried over and the new tail left alone.
fn test_growth(h: &heap::K1kHeap) -> bool {
    let mut p = unsafe { h.alloc(Layout::from_size_align(1, 1).unwrap()) };
    let mut size = 1usize;
    let mut cap = 1usize;
    unsafe { std::ptr::write_bytes(p, 0x5A, 1) };
    for step in 0..20 {
        let want = cap * 2;
        let q = unsafe { h.realloc(p, Layout::from_size_align(size, 1).unwrap(), want) };
        if q.is_null() {
            say("FAIL: growth realloc returned null");
            return false;
        }
        let old = unsafe { std::slice::from_raw_parts(q, size) };
        if old.iter().any(|b| *b != 0x5A) {
            say(&format!("FAIL: growth lost data at step {step}"));
            return false;
        }
        // Fill the fresh tail so the next round can check the whole buffer.
        unsafe { std::ptr::write_bytes(q.add(size), 0x5A, want - size) };
        p = q;
        size = want;
        cap = want;
        if let Err(why) = audit(&[]) {
            say(&format!("FAIL: audit during growth: {why}"));
            return false;
        }
    }
    unsafe { h.dealloc(p, Layout::from_size_align(size, 1).unwrap()) };
    if let Err(why) = audit(&[]) {
        say(&format!("FAIL: audit after growth: {why}"));
        return false;
    }
    true
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
fn test_churn(h: &heap::K1kHeap, rounds: usize) -> bool {
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
                // Out of memory is a legitimate answer: the churn keeps
                // growing until the heap is full.
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
                // Same here: the old block stays valid, which the audit below
                // checks.
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
        if let Err(why) = audit(&live) {
            say(&format!("FAIL: audit at churn round {round}: {why}"));
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
    say(&format!("churn: {rounds} rounds, {oom} request(s) refused for lack of memory"));
    for l in &live {
        if !l.intact() {
            say("FAIL: churn corrupted a live block at the end");
            return false;
        }
        unsafe { h.dealloc(l.ptr, l.layout()) };
    }
    let s = heap::heap_stats();
    if s.in_use != 0 || s.live != 0 {
        say(&format!(
            "FAIL: churn left in_use={} live={}",
            s.in_use, s.live
        ));
        return false;
    }
    if let Err(why) = audit(&[]) {
        say(&format!("FAIL: audit after churn: {why}"));
        return false;
    }
    true
}

fn main() {
    let h = heap::K1kHeap;
    let mut ok = true;
    if let Err(why) = audit(&[]) {
        say(&format!("FAIL: the heap is not clean at boot: {why}"));
        std::process::exit(1);
    }
    ok &= test_basic(&h);
    ok &= test_reclaim(&h);
    ok &= test_exhaustion(&h);
    ok &= test_growth(&h);
    ok &= test_churn(&h, 50_000);
    let s = heap::heap_stats();
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
