use linked_list_allocator::LockedHeap;
use x86_64::VirtAddr;

use super::vmm::{self, Flags};
use crate::klog;

#[global_allocator]
pub static HEAP: LockedHeap = LockedHeap::empty();

pub const HEAP_SIZE: usize = 16 * 1024 * 1024;

pub fn init() {
    let start = VirtAddr::new(vmm::KERNEL_HEAP_START);
    let pages = HEAP_SIZE / 4096;
    vmm::map_kernel_pages(start, pages, Flags::WRITABLE | Flags::NO_EXECUTE).expect("heap mapping failed");
    unsafe { HEAP.lock().init(start.as_mut_ptr(), HEAP_SIZE) };
    klog!("heap", "{} MiB at {:#x}", HEAP_SIZE / 1024 / 1024, vmm::KERNEL_HEAP_START);
}

pub fn stats() -> (usize, usize) {
    let h = HEAP.lock();
    (h.used(), h.free())
}
