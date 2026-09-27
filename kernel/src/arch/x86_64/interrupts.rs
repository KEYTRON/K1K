use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::registers::control::{Cr4, Cr4Flags};

use super::{acpi, apic, clock, idt, percpu, pic};
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
    // Before the tick starts: the PIT the TSC is measured against is busy-waited
    // on, and a tick in the middle of that would only add noise.
    clock::init();
    init_local();
    klog!("irq", "lapic timer @ {} Hz", TIMER_HZ);
}

/// Per-CPU part: harden CR4 and start this CPU's timer.
pub fn init_local() {
    unsafe { Cr4::update(|f| f.remove(Cr4Flags::FSGSBASE)) };
    // So that a TSC reading says which CPU took it.
    clock::set_tsc_aux(percpu::cpu_id());
    apic::enable_local();
    apic::start_timer(idt::IRQ_TIMER, TIMER_HZ);
}

pub fn enable() {
    x86_64::instructions::interrupts::enable();
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Milliseconds since boot, from the best clock this machine has.
///
/// The tick counter is the fallback, not the answer: it only advances on the
/// boot processor and it advances in whole interrupts, which is the wrong
/// resolution for anything measuring a delay.
#[inline]
pub fn uptime_ms() -> u64 {
    clock::now_ns() / 1_000_000
}

/// Nanoseconds since boot. See [`clock::now_ns`].
#[inline]
pub fn uptime_ns() -> u64 {
    clock::now_ns()
}

/// Timer vector: only the BSP advances wall time, every CPU schedules.
pub fn on_timer() {
    if percpu::cpu_id() == 0 {
        TICKS.fetch_add(1, Ordering::Relaxed);
    }
    // Every CPU samples the TSC against the same global tick; the boot processor
    // also keeps the tick-based clock and the HPET comparison up to date.
    clock::on_tick();
    apic::eoi();
    crate::sched::on_tick();
}
