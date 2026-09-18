use core::ptr::addr_of_mut;
use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{CS, DS, ES, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

const IST_STACK_SIZE: usize = 32 * 1024;

#[repr(align(16))]
#[allow(dead_code)]
struct Stack([u8; IST_STACK_SIZE]);

static mut DF_STACK: Stack = Stack([0; IST_STACK_SIZE]);
static mut TSS: TaskStateSegment = TaskStateSegment::new();
static mut GDT: GlobalDescriptorTable = GlobalDescriptorTable::new();

#[derive(Clone, Copy)]
pub struct Selectors {
    pub kernel_code: SegmentSelector,
    pub kernel_data: SegmentSelector,
    pub user_data: SegmentSelector,
    pub user_code: SegmentSelector,
}

static mut SELECTORS: Option<Selectors> = None;

pub fn selectors() -> Selectors {
    unsafe { (*addr_of_mut!(SELECTORS)).expect("gdt not initialised") }
}

fn stack_top(stack: *mut Stack) -> VirtAddr {
    VirtAddr::from_ptr(stack) + IST_STACK_SIZE as u64
}

pub fn init() {
    unsafe {
        let tss = &mut *addr_of_mut!(TSS);
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] =
            stack_top(addr_of_mut!(DF_STACK));

        let gdt = &mut *addr_of_mut!(GDT);
        let kernel_code = gdt.append(Descriptor::kernel_code_segment());
        let kernel_data = gdt.append(Descriptor::kernel_data_segment());
        let user_data = gdt.append(Descriptor::user_data_segment());
        let user_code = gdt.append(Descriptor::user_code_segment());
        let tss_sel = gdt.append(Descriptor::tss_segment(&*addr_of_mut!(TSS)));
        gdt.load();

        CS::set_reg(kernel_code);
        DS::set_reg(kernel_data);
        ES::set_reg(kernel_data);
        SS::set_reg(kernel_data);
        load_tss(tss_sel);

        *addr_of_mut!(SELECTORS) = Some(Selectors {
            kernel_code,
            kernel_data,
            user_data,
            user_code,
        });
    }
}

/// Kernel stack used when a ring-3 thread traps into the kernel.
pub fn set_kernel_stack(top: VirtAddr) {
    unsafe {
        (*addr_of_mut!(TSS)).privilege_stack_table[0] = top;
    }
}
