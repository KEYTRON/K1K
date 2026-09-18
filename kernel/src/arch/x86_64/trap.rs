//! Common interrupt/exception entry. The asm stubs in `trap_stubs.s` push a
//! uniform frame and call `trap_dispatch`; this file decides what a vector
//! means: CPU exception (kill the task or panic), device IRQ, or spurious.

use core::arch::global_asm;
use x86_64::registers::control::Cr2;

use super::{apic, idt, interrupts, percpu};
use crate::{klog, println};

global_asm!(include_str!("trap_stubs.s"));

unsafe extern "C" {
    pub static trap_stub_table: [u64; 256];
}

/// Register file as saved by `trap_common` (lowest address first).
#[repr(C)]
#[derive(Debug)]
pub struct TrapFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub vector: u64,
    pub error_code: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

impl TrapFrame {
    pub fn from_user(&self) -> bool {
        self.cs & 3 == 3
    }
}

const EXCEPTION_NAMES: [&str; 32] = [
    "#DE divide error",
    "#DB debug",
    "NMI",
    "#BP breakpoint",
    "#OF overflow",
    "#BR bound range exceeded",
    "#UD invalid opcode",
    "#NM device not available",
    "#DF double fault",
    "coprocessor segment overrun",
    "#TS invalid TSS",
    "#NP segment not present",
    "#SS stack segment fault",
    "#GP general protection fault",
    "#PF page fault",
    "reserved",
    "#MF x87 floating point",
    "#AC alignment check",
    "#MC machine check",
    "#XM SIMD floating point",
    "#VE virtualization",
    "#CP control protection",
    "reserved",
    "reserved",
    "reserved",
    "reserved",
    "reserved",
    "reserved",
    "#HV hypervisor injection",
    "#VC VMM communication",
    "#SX security",
    "reserved",
];

#[unsafe(no_mangle)]
extern "C" fn trap_dispatch(frame: &mut TrapFrame) {
    let vector = frame.vector as u8;
    match vector {
        0..=31 => exception(frame),
        idt::IRQ_TIMER => interrupts::on_timer(),
        apic::SPURIOUS_VECTOR => {}
        _ => super::irq::on_vector(vector),
    }
}

fn exception(frame: &mut TrapFrame) {
    let vector = frame.vector as u8;
    let name = EXCEPTION_NAMES[vector as usize];
    match vector {
        1 | 2 | 3 => {
            klog!(
                "trap",
                "{} at {:#x} (cpu {})",
                name,
                frame.rip,
                percpu::cpu_id()
            );
            return;
        }
        14 => {
            let addr = Cr2::read_raw();
            if frame.from_user() {
                crate::sched::on_user_page_fault(addr, frame.error_code, frame.rip);
            }
            println!();
            println!(
                "!! #PF page fault: addr={:#x} error={:#x} cpu={}",
                addr,
                frame.error_code,
                percpu::cpu_id()
            );
            kernel_panic(name, frame);
        }
        _ => {
            if frame.from_user() {
                crate::sched::on_user_fault(name, frame.error_code, frame.rip);
            }
            kernel_panic(name, frame);
        }
    }
}

fn kernel_panic(what: &str, frame: &TrapFrame) -> ! {
    println!();
    println!("!! KERNEL TRAP on cpu {}: {}", percpu::cpu_id(), what);
    println!("   error code = {:#x}", frame.error_code);
    println!(
        "   rip={:#018x} cs={:#x} rflags={:#x}",
        frame.rip, frame.cs, frame.rflags
    );
    println!("   rsp={:#018x} ss={:#x}", frame.rsp, frame.ss);
    println!(
        "   rax={:#018x} rbx={:#018x} rcx={:#018x} rdx={:#018x}",
        frame.rax, frame.rbx, frame.rcx, frame.rdx
    );
    println!(
        "   rsi={:#018x} rdi={:#018x} rbp={:#018x}",
        frame.rsi, frame.rdi, frame.rbp
    );
    panic!("unrecoverable CPU exception in kernel mode");
}
