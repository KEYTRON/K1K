//! Physical memory manager: a bitmap frame allocator built from the Limine
//! memory map. One bit per 4 KiB frame; the bitmap itself lives in the largest
//! usable region and is accessed through the HHDM.

use core::ptr::addr_of_mut;
use limine::memmap::{Entry, MEMMAP_BOOTLOADER_RECLAIMABLE, MEMMAP_USABLE};
use spin::Mutex;
use x86_64::instructions::interrupts;
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

/// Bootloader-reclaimable regions, copied out of Limine's memory map so the
/// map itself can be freed. Limine reports a handful of regions at most.
const MAX_RECLAIM: usize = 16;
static mut RECLAIM: [(u64, u64); MAX_RECLAIM] = [(0, 0); MAX_RECLAIM];
static mut RECLAIM_N: usize = 0;
static mut RECLAIMED: bool = false;

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

    /// `count` adjacent free frames, found a word at a time: a fully used word
    /// is skipped in one step, so a large map costs `frames / 64` iterations
    /// rather than `frames`.
    fn alloc_contiguous(&mut self, count: usize) -> Option<usize> {
        if count == 0 || count > self.frames {
            return None;
        }
        let mut run = 0usize;
        let mut start = 0usize;
        for w in 0..self.bits.len() {
            let mut free = !self.bits[w];
            while free != 0 {
                let idx = w * 64 + free.trailing_zeros() as usize;
                if idx >= self.frames {
                    return None;
                }
                if run == 0 {
                    start = idx;
                }
                run += 1;
                if run == count {
                    for j in start..=idx {
                        self.set_used(j);
                    }
                    self.free -= count;
                    return Some(start);
                }
                free &= free - 1;
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
}

pub fn stats() -> Stats {
    interrupts::without_interrupts(|| {
        let g = PMM.lock();
        let b = g.as_ref().expect("pmm not initialised");
        Stats {
            total_usable_kib: b.total_usable * 4,
            free_kib: b.free * 4,
        }
    })
}

pub fn init(hhdm_offset: u64, entries: &[&Entry]) {
    unsafe { *addr_of_mut!(HHDM_OFFSET) = hhdm_offset };

    // The bitmap has to reach the highest address we may ever free, which
    // includes the bootloader's structures: they usually sit above RAM, and
    // leaving them out would mean never giving that memory back.
    let mut usable_end = 0u64;
    let mut highest = 0u64;
    let mut usable_frames = 0usize;
    let mut largest: Option<&Entry> = None;
    for e in entries {
        if e.type_ == MEMMAP_USABLE {
            usable_end = usable_end.max(e.base + e.length);
            usable_frames += (e.length / FRAME_SIZE) as usize;
            if largest.is_none_or(|l| e.length > l.length) {
                largest = Some(e);
            }
        }
        if e.type_ == MEMMAP_USABLE || e.type_ == MEMMAP_BOOTLOADER_RECLAIMABLE {
            highest = highest.max(e.base + e.length);
        }
    }
    let largest = largest.expect("no usable memory");
    // Never spend more than an eighth of the largest usable region on the map.
    let budget = (largest.length / 8) as usize;
    let mut frames = (highest / FRAME_SIZE) as usize;
    let mut words = frames.div_ceil(64);
    let mut bitmap_bytes = words * 8;
    if bitmap_bytes > budget {
        frames = (usable_end / FRAME_SIZE) as usize;
        words = frames.div_ceil(64);
        bitmap_bytes = words * 8;
        klog!(
            "pmm",
            "memory map would need {} KiB, over the {} KiB budget: bootloader memory above RAM is kept",
            (highest / FRAME_SIZE).div_ceil(64) as u64 * 8 / 1024,
            budget / 1024
        );
    }
    assert!(
        largest.length as usize >= bitmap_bytes,
        "largest region too small for bitmap"
    );

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

    // Remember the reclaimable regions: after `reclaim_bootloader` the memory
    // map itself is gone, and so is every response hanging off it.
    let mut reclaimable = 0u64;
    let mut n = 0usize;
    for e in entries
        .iter()
        .filter(|e| e.type_ == MEMMAP_BOOTLOADER_RECLAIMABLE)
    {
        reclaimable += e.length;
        if n < MAX_RECLAIM {
            unsafe {
                let slot = core::ptr::addr_of_mut!(RECLAIM[n]);
                (*slot).0 = e.base;
                (*slot).1 = e.length;
            }
            n += 1;
        } else {
            klog!(
                "pmm",
                "more reclaimable regions than we track; {} KiB kept",
                e.length / 1024
            );
        }
    }
    unsafe { RECLAIM_N = n };

    klog!(
        "pmm",
        "{} MiB usable in {} regions, bitmap {} KiB, {} KiB bootloader-reclaimable",
        usable_frames * 4 / 1024,
        entries.iter().filter(|e| e.type_ == MEMMAP_USABLE).count(),
        bitmap_bytes / 1024,
        reclaimable / 1024
    );

    interrupts::without_interrupts(|| *PMM.lock() = Some(bm));
}

/// Give the bootloader's memory back to the allocator.
///
/// Only safe once boot is finished with Limine: the command line is copied by
/// [`crate::boot::init`], the ACPI tables are parsed into owned structures, the
/// framebuffer console copied its geometry, and the application processors have
/// been released. One caveat is documented on [`reclaim_bootloader_unsafe`]:
/// the HHDM aliases of the ACPI tables are left in place.
pub fn reclaim_bootloader() -> u64 {
    if unsafe { RECLAIMED } {
        return 0;
    }
    let mut freed = 0u64;
    let n = unsafe { RECLAIM_N };
    for i in 0..n {
        let (base, length) = unsafe { RECLAIM[i] };
        let first = (base / FRAME_SIZE) as usize;
        let count = (length / FRAME_SIZE) as usize;
        let mut freed_here = 0u64;
        interrupts::without_interrupts(|| {
            if let Some(bm) = PMM.lock().as_mut() {
                // The reclaimable range can reach past the last usable frame
                // (Limine keeps structures above RAM), so stop at the end of
                // the map rather than trusting the entry.
                for j in first..(first + count).min(bm.frames) {
                    // These frames start out marked used: they are not part of
                    // any usable region, so nothing of ours can be in there.
                    if bm.is_used(j) {
                        bm.set_free(j);
                        bm.free += 1;
                        freed_here += FRAME_SIZE;
                    }
                }
            }
        });
        freed += freed_here;
    }
    unsafe { RECLAIMED = true };
    klog!(
        "pmm",
        "reclaimed {} KiB of bootloader memory ({} region(s))",
        freed / 1024,
        n
    );
    freed
}

/// Whether the bootloader's memory has been handed back yet.
pub fn bootloader_reclaimed() -> bool {
    unsafe { RECLAIMED }
}

pub fn alloc_frame() -> Option<PhysFrame> {
    let idx = interrupts::without_interrupts(|| PMM.lock().as_mut()?.alloc())?;
    Some(PhysFrame::containing_address(PhysAddr::new(
        idx as u64 * FRAME_SIZE,
    )))
}

/// Allocate a frame and zero it through the HHDM.
pub fn alloc_zeroed_frame() -> Option<PhysFrame> {
    let f = alloc_frame()?;
    unsafe {
        core::ptr::write_bytes(
            phys_to_virt(f.start_address()).as_mut_ptr::<u8>(),
            0,
            FRAME_SIZE as usize,
        );
    }
    Some(f)
}

/// Allocate `count` physically contiguous zeroed frames; returns the first.
pub fn alloc_contiguous_zeroed(count: usize) -> Option<PhysFrame> {
    let idx = interrupts::without_interrupts(|| PMM.lock().as_mut()?.alloc_contiguous(count))?;
    let first = PhysFrame::containing_address(PhysAddr::new(idx as u64 * FRAME_SIZE));
    unsafe {
        core::ptr::write_bytes(
            phys_to_virt(first.start_address()).as_mut_ptr::<u8>(),
            0,
            count * FRAME_SIZE as usize,
        );
    }
    Some(first)
}

pub fn free_frame(frame: PhysFrame) {
    let idx = (frame.start_address().as_u64() / FRAME_SIZE) as usize;
    interrupts::without_interrupts(|| {
        if let Some(bm) = PMM.lock().as_mut() {
            bm.release(idx);
        }
    });
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
