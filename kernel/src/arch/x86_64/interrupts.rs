use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::structures::idt::InterruptStackFrame;

use super::{acpi, apic, idt, pic};
use crate::klog;

pub const TIMER_HZ: u32 = 1000;

pub static TICKS: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    let rsdp = crate::boot::BOOT_RSDP
        .response()
        .expect("limine: no RSDP")
        .address as u64;
    acpi::init(rsdp);
    pic::disable();
    apic::init_lapic();
    apic::init_ioapic();
    apic::route_isa_irq(1, idt::IRQ_KEYBOARD);
    apic::start_timer(idt::IRQ_TIMER, TIMER_HZ);
    klog!("irq", "lapic timer @ {} Hz, keyboard via ioapic", TIMER_HZ);
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
    apic::eoi();
    crate::sched::on_tick();
}

pub extern "x86-interrupt" fn keyboard_irq(_frame: InterruptStackFrame) {
    let scancode: u8 = unsafe { x86_64::instructions::port::Port::new(0x60).read() };
    apic::eoi();
    crate::sched::on_keyboard(scancode);
}

pub extern "x86-interrupt" fn unexpected_irq(_frame: InterruptStackFrame) {
    apic::eoi();
}

pub extern "x86-interrupt" fn spurious_irq(_frame: InterruptStackFrame) {}
