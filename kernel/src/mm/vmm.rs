//! Virtual memory: kernel page tables (inherited from Limine, extended in
//! place) and per-task user address spaces that share the kernel half.

use spin::Mutex;
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::structures::paging::mapper::{MapToError, UnmapError};
use x86_64::structures::paging::page_table::PageTableEntry;
use x86_64::structures::paging::{
    Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB, Translate,
};
use x86_64::{PhysAddr, VirtAddr};

use super::pmm::{self, GlobalFrameAllocator, phys_to_virt};

pub use x86_64::structures::paging::PageTableFlags as Flags;

/// Kernel heap window (PML4 slot 288), far away from HHDM (256) and the image (511).
pub const KERNEL_HEAP_START: u64 = 0xffff_9000_0000_0000;
/// Top of the canonical lower half; user stacks grow down from here.
pub const USER_STACK_TOP: u64 = 0x0000_7fff_ffff_0000;
pub const USER_CODE_BASE: u64 = 0x0000_0000_0040_0000;

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
}

/// Map `count` fresh frames at `start` in the kernel address space.
pub fn map_kernel_pages(start: VirtAddr, count: usize, flags: PageTableFlags) -> Result<(), MapToError<Size4KiB>> {
    let mut mapper = mapper_for(kernel_pml4());
    let mut alloc = GlobalFrameAllocator;
    for i in 0..count {
        let page = Page::containing_address(start + (i as u64) * pmm::FRAME_SIZE);
        let frame = pmm::alloc_frame().ok_or(MapToError::FrameAllocationFailed)?;
        unsafe {
            mapper
                .map_to(page, frame, flags | PageTableFlags::PRESENT | PageTableFlags::GLOBAL, &mut alloc)?
                .flush();
        }
    }
    Ok(())
}

pub fn translate_kernel(addr: VirtAddr) -> Option<PhysAddr> {
    mapper_for(kernel_pml4()).translate_addr(addr)
}

/// A user address space. The kernel half (PML4 entries 256..512) is shared with
/// the kernel page tables by pointing at the same lower-level tables.
pub struct AddressSpace {
    pml4: PhysFrame,
}

impl AddressSpace {
    pub fn new() -> Option<Self> {
        let pml4 = pmm::alloc_zeroed_frame()?;
        let src = table_at(kernel_pml4());
        let dst = table_at(pml4);
        for i in 256..512 {
            dst[i] = PageTableEntry::from(src[i].clone());
        }
        Some(Self { pml4 })
    }

    pub fn cr3(&self) -> PhysFrame {
        self.pml4
    }

    pub fn is_current(&self) -> bool {
        Cr3::read().0 == self.pml4
    }

    pub unsafe fn switch(&self) {
        if !self.is_current() {
            unsafe { Cr3::write(self.pml4, Cr3Flags::empty()) };
        }
    }

    /// Map a fresh zeroed frame at `page` with user permissions.
    pub fn map_user_page(&mut self, page: Page, flags: PageTableFlags) -> Result<PhysFrame, MapToError<Size4KiB>> {
        let frame = pmm::alloc_zeroed_frame().ok_or(MapToError::FrameAllocationFailed)?;
        let mut mapper = mapper_for(self.pml4);
        let mut alloc = GlobalFrameAllocator;
        unsafe {
            mapper
                .map_to(page, frame, flags | PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE, &mut alloc)?
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
            let pa = mapper.translate_addr(va).expect("write_user: destination not mapped");
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

    pub fn unmap_user_page(&mut self, page: Page) -> Result<(), UnmapError> {
        let mut mapper = mapper_for(self.pml4);
        let (frame, flush) = mapper.unmap(page)?;
        flush.ignore();
        pmm::free_frame(frame);
        Ok(())
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
        self.free_user_half();
        pmm::free_frame(self.pml4);
    }
}
