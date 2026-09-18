//! Scheduler and task supervision (milestone 3/4). Hooks below are called
//! from the trap and IRQ paths.

use crate::klog;

pub fn on_tick() {}

pub fn on_keyboard(_scancode: u8) {}

pub fn on_user_fault(what: &str, code: u64, rip: u64) -> ! {
    klog!("sched", "user fault {} code={:#x} rip={:#x}", what, code, rip);
    crate::arch::x86_64::halt_loop()
}

pub fn on_user_page_fault(addr: u64, code: u64, rip: u64) -> ! {
    klog!("sched", "user page fault addr={:#x} code={:#x} rip={:#x}", addr, code, rip);
    crate::arch::x86_64::halt_loop()
}
