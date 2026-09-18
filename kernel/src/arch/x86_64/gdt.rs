//! Per-CPU GDT + TSS. Every CPU gets the same descriptor layout (so selectors
//! are global constants) but its own TSS, ring-0 stack pointer and IST stack.

use alloc::boxed::Box;
use core::ptr::addr_of_mut;
use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{CS, DS, ES, FS, GS, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

const IST_STACK_SIZE: usize = 32 * 1024;

#[repr(C, align(16))]
pub struct CpuTables {
    ist_stack: [u8; IST_STACK_SIZE],
    tss: TaskStateSegment,
    gdt: GlobalDescriptorTable,
}

#[derive(Clone, Copy)]
pub struct Selectors {
    pub kernel_code: SegmentSelector,
    pub kernel_data: SegmentSelector,
    pub user_data: SegmentSelector,
    pub user_code: SegmentSelector,
}

static mut BSP_TABLES: CpuTables = CpuTables::empty();
static mut SELECTORS: Option<Selectors> = None;

impl CpuTables {
    const fn empty() -> Self {
        Self {
            ist_stack: [0; IST_STACK_SIZE],
            tss: TaskStateSegment::new(),
            gdt: GlobalDescriptorTable::new(),
        }
    }

    /// Fill in and load this CPU's tables. Returns the TSS pointer for the
    /// per-CPU block; selectors are recorded once (identical on every CPU).
    unsafe fn load(this: *mut CpuTables) -> *mut TaskStateSegment {
        unsafe {
            let t = &mut *this;
            let ist_top = VirtAddr::from_ptr(t.ist_stack.as_ptr()) + IST_STACK_SIZE as u64;
            t.tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = ist_top;

            let kernel_code = t.gdt.append(Descriptor::kernel_code_segment());
            let kernel_data = t.gdt.append(Descriptor::kernel_data_segment());
            let user_data = t.gdt.append(Descriptor::user_data_segment());
            let user_code = t.gdt.append(Descriptor::user_code_segment());
            let tss_sel = t.gdt.append(Descriptor::tss_segment(&*addr_of_mut!(t.tss)));
            t.gdt.load();

            CS::set_reg(kernel_code);
            DS::set_reg(kernel_data);
            ES::set_reg(kernel_data);
            SS::set_reg(kernel_data);
            FS::set_reg(SegmentSelector(0));
            GS::set_reg(SegmentSelector(0));
            load_tss(tss_sel);

            if (*addr_of_mut!(SELECTORS)).is_none() {
                *addr_of_mut!(SELECTORS) = Some(Selectors {
                    kernel_code,
                    kernel_data,
                    user_data,
                    user_code,
                });
            }
            addr_of_mut!(t.tss)
        }
    }
}

pub fn selectors() -> Selectors {
    unsafe { (*addr_of_mut!(SELECTORS)).expect("gdt not initialised") }
}

/// Bootstrap processor: static tables, no heap needed.
pub fn init_bsp() -> *mut TaskStateSegment {
    unsafe { CpuTables::load(addr_of_mut!(BSP_TABLES)) }
}

/// Allocate tables for an application processor (on the BSP, heap required).
pub fn alloc_ap() -> *mut CpuTables {
    Box::leak(Box::new(CpuTables::empty()))
}

pub fn tss_ptr(tables: *mut CpuTables) -> *mut TaskStateSegment {
    unsafe { addr_of_mut!((*tables).tss) }
}

/// Load previously allocated tables on the AP itself.
pub unsafe fn load_ap(tables: *mut CpuTables) -> *mut TaskStateSegment {
    unsafe { CpuTables::load(tables) }
}

/// Kernel stack used when a ring-3 thread traps into the kernel on this CPU.
pub fn set_kernel_stack(top: VirtAddr) {
    let tss = super::percpu::get().tss;
    unsafe { (*tss).privilege_stack_table[0] = top };
}
