//! One IDT shared by all CPUs; every vector points at its asm stub from
//! `trap_stubs.s`, so the same frame layout reaches `trap_dispatch`.

use core::ptr::addr_of_mut;
use x86_64::VirtAddr;
use x86_64::structures::idt::InterruptDescriptorTable;

use super::{gdt, trap};

pub const IRQ_BASE: u8 = 32;
pub const IRQ_TIMER: u8 = IRQ_BASE;
pub const IRQ_KEYBOARD: u8 = IRQ_BASE + 1;

static mut IDT: InterruptDescriptorTable = InterruptDescriptorTable::new();

pub fn init() {
    unsafe {
        let idt = &mut *addr_of_mut!(IDT);
        let table = &*core::ptr::addr_of!(trap::trap_stub_table);
        let stub = |v: usize| VirtAddr::new(table[v]);

        idt.divide_error.set_handler_addr(stub(0));
        idt.debug.set_handler_addr(stub(1));
        idt.non_maskable_interrupt.set_handler_addr(stub(2));
        idt.breakpoint.set_handler_addr(stub(3));
        idt.overflow.set_handler_addr(stub(4));
        idt.bound_range_exceeded.set_handler_addr(stub(5));
        idt.invalid_opcode.set_handler_addr(stub(6));
        idt.device_not_available.set_handler_addr(stub(7));
        idt.double_fault
            .set_handler_addr(stub(8))
            .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
        idt.invalid_tss.set_handler_addr(stub(10));
        idt.segment_not_present.set_handler_addr(stub(11));
        idt.stack_segment_fault.set_handler_addr(stub(12));
        idt.general_protection_fault.set_handler_addr(stub(13));
        idt.page_fault.set_handler_addr(stub(14));
        idt.x87_floating_point.set_handler_addr(stub(16));
        idt.alignment_check.set_handler_addr(stub(17));
        idt.machine_check.set_handler_addr(stub(18));
        idt.simd_floating_point.set_handler_addr(stub(19));
        idt.virtualization.set_handler_addr(stub(20));
        idt.cp_protection_exception.set_handler_addr(stub(21));
        idt.hv_injection_exception.set_handler_addr(stub(28));
        idt.vmm_communication_exception.set_handler_addr(stub(29));
        idt.security_exception.set_handler_addr(stub(30));

        for v in IRQ_BASE..=255 {
            idt[v].set_handler_addr(stub(v as usize));
        }
        idt.load();
    }
}

/// Load the (already built) IDT on an application processor.
pub fn load() {
    unsafe { (*addr_of_mut!(IDT)).load() };
}
