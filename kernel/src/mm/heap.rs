use core::alloc::{GlobalAlloc, Layout};
use linked_list_allocator::LockedHeap;
use x86_64::VirtAddr;
use x86_64::instructions::interrupts;

use super::vmm::{self, Flags};
use crate::klog;

/// The kernel heap. Allocations run with interrupts disabled so an interrupt
/// handler on this CPU can never spin on a heap lock its own CPU holds — and,
/// by extension, the scheduler lock can never wait on a heap holder that is
/// itself waiting on the scheduler.
pub struct KernelHeap(LockedHeap);

unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        interrupts::without_interrupts(|| unsafe { self.0.alloc(layout) })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        interrupts::without_interrupts(|| unsafe { self.0.dealloc(ptr, layout) })
    }
}

#[global_allocator]
pub static HEAP: KernelHeap = KernelHeap(LockedHeap::empty());

pub const HEAP_SIZE: usize = 16 * 1024 * 1024;

pub fn init() {
    let start = VirtAddr::new(vmm::KERNEL_HEAP_START);
    let pages = HEAP_SIZE / 4096;
    vmm::map_kernel_pages(start, pages, Flags::WRITABLE | Flags::NO_EXECUTE)
        .expect("heap mapping failed");
    unsafe { HEAP.0.lock().init(start.as_mut_ptr(), HEAP_SIZE) };
    klog!(
        "heap",
        "{} MiB at {:#x}",
        HEAP_SIZE / 1024 / 1024,
        vmm::KERNEL_HEAP_START
    );
}

pub fn stats() -> (usize, usize) {
    interrupts::without_interrupts(|| {
        let h = HEAP.0.lock();
        (h.used(), h.free())
    })
}
