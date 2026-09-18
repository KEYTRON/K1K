use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::structures::idt::InterruptStackFrame;

use super::{idt, pic, pit};

pub const TIMER_HZ: u32 = 200;

pub static TICKS: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    pic::init(idt::IRQ_BASE);
    pit::init(TIMER_HZ);
    pic::unmask(0);
    pic::unmask(1);
}

pub fn enable() {
    x86_64::instructions::interrupts::enable();
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

pub fn uptime_ms() -> u64 {
    ticks() * 1000 / TIMER_HZ as u64
}

pub extern "x86-interrupt" fn timer_irq(_frame: InterruptStackFrame) {
    TICKS.fetch_add(1, Ordering::Relaxed);
    pic::eoi(0);
    crate::sched::on_tick();
}

pub extern "x86-interrupt" fn keyboard_irq(_frame: InterruptStackFrame) {
    let scancode: u8 = unsafe { x86_64::instructions::port::Port::new(0x60).read() };
    crate::sched::on_keyboard(scancode);
    pic::eoi(1);
}

pub extern "x86-interrupt" fn spurious_irq(_frame: InterruptStackFrame) {
    pic::eoi(7);
}
