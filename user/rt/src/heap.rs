//! The per-service heap.
//!
//! The supervisor maps a private range into every ring-3 task and writes its
//! bounds into the boot info page; `k1k-rt` hands that range to the shared
//! [`k1k_alloc`] heap and becomes the program's global allocator, so services
//! can use `Box`, `Vec`, `String` and `format!`.
//!
//! A task is never running on two CPUs at once and the kernel never touches a
//! user heap, so the allocator needs no locking here — see [`k1k_alloc::GlobalHeap`]
//! for what that promise is. The range is fixed: growing it would mean asking
//! the kernel for another memory object mid-allocation, which is a syscall on a
//! path that must not take one.
//!
//! The algorithm itself is tested on the host against an independent reading of
//! its own block list (`make test-heap`).

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicBool, Ordering};

use k1k_alloc::{Fixed, GlobalHeap, Heap, Stats};

use crate::boot_info;

pub use k1k_alloc::ALIGN;

static HEAP: GlobalHeap<Fixed> = GlobalHeap::new(Fixed);

/// The allocator the program actually allocates through.
///
/// It has to be a wrapper rather than [`HEAP`] itself: the region comes from
/// the boot info page, and the very first allocation of a program happens
/// before any of the program's own code runs — Rust's `__rust_alloc` goes
/// straight at the global allocator, so an allocator that only initialises when
/// asked politely would hand out null to the first `format!` it ever sees.
struct ServiceHeap;

unsafe impl GlobalAlloc for ServiceHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        init();
        // Safety: a task is never on two CPUs at once, as described above.
        unsafe { HEAP.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // Safety: `ptr` came from `alloc` on this task's heap.
        unsafe { HEAP.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        init();
        // Safety: as above.
        unsafe { HEAP.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        init();
        // Safety: as above.
        unsafe { HEAP.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static ALLOCATOR: ServiceHeap = ServiceHeap;

static READY: AtomicBool = AtomicBool::new(false);

/// Take the range from the boot info page the first time the allocator is used.
/// Everything after that needs no setup call in user code.
fn init() {
    if READY.load(Ordering::Acquire) {
        return;
    }
    let Some(info) = boot_info() else {
        return;
    };
    let base = info.heap_base as usize;
    let len = info.heap_size as usize;
    // Safety: the supervisor mapped this range for this task and nothing else
    // runs on its address space; we are the only user of the allocator.
    unsafe {
        HEAP.with(|h| h.add_region(k1k_alloc::Region { base, len }));
    }
    READY.store(true, Ordering::Release);
}

/// Allocate from the task's heap, failing only if the boot info page was never
/// filled: a panic in the service beats silent corruption.
pub fn alloc(size: usize, align: usize) -> *mut u8 {
    init();
    // Safety: single-threaded, as described at the top of the file.
    unsafe { HEAP.with(|h| h.alloc(size, align)) }
}

pub fn dealloc(ptr: *mut u8) {
    // Safety: `ptr` came from `alloc` on this task's heap.
    unsafe { HEAP.with(|h| h.dealloc(ptr)) };
}

/// Heap counters, for services that want to report their memory use. All
/// zeroes mean the service never allocated.
pub fn heap_stats() -> Stats {
    init();
    // Safety: as above.
    unsafe { HEAP.with(|h| h.stats()) }
}

/// Bytes still available in the task's heap, or 0 if the allocator never ran.
pub fn heap_free() -> usize {
    init();
    // Safety: as above.
    unsafe { HEAP.with(|h| h.free_bytes()) }
}

/// The heap itself, for callers that want to reason about it directly.
/// Safety: the caller must exclude other users of the allocator.
pub unsafe fn with_heap<R>(f: impl FnOnce(&mut Heap<Fixed>) -> R) -> R {
    init();
    unsafe { HEAP.with(f) }
}
