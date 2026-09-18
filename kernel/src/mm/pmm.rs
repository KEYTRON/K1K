//! Physical memory manager: a bitmap frame allocator built from the Limine
//! memory map. One bit per 4 KiB frame; the bitmap itself lives in the largest
//! usable region and is accessed through the HHDM.

use core::ptr::addr_of_mut;
use limine::memmap::{Entry, MEMMAP_BOOTLOADER_RECLAIMABLE, MEMMAP_USABLE};
use spin::Mutex;
use x86_64::structures::paging::{FrameAllocator, FrameDeallocator, PhysFrame, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

use crate::klog;

pub const FRAME_SIZE: u64 = 4096;

static mut HHDM_OFFSET: u64 = 0;

#[inline]
pub fn hhdm() -> u64 {
    unsafe { *addr_of_mut!(HHDM_OFFSET) }
}

#[inline]
pub fn phys_to_virt(p: PhysAddr) -> VirtAddr {
    VirtAddr::new(p.as_u64() + hhdm())
}

#[inline]
pub fn virt_to_phys_hhdm(v: VirtAddr) -> PhysAddr {
    PhysAddr::new(v.as_u64() - hhdm())
}

struct Bitmap {
    bits: &'static mut [u64],
    frames: usize,
    free: usize,
    total_usable: usize,
    next_hint: usize,
}

impl Bitmap {
    #[inline]
    fn is_used(&self, i: usize) -> bool {
        self.bits[i / 64] & (1 << (i % 64)) != 0
    }
    #[inline]
    fn set_used(&mut self, i: usize) {
        self.bits[i / 64] |= 1 << (i % 64);
    }
    #[inline]
    fn set_free(&mut self, i: usize) {
        self.bits[i / 64] &= !(1 << (i % 64));
    }

    fn alloc(&mut self) -> Option<usize> {
        let words = self.bits.len();
        let start = self.next_hint / 64;
        for step in 0..words {
            let w = (start + step) % words;
            if self.bits[w] != u64::MAX {
                let bit = (!self.bits[w]).trailing_zeros() as usize;
                let idx = w * 64 + bit;
                if idx >= self.frames {
                    continue;
                }
                self.set_used(idx);
                self.free -= 1;
                self.next_hint = idx + 1;
                return Some(idx);
            }
        }
        None
    }

    fn alloc_contiguous(&mut self, count: usize) -> Option<usize> {
        let mut run = 0;
        for i in 0..self.frames {
            if self.is_used(i) {
                run = 0;
                continue;
            }
            run += 1;
            if run == count {
                let first = i + 1 - count;
                for j in first..=i {
                    self.set_used(j);
                }
                self.free -= count;
                return Some(first);
            }
        }
        None
    }

    fn release(&mut self, i: usize) {
        debug_assert!(self.is_used(i), "double free of frame {i}");
        self.set_free(i);
        self.free += 1;
        if i < self.next_hint {
            self.next_hint = i;
        }
    }
}

static PMM: Mutex<Option<Bitmap>> = Mutex::new(None);

pub struct Stats {
    pub total_usable_kib: usize,
    pub free_kib: usize,
    pub used_kib: usize,
}

pub fn stats() -> Stats {
    let g = PMM.lock();
    let b = g.as_ref().expect("pmm not initialised");
    Stats {
        total_usable_kib: b.total_usable * 4,
        free_kib: b.free * 4,
        used_kib: (b.total_usable - b.free) * 4,
    }
}

pub fn init(hhdm_offset: u64, entries: &[&Entry]) {
    unsafe { *addr_of_mut!(HHDM_OFFSET) = hhdm_offset };

    let mut highest = 0u64;
    let mut usable_frames = 0usize;
    let mut largest: Option<&Entry> = None;
    for e in entries {
        if e.type_ == MEMMAP_USABLE {
            highest = highest.max(e.base + e.length);
            usable_frames += (e.length / FRAME_SIZE) as usize;
            if largest.is_none_or(|l| e.length > l.length) {
                largest = Some(e);
            }
        }
    }
    let frames = (highest / FRAME_SIZE) as usize;
    let words = frames.div_ceil(64);
    let bitmap_bytes = words * 8;
    let largest = largest.expect("no usable memory");
    assert!(largest.length as usize >= bitmap_bytes, "largest region too small for bitmap");

    let bits: &'static mut [u64] = unsafe {
        let p = phys_to_virt(PhysAddr::new(largest.base)).as_mut_ptr::<u64>();
        core::ptr::write_bytes(p, 0xFF, words);
        core::slice::from_raw_parts_mut(p, words)
    };

    let mut bm = Bitmap {
        bits,
        frames,
        free: 0,
        total_usable: usable_frames,
        next_hint: 0,
    };

    for e in entries {
        if e.type_ == MEMMAP_USABLE {
            let first = (e.base / FRAME_SIZE) as usize;
            let n = (e.length / FRAME_SIZE) as usize;
            for i in first..first + n {
                bm.set_free(i);
            }
            bm.free += n;
        }
    }

    let bm_first = (largest.base / FRAME_SIZE) as usize;
    let bm_frames = (bitmap_bytes as u64).div_ceil(FRAME_SIZE) as usize;
    for i in bm_first..bm_first + bm_frames {
        bm.set_used(i);
    }
    bm.free -= bm_frames;

    let reclaimable: u64 = entries
        .iter()
        .filter(|e| e.type_ == MEMMAP_BOOTLOADER_RECLAIMABLE)
        .map(|e| e.length)
        .sum();

    klog!(
        "pmm",
        "{} MiB usable in {} regions, bitmap {} KiB, {} KiB bootloader-reclaimable (kept)",
        usable_frames * 4 / 1024,
        entries.iter().filter(|e| e.type_ == MEMMAP_USABLE).count(),
        bitmap_bytes / 1024,
        reclaimable / 1024
    );

    *PMM.lock() = Some(bm);
}

pub fn alloc_frame() -> Option<PhysFrame> {
    let idx = PMM.lock().as_mut()?.alloc()?;
    Some(PhysFrame::containing_address(PhysAddr::new(idx as u64 * FRAME_SIZE)))
}

/// Allocate a frame and zero it through the HHDM.
pub fn alloc_zeroed_frame() -> Option<PhysFrame> {
    let f = alloc_frame()?;
    unsafe {
        core::ptr::write_bytes(phys_to_virt(f.start_address()).as_mut_ptr::<u8>(), 0, FRAME_SIZE as usize);
    }
    Some(f)
}

pub fn alloc_contiguous(count: usize) -> Option<PhysFrame> {
    let idx = PMM.lock().as_mut()?.alloc_contiguous(count)?;
    Some(PhysFrame::containing_address(PhysAddr::new(idx as u64 * FRAME_SIZE)))
}

pub fn free_frame(frame: PhysFrame) {
    let idx = (frame.start_address().as_u64() / FRAME_SIZE) as usize;
    if let Some(bm) = PMM.lock().as_mut() {
        bm.release(idx);
    }
}

/// Adapter so the paging code can pull frames straight from the PMM.
pub struct GlobalFrameAllocator;

unsafe impl FrameAllocator<Size4KiB> for GlobalFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        alloc_frame()
    }
}

impl FrameDeallocator<Size4KiB> for GlobalFrameAllocator {
    unsafe fn deallocate_frame(&mut self, frame: PhysFrame) {
        free_frame(frame);
    }
}
