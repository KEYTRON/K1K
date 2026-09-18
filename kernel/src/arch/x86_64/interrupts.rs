use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::registers::control::{Cr4, Cr4Flags};

use super::{acpi, apic, idt, percpu, pic};
use crate::klog;

pub const TIMER_HZ: u32 = 1000;

pub static TICKS: AtomicU64 = AtomicU64::new(0);

/// Bootstrap processor: discover the interrupt hardware and start the timer.
/// Device interrupts are routed later, when a driver asks for them (`irq.rs`).
pub fn init() {
    let rsdp = crate::boot::BOOT_RSDP
        .response()
        .expect("limine: no RSDP")
        .address as u64;
    acpi::init(rsdp);
    pic::disable();
    apic::init_lapic();
    apic::init_ioapic();
    init_local();
    klog!("irq", "lapic timer @ {} Hz", TIMER_HZ);
}

/// Per-CPU part: harden CR4 and start this CPU's timer.
pub fn init_local() {
    unsafe { Cr4::update(|f| f.remove(Cr4Flags::FSGSBASE)) };
    apic::enable_local();
    apic::start_timer(idt::IRQ_TIMER, TIMER_HZ);
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

/// Timer vector: only the BSP advances wall time, every CPU schedules.
pub fn on_timer() {
    if percpu::cpu_id() == 0 {
        TICKS.fetch_add(1, Ordering::Relaxed);
    }
    apic::eoi();
    crate::sched::on_tick();
}
