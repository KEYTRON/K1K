//! Local APIC (timer, EOI) and I/O APIC (IRQ routing).

use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use x86_64::PhysAddr;

use super::{acpi, pit};
use crate::klog;
use crate::mm::{pmm, vmm};

const LAPIC_ID: u32 = 0x020;
const LAPIC_EOI: u32 = 0x0B0;
const LAPIC_SVR: u32 = 0x0F0;
const LAPIC_LVT_TIMER: u32 = 0x320;
const LAPIC_TIMER_INIT: u32 = 0x380;
const LAPIC_TIMER_CUR: u32 = 0x390;
const LAPIC_TIMER_DIV: u32 = 0x3E0;
const LAPIC_TPR: u32 = 0x080;
/// Interrupt command register: bits 0-7 vector, 8-10 delivery mode, 11
/// destination mode (0 = physical), 12 delivery status, 24-31 the low 8 bits of
/// the destination APIC id.
const LAPIC_ICR: u32 = 0x300;
/// ICR bit 12: a delivery is in progress. The APIC holds one command at a
/// time, so a busy bit means the previous IPI has not been taken yet — which,
/// on a target that is inside a critical section, can be for a while.
const ICR_BUSY: u32 = 1 << 12;
/// LVT entries start here; the timer at vector 0x20 lands at 0x220 + 0x100.
const LAPIC_LVT_BASE: u32 = 0x220;
/// LVT delivery mode "fixed", unmasked.
const LVT_FIXED: u32 = 0x400;

const TIMER_PERIODIC: u32 = 1 << 17;
const LVT_MASKED: u32 = 1 << 16;
const DIV_16: u32 = 0b0011;

pub const SPURIOUS_VECTOR: u8 = 0xFF;

static LAPIC_BASE: AtomicU64 = AtomicU64::new(0);
static BSP_LAPIC_ID: AtomicU8 = AtomicU8::new(0);
static TICKS_PER_MS: AtomicU32 = AtomicU32::new(0);

#[inline]
fn lapic_read(reg: u32) -> u32 {
    let base = LAPIC_BASE.load(Ordering::Relaxed);
    unsafe { ((base + reg as u64) as *const u32).read_volatile() }
}

#[inline]
fn lapic_write(reg: u32, v: u32) {
    let base = LAPIC_BASE.load(Ordering::Relaxed);
    unsafe { ((base + reg as u64) as *mut u32).write_volatile(v) }
}

pub fn lapic_id() -> u8 {
    (lapic_read(LAPIC_ID) >> 24) as u8
}

/// Broadcast `vector` to every other processor, reporting whether the APIC
/// accepted it.
///
/// Delivery is logical with the "all processors except self" shorthand, which
/// is the form that works on real hardware and on QEMU alike; a directed
/// physical IPI is accepted by QEMU's local APIC and then quietly dropped for
/// anything but the sending CPU, so relying on it would leave every shootdown
/// looking like it had worked. Each target reports back through its own
/// per-CPU slot, so the sender still learns exactly who did the work.
///
/// No "wait for delivery" bit: a target with interrupts disabled holds the
/// delivery status bit set for as long as it stays disabled, so waiting on it
/// would block the sender for reasons that have nothing to do with the
/// interrupt. Waiting for the acknowledgement is the real synchronisation.
pub fn send_broadcast_ipi(vector: u8) -> bool {
    if lapic_read(LAPIC_ICR) & ICR_BUSY != 0 {
        return false;
    }
    const LOGICAL: u32 = 1 << 11;
    const ALL_BUT_SELF: u32 = 2 << 18;
    lapic_write(LAPIC_ICR, LOGICAL | ALL_BUT_SELF | vector as u32);
    true
}

pub fn eoi() {
    lapic_write(LAPIC_EOI, 0);
}

fn mmio(phys: u64, len: u64) -> u64 {
    vmm::map_mmio(PhysAddr::new(phys), len);
    pmm::phys_to_virt(PhysAddr::new(phys)).as_u64()
}

/// `IA32_APIC_BASE`: bit 8 "is bootstrap processor", bit 10 "APIC global
/// enable". The MP startup procedure has every application processor enable
/// its own local APIC here; a processor whose APIC is not enabled receives no
/// interrupts at all, IPIs included.
const IA32_APIC_BASE: u32 = 0x1B;
const APIC_BASE_ENABLE: u64 = 1 << 10;
const APIC_BASE_BSP: u64 = 1 << 8;

/// Software-enable the calling CPU's local APIC with the timer masked.
///
/// The IPI vector has to be unmasked explicitly: an LVT entry that was never
/// written is masked, and a masked vector drops the interrupt instead of
/// delivering it — which would make every TLB shootdown silently do nothing.
pub fn enable_local() {
    let _ = (APIC_BASE_ENABLE, APIC_BASE_BSP, IA32_APIC_BASE);
    lapic_write(LAPIC_TPR, 0);
    lapic_write(LAPIC_SVR, 0x100 | SPURIOUS_VECTOR as u32);
    lapic_write(LAPIC_LVT_TIMER, LVT_MASKED);
    let ipi_vector = super::ipi::IPI_VECTOR as u32;
    lapic_write(LAPIC_LVT_BASE + 8 * ipi_vector, LVT_FIXED | ipi_vector);
}

/// Map the BSP's local APIC and calibrate its timer against the PIT.
pub fn init_lapic() {
    let phys = acpi::madt().lapic_address;
    LAPIC_BASE.store(mmio(phys, 0x1000), Ordering::Relaxed);
    BSP_LAPIC_ID.store(lapic_id(), Ordering::Relaxed);
    enable_local();

    lapic_write(LAPIC_TIMER_DIV, DIV_16);
    lapic_write(LAPIC_TIMER_INIT, u32::MAX);
    pit::busy_wait_ms(20);
    let elapsed = u32::MAX - lapic_read(LAPIC_TIMER_CUR);
    lapic_write(LAPIC_TIMER_INIT, 0);
    let per_ms = (elapsed / 20).max(1);
    TICKS_PER_MS.store(per_ms, Ordering::Relaxed);

    klog!(
        "apic",
        "lapic id {} at {:#x}, timer {} ticks/ms (div 16)",
        lapic_id(),
        phys,
        per_ms
    );
}

/// Start the periodic timer interrupt on `vector` at `hz`.
pub fn start_timer(vector: u8, hz: u32) {
    let per_ms = TICKS_PER_MS.load(Ordering::Relaxed);
    let count = (per_ms as u64 * 1000 / hz as u64).max(1) as u32;
    lapic_write(LAPIC_TIMER_DIV, DIV_16);
    lapic_write(LAPIC_LVT_TIMER, TIMER_PERIODIC | vector as u32);
    lapic_write(LAPIC_TIMER_INIT, count);
}

struct IoApic {
    base: u64,
    gsi_base: u32,
    entries: u32,
}

impl IoApic {
    fn read(&self, reg: u32) -> u32 {
        unsafe {
            (self.base as *mut u32).write_volatile(reg);
            ((self.base + 0x10) as *const u32).read_volatile()
        }
    }
    fn write(&self, reg: u32, v: u32) {
        unsafe {
            (self.base as *mut u32).write_volatile(reg);
            ((self.base + 0x10) as *mut u32).write_volatile(v);
        }
    }
    fn set_redirect(&self, index: u32, low: u32, high: u32) {
        self.write(0x10 + index * 2 + 1, high);
        self.write(0x10 + index * 2, low);
    }
}

static IOAPICS: spin::Once<alloc::vec::Vec<IoApic>> = spin::Once::new();

pub fn init_ioapic() {
    let madt = acpi::madt();
    let mut list = alloc::vec::Vec::new();
    for info in &madt.ioapics {
        let base = mmio(info.address as u64, 0x1000);
        let mut io = IoApic {
            base,
            gsi_base: info.gsi_base,
            entries: 0,
        };
        io.entries = ((io.read(1) >> 16) & 0xFF) + 1;
        for i in 0..io.entries {
            io.set_redirect(i, LVT_MASKED, 0);
        }
        klog!(
            "apic",
            "ioapic {} at {:#x}: gsi {}..{}",
            info.id,
            info.address,
            info.gsi_base,
            info.gsi_base + io.entries
        );
        list.push(io);
    }
    IOAPICS.call_once(|| list);
}

/// Route legacy ISA `irq` to `vector` on the BSP, honouring MADT overrides.
#[derive(Debug, Clone, Copy)]
pub struct IsaRoute {
    pub gsi: u32,
    pub level: bool,
}

fn ioapic_for(gsi: u32) -> &'static IoApic {
    IOAPICS
        .get()
        .expect("ioapic not initialised")
        .iter()
        .find(|io| gsi >= io.gsi_base && gsi < io.gsi_base + io.entries)
        .expect("no ioapic covers gsi")
}

pub fn route_isa_irq(irq: u8, vector: u8) -> IsaRoute {
    let madt = acpi::madt();
    let ovr = madt.overrides.iter().find(|o| o.isa_irq == irq);
    let gsi = ovr.map(|o| o.gsi).unwrap_or(irq as u32);
    let level = ovr.is_some_and(|o| o.level_triggered);
    let mut low = vector as u32;
    if ovr.is_some_and(|o| o.active_low) {
        low |= 1 << 13;
    }
    if level {
        low |= 1 << 15;
    }
    let dest = (bsp_lapic_id() as u32) << 24;
    let io = ioapic_for(gsi);
    io.set_redirect(gsi - io.gsi_base, low, dest);
    klog!(
        "apic",
        "irq {} -> gsi {} -> vector {}{}",
        irq,
        gsi,
        vector,
        if level { " (level)" } else { "" }
    );
    IsaRoute { gsi, level }
}

/// Mask or unmask one I/O APIC input without touching its routing.
pub fn set_gsi_mask(gsi: u32, masked: bool) {
    let io = ioapic_for(gsi);
    let reg = 0x10 + (gsi - io.gsi_base) * 2;
    let low = io.read(reg);
    io.write(
        reg,
        if masked {
            low | LVT_MASKED
        } else {
            low & !LVT_MASKED
        },
    );
}

pub fn bsp_lapic_id() -> u8 {
    BSP_LAPIC_ID.load(Ordering::Relaxed)
}
