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
use crate::obj::MemoryObject;

pub use x86_64::structures::paging::PageTableFlags as Flags;

/// Kernel heap window (PML4 slot 288), far away from HHDM (256) and the image (511).
pub const KERNEL_HEAP_START: u64 = 0xffff_9000_0000_0000;
/// Top of the canonical lower half; user stacks grow down from here.
pub const USER_STACK_TOP: u64 = 0x0000_7fff_ffff_0000;

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

/// Map `count` fresh frames at `start` in the kernel address space.
pub fn map_kernel_pages(
    start: VirtAddr,
    count: usize,
    flags: PageTableFlags,
) -> Result<(), MapToError<Size4KiB>> {
    let mut mapper = mapper_for(kernel_pml4());
    let mut alloc = GlobalFrameAllocator;
    for i in 0..count {
        let page = Page::containing_address(start + (i as u64) * pmm::FRAME_SIZE);
        let frame = pmm::alloc_frame().ok_or(MapToError::FrameAllocationFailed)?;
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
            }
        }
        p += pmm::FRAME_SIZE;
    }
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
        self.unmap_shared();
        self.free_user_half();
        pmm::free_frame(self.pml4);
    }
}
