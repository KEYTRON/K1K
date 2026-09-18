use linked_list_allocator::LockedHeap;

#[global_allocator]
pub static HEAP: LockedHeap = LockedHeap::empty();

pub const HEAP_SIZE: usize = 8 * 1024 * 1024;
