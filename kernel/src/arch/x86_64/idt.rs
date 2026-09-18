use core::ptr::addr_of_mut;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use super::gdt;
use crate::{klog, println};

pub const IRQ_BASE: u8 = 32;
pub const IRQ_TIMER: u8 = IRQ_BASE;
pub const IRQ_KEYBOARD: u8 = IRQ_BASE + 1;

static mut IDT: InterruptDescriptorTable = InterruptDescriptorTable::new();

pub fn init() {
    unsafe {
        let idt = &mut *addr_of_mut!(IDT);
        idt.divide_error.set_handler_fn(divide_error);
        idt.debug.set_handler_fn(debug);
        idt.non_maskable_interrupt.set_handler_fn(nmi);
        idt.breakpoint.set_handler_fn(breakpoint);
        idt.overflow.set_handler_fn(overflow);
        idt.bound_range_exceeded.set_handler_fn(bound_range);
        idt.invalid_opcode.set_handler_fn(invalid_opcode);
        idt.device_not_available.set_handler_fn(device_not_available);
        idt.double_fault
            .set_handler_fn(double_fault)
            .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
        idt.invalid_tss.set_handler_fn(invalid_tss);
        idt.segment_not_present.set_handler_fn(segment_not_present);
        idt.stack_segment_fault.set_handler_fn(stack_segment);
        idt.general_protection_fault.set_handler_fn(general_protection);
        idt.page_fault.set_handler_fn(page_fault);
        idt.x87_floating_point.set_handler_fn(x87_fp);
        idt.alignment_check.set_handler_fn(alignment_check);
        idt.machine_check.set_handler_fn(machine_check);
        idt.simd_floating_point.set_handler_fn(simd_fp);

        idt[IRQ_TIMER].set_handler_fn(super::interrupts::timer_irq);
        idt[IRQ_KEYBOARD].set_handler_fn(super::interrupts::keyboard_irq);
        for v in (IRQ_BASE + 2)..=255 {
            idt[v].set_handler_fn(super::interrupts::spurious_irq);
        }
        idt.load();
    }
}

/// True when the trap came from ring 3 — the fault belongs to a user thread,
/// not the kernel, and is handled by killing/restarting that thread.
fn from_user(frame: &InterruptStackFrame) -> bool {
    frame.code_segment.rpl() == x86_64::PrivilegeLevel::Ring3
}

fn user_fault(frame: &InterruptStackFrame, what: &str, code: u64) -> ! {
    crate::sched::on_user_fault(what, code, frame.instruction_pointer.as_u64());
}

macro_rules! trap {
    ($name:ident, $msg:expr) => {
        extern "x86-interrupt" fn $name(frame: InterruptStackFrame) {
            if from_user(&frame) {
                user_fault(&frame, $msg, 0);
            }
            kernel_panic($msg, &frame, None);
        }
    };
    ($name:ident, $msg:expr, code) => {
        extern "x86-interrupt" fn $name(frame: InterruptStackFrame, code: u64) {
            if from_user(&frame) {
                user_fault(&frame, $msg, code);
            }
            kernel_panic($msg, &frame, Some(code));
        }
    };
}

trap!(divide_error, "#DE divide error");
trap!(overflow, "#OF overflow");
trap!(bound_range, "#BR bound range exceeded");
trap!(invalid_opcode, "#UD invalid opcode");
trap!(device_not_available, "#NM device not available");
trap!(invalid_tss, "#TS invalid TSS", code);
trap!(segment_not_present, "#NP segment not present", code);
trap!(stack_segment, "#SS stack segment fault", code);
trap!(general_protection, "#GP general protection fault", code);
trap!(x87_fp, "#MF x87 floating point");
trap!(alignment_check, "#AC alignment check", code);
trap!(simd_fp, "#XM SIMD floating point");

extern "x86-interrupt" fn debug(frame: InterruptStackFrame) {
    klog!("trap", "#DB debug at {:#x}", frame.instruction_pointer.as_u64());
}

extern "x86-interrupt" fn nmi(frame: InterruptStackFrame) {
    klog!("trap", "NMI at {:#x}", frame.instruction_pointer.as_u64());
}

extern "x86-interrupt" fn breakpoint(frame: InterruptStackFrame) {
    klog!("trap", "#BP breakpoint at {:#x}", frame.instruction_pointer.as_u64());
}

extern "x86-interrupt" fn double_fault(frame: InterruptStackFrame, code: u64) -> ! {
    kernel_panic("#DF double fault", &frame, Some(code));
}

extern "x86-interrupt" fn machine_check(frame: InterruptStackFrame) -> ! {
    kernel_panic("#MC machine check", &frame, None);
}

extern "x86-interrupt" fn page_fault(frame: InterruptStackFrame, code: PageFaultErrorCode) {
    let addr = Cr2::read_raw();
    if from_user(&frame) {
        crate::sched::on_user_page_fault(addr, code.bits(), frame.instruction_pointer.as_u64());
    }
    println!();
    println!("!! #PF page fault: addr={:#x} {:?}", addr, code);
    kernel_panic("#PF page fault", &frame, Some(code.bits()));
}

fn kernel_panic(what: &str, frame: &InterruptStackFrame, code: Option<u64>) -> ! {
    println!();
    println!("!! KERNEL TRAP: {}", what);
    if let Some(c) = code {
        println!("   error code = {:#x}", c);
    }
    println!(
        "   rip={:#018x} cs={:#x} rflags={:#x}",
        frame.instruction_pointer.as_u64(),
        frame.code_segment.0,
        frame.cpu_flags.bits()
    );
    println!(
        "   rsp={:#018x} ss={:#x}",
        frame.stack_pointer.as_u64(),
        frame.stack_segment.0
    );
    panic!("unrecoverable CPU exception in kernel mode");
}
