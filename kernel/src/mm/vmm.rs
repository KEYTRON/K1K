//! Virtual memory: kernel page tables (inherited from Limine, extended in
//! place) and per-task user address spaces that share the kernel half.

use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::mapper::MapToError;
use x86_64::structures::paging::page_table::PageTableEntry;
use x86_64::structures::paging::{
    Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB, Translate,
};
use x86_64::{PhysAddr, VirtAddr};

use super::pmm::{self, GlobalFrameAllocator, phys_to_virt};
use crate::arch::x86_64::percpu;
use crate::obj::MemoryObject;

pub use x86_64::structures::paging::PageTableFlags as Flags;

/// Kernel heap window (PML4 slot 288), far away from HHDM (256) and the image (511).
pub const KERNEL_HEAP_START: u64 = 0xffff_9000_0000_0000;
/// Top of the canonical lower half; user stacks grow down from here.
pub const USER_STACK_TOP: u64 = 0x0000_7fff_ffff_0000;

/// One page holding the [`BootInfo`] the supervisor writes before a ring-3
/// task starts. It sits below the stack with a gap, so a service overflow
/// cannot reach it.
pub const USER_INFO_BASE: u64 = USER_STACK_TOP - 0x0000_0000_0002_0000;
/// Per-service heap. The supervisor maps this range in every ring-3 task and
/// `k1k-rt` turns it into its global allocator, so services can use `Box`,
/// `Vec`, `String` and `format!`.
pub const USER_HEAP_BASE: u64 = 0x0000_7000_0000_0000;
pub const USER_HEAP_PAGES: usize = 1024; // 4 MiB

/// "K1KBOOT1" — identifies a filled [`USER_INFO_BASE`] page.
pub const BOOT_INFO_MAGIC: u64 = 0x3148_4F4F_425A_314B;
/// Bumped whenever the layout below changes; `k1k-rt` refuses a mismatch.
pub const BOOT_INFO_LAYOUT: u64 = 1;

/// The first bytes a ring-3 task sees, written by the supervisor at
/// [`USER_INFO_BASE`]: where its heap and stack are, and the launch arguments
/// the spawning service attached to the `spawn` call. `user/rt` mirrors this
/// layout — keep the two in step.
#[repr(C)]
pub struct BootInfo {
    pub magic: u64,
    pub layout: u64,
    pub task_id: u32,
    pub _pad: u32,
    pub heap_base: u64,
    pub heap_size: u64,
    pub stack_top: u64,
    pub arg_len: u64,
    // `arg_len` bytes of arguments follow, NUL-terminated.
}

/// Largest argument blob [`BootInfo`] can carry (one page, minus the header).
pub const MAX_BOOT_ARGS: usize = 1024;

static KERNEL_PML4: Mutex<Option<PhysFrame>> = Mutex::new(None);

pub fn kernel_pml4() -> PhysFrame {
    KERNEL_PML4.lock().expect("vmm not initialised")
}

fn table_at(frame: PhysFrame) -> &'static mut PageTable {
    unsafe { &mut *phys_to_virt(frame.start_address()).as_mut_ptr::<PageTable>() }
}

fn mapper_for(pml4: PhysFrame) -> OffsetPageTable<'static> {
    unsafe { OffsetPageTable::new(table_at(pml4), VirtAddr::new(pmm::hhdm())) }
}

pub fn init() {
    let (frame, _) = Cr3::read();
    *KERNEL_PML4.lock() = Some(frame);

    // User address spaces copy the kernel half of the PML4 when they are
    // created, so every kernel-half entry must already exist: later kernel
    // mappings (heap, MMIO above 512 GiB, ...) then only add lower-level
    // tables, which all address spaces share.
    let pml4 = table_at(frame);
    let mut added = 0;
    for i in 256..512 {
        if pml4[i].is_unused() {
            let pdpt = pmm::alloc_zeroed_frame().expect("out of memory for kernel PDPTs");
            pml4[i].set_frame(pdpt, PageTableFlags::PRESENT | PageTableFlags::WRITABLE);
            added += 1;
        }
    }
    crate::klog!("vmm", "kernel PML4 ready ({} PDPTs pre-populated)", added);
}

/// Kernel-half mappings are global, so a change to them is invisible to a CR3
/// reload on every CPU: tell them all once the mapping is in place.
fn announce_shared(start: VirtAddr, len: u64) {
    if percpu::count() > 1 {
        crate::klog!(
            "vmm",
            "shared map {:#x}..{:#x} on {} cpus",
            start.as_u64(),
            start.as_u64() + len,
            percpu::count()
        );
        crate::arch::x86_64::ipi::flush_shared_range(start, VirtAddr::new(start.as_u64() + len));
    }
}

/// Map `count` zeroed frames at `start` in the kernel address space.
///
/// The other CPUs are *not* told. Kernel-half mappings are global, so a CR3
/// reload elsewhere would not pick them up and a page-walk cache elsewhere may
/// still hold the old, absent entry, so somebody has to announce the range
/// afterwards with [`flush_shared_range`] — which is why the kernel heap does
/// it after leaving its critical section rather than from inside the allocator,
/// where an IPI would never be acknowledged.
pub fn map_kernel_pages_deferred(
    start: VirtAddr,
    count: usize,
    flags: PageTableFlags,
) -> Result<(), MapToError<Size4KiB>> {
    let mut mapper = mapper_for(kernel_pml4());
    let mut alloc = GlobalFrameAllocator;
    for i in 0..count {
        let page = Page::containing_address(start + (i as u64) * pmm::FRAME_SIZE);
        let frame = pmm::alloc_zeroed_frame().ok_or(MapToError::FrameAllocationFailed)?;
        unsafe {
            mapper
                .map_to(
                    page,
                    frame,
                    flags | PageTableFlags::PRESENT | PageTableFlags::GLOBAL,
                    &mut alloc,
                )?
                .flush();
        }
    }
    Ok(())
}

/// Make physical range `[phys, phys+len)` reachable through the HHDM. Pages
/// Limine already mapped are left alone; missing ones are added with `extra`.
fn map_phys_range(phys: PhysAddr, len: u64, extra: PageTableFlags) {
    let mut mapper = mapper_for(kernel_pml4());
    let mut alloc = GlobalFrameAllocator;
    let start = phys.as_u64() & !(pmm::FRAME_SIZE - 1);
    let end = (phys.as_u64() + len + pmm::FRAME_SIZE - 1) & !(pmm::FRAME_SIZE - 1);
    let mut added = 0u64;
    let mut p = start;
    while p < end {
        let va = phys_to_virt(PhysAddr::new(p));
        if mapper.translate_addr(va).is_none() {
            let page = Page::<Size4KiB>::containing_address(va);
            let frame = PhysFrame::containing_address(PhysAddr::new(p));
            unsafe {
                mapper
                    .map_to(
                        page,
                        frame,
                        PageTableFlags::PRESENT
                            | PageTableFlags::WRITABLE
                            | PageTableFlags::GLOBAL
                            | PageTableFlags::NO_EXECUTE
                            | extra,
                        &mut alloc,
                    )
                    .expect("map_phys_range")
                    .flush();
                added += pmm::FRAME_SIZE;
            }
        }
        p += pmm::FRAME_SIZE;
    }
    if added > 0 {
        // Only pages we actually added can be stale anywhere.
        announce_shared(
            VirtAddr::new(phys_to_virt(PhysAddr::new(start)).as_u64()),
            added,
        );
    }
}

/// Tell every CPU that a kernel-half range changed. Kernel-half mappings are
/// global, so a CR3 reload elsewhere would not pick them up, and a page-walk
/// cache elsewhere may still hold the old, absent entry.
pub fn flush_shared_range(start: VirtAddr, end: VirtAddr) {
    announce_shared(start, end.as_u64().saturating_sub(start.as_u64()));
}

/// Map firmware tables (ACPI etc.) that base revision 3 leaves out of the HHDM.
pub fn map_phys_hhdm(phys: PhysAddr, len: u64) {
    map_phys_range(phys, len, PageTableFlags::empty());
}

/// Map device registers uncached.
pub fn map_mmio(phys: PhysAddr, len: u64) {
    map_phys_range(
        phys,
        len,
        PageTableFlags::NO_CACHE | PageTableFlags::WRITE_THROUGH,
    );
}

/// Where shared memory objects get mapped in user space (grows upward).
const USER_MMAP_BASE: u64 = 0x0000_0010_0000_0000;

/// What backs a user mapping whose frames this address space does not own.
enum Foreign {
    Memory(#[allow(dead_code)] Arc<MemoryObject>),
    Device,
}

struct SharedMapping {
    start: VirtAddr,
    pages: usize,
    _owner: Foreign,
}

/// A user address space. The kernel half (PML4 entries 256..512) is shared with
/// the kernel page tables by pointing at the same lower-level tables.
pub struct AddressSpace {
    pml4: PhysFrame,
    shared: Vec<SharedMapping>,
    mmap_next: u64,
}

impl AddressSpace {
    pub fn new() -> Option<Self> {
        let pml4 = pmm::alloc_zeroed_frame()?;
        let src = table_at(kernel_pml4());
        let dst = table_at(pml4);
        for i in 256..512 {
            dst[i] = PageTableEntry::from(src[i].clone());
        }
        Some(Self {
            pml4,
            shared: Vec::new(),
            mmap_next: USER_MMAP_BASE,
        })
    }

    /// Map a shared memory object's frames at a fresh address; the frames stay
    /// owned by the object and are not freed with this address space.
    pub fn map_shared(&mut self, obj: Arc<MemoryObject>, writable: bool) -> Option<VirtAddr> {
        let start = VirtAddr::new(self.mmap_next);
        let pages = obj.pages();
        let mut flags =
            PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE | PageTableFlags::NO_EXECUTE;
        if writable {
            flags |= PageTableFlags::WRITABLE;
        }
        let mut mapper = mapper_for(self.pml4);
        let mut alloc = GlobalFrameAllocator;
        for (i, frame) in obj.frames().iter().enumerate() {
            let page = Page::containing_address(start + (i as u64) * pmm::FRAME_SIZE);
            unsafe {
                mapper
                    .map_to(page, *frame, flags, &mut alloc)
                    .ok()?
                    .ignore();
            }
        }
        // Leave a guard page between mappings.
        self.mmap_next += (pages as u64 + 1) * pmm::FRAME_SIZE;
        self.shared.push(SharedMapping {
            start,
            pages,
            _owner: Foreign::Memory(obj),
        });
        Some(start)
    }

    /// Map a device's MMIO range (uncached) at a fresh address.
    pub fn map_device(&mut self, phys: PhysAddr, size: u64) -> Option<VirtAddr> {
        let start = VirtAddr::new(self.mmap_next);
        let first = phys.as_u64() & !(pmm::FRAME_SIZE - 1);
        let end = (phys.as_u64() + size + pmm::FRAME_SIZE - 1) & !(pmm::FRAME_SIZE - 1);
        let pages = ((end - first) / pmm::FRAME_SIZE) as usize;
        let flags = PageTableFlags::PRESENT
            | PageTableFlags::WRITABLE
            | PageTableFlags::USER_ACCESSIBLE
            | PageTableFlags::NO_EXECUTE
            | PageTableFlags::NO_CACHE
            | PageTableFlags::WRITE_THROUGH;
        let mut mapper = mapper_for(self.pml4);
        let mut alloc = GlobalFrameAllocator;
        for i in 0..pages {
            let page = Page::<Size4KiB>::containing_address(start + (i as u64) * pmm::FRAME_SIZE);
            let frame = PhysFrame::<Size4KiB>::containing_address(PhysAddr::new(
                first + (i as u64) * pmm::FRAME_SIZE,
            ));
            unsafe {
                mapper.map_to(page, frame, flags, &mut alloc).ok()?.ignore();
            }
        }
        self.mmap_next += (pages as u64 + 1) * pmm::FRAME_SIZE;
        self.shared.push(SharedMapping {
            start,
            pages,
            _owner: Foreign::Device,
        });
        Some(start + (phys.as_u64() - first))
    }

    fn unmap_shared(&mut self) {
        let mut mapper = mapper_for(self.pml4);
        for m in self.shared.drain(..) {
            for i in 0..m.pages {
                let page: Page<Size4KiB> =
                    Page::containing_address(m.start + (i as u64) * pmm::FRAME_SIZE);
                if let Ok((_, flush)) = mapper.unmap(page) {
                    flush.ignore();
                }
            }
        }
    }

    pub fn cr3(&self) -> PhysFrame {
        self.pml4
    }

    pub fn is_current(&self) -> bool {
        Cr3::read().0 == self.pml4
    }

    /// Map a fresh zeroed frame at `page` with user permissions.
    pub fn map_user_page(
        &mut self,
        page: Page,
        flags: PageTableFlags,
    ) -> Result<PhysFrame, MapToError<Size4KiB>> {
        let frame = pmm::alloc_zeroed_frame().ok_or(MapToError::FrameAllocationFailed)?;
        let mut mapper = mapper_for(self.pml4);
        let mut alloc = GlobalFrameAllocator;
        unsafe {
            mapper
                .map_to(
                    page,
                    frame,
                    flags | PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE,
                    &mut alloc,
                )?
                .ignore();
        }
        Ok(frame)
    }

    /// Map `count` pages starting at `start`; returns the physical frames in order.
    pub fn map_user_range(
        &mut self,
        start: VirtAddr,
        count: usize,
        flags: PageTableFlags,
    ) -> Result<(), MapToError<Size4KiB>> {
        for i in 0..count {
            let page = Page::containing_address(start + (i as u64) * pmm::FRAME_SIZE);
            self.map_user_page(page, flags)?;
        }
        Ok(())
    }

    /// Copy `data` into the user mapping at `dst` (must already be mapped).
    pub fn write_user(&self, dst: VirtAddr, data: &[u8]) {
        let mapper = mapper_for(self.pml4);
        let mut off = 0usize;
        while off < data.len() {
            let va = dst + off as u64;
            let pa = mapper
                .translate_addr(va)
                .expect("write_user: destination not mapped");
            let page_off = (va.as_u64() % pmm::FRAME_SIZE) as usize;
            let chunk = ((pmm::FRAME_SIZE as usize) - page_off).min(data.len() - off);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    data[off..].as_ptr(),
                    phys_to_virt(pa).as_mut_ptr::<u8>(),
                    chunk,
                );
            }
            off += chunk;
        }
    }

    pub fn translate(&self, addr: VirtAddr) -> Option<PhysAddr> {
        mapper_for(self.pml4).translate_addr(addr)
    }

    fn free_user_half(&mut self) {
        let pml4 = table_at(self.pml4);
        for i in 0..256 {
            if pml4[i].is_unused() {
                continue;
            }
            free_table_recursive(pml4[i].frame().ok(), 3);
            pml4[i].set_unused();
        }
    }
}

fn free_table_recursive(frame: Option<PhysFrame>, level: u8) {
    let Some(frame) = frame else { return };
    let table = table_at(frame);
    for entry in table.iter() {
        if entry.is_unused() {
            continue;
        }
        if level > 1 {
            if !entry.flags().contains(PageTableFlags::HUGE_PAGE) {
                free_table_recursive(entry.frame().ok(), level - 1);
            }
        } else if let Ok(f) = entry.frame() {
            pmm::free_frame(f);
        }
    }
    pmm::free_frame(frame);
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        assert!(!self.is_current(), "dropping the active address space");
        // The frames behind these tables go straight back to the allocator, so
        // no CPU may still have this address space cached: a stale entry there
        // would be a stale entry onto somebody else's memory. Another CPU can
        // only have it loaded if a task of ours is running there, which the
        // supervisor rules out before a task is reaped — this makes the
        // guarantee hold even if that ever changes.
        if percpu::count() > 1 {
            crate::arch::x86_64::ipi::shootdown_all(self.pml4.start_address());
        }
        self.unmap_shared();
        self.free_user_half();
        pmm::free_frame(self.pml4);
    }
}
