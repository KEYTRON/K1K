pub mod heap;
pub mod pmm;
pub mod vmm;

use crate::boot;

pub fn init() {
    let memmap = boot::MEMMAP.response().expect("limine: no memory map");
    pmm::init(boot::hhdm_offset(), memmap.entries());
    vmm::init();
    heap::init();
}
