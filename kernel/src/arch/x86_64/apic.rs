//! Local APIC (timer, EOI) and I/O APIC (IRQ routing).

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
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

const TIMER_PERIODIC: u32 = 1 << 17;
const LVT_MASKED: u32 = 1 << 16;
const DIV_16: u32 = 0b0011;

pub const SPURIOUS_VECTOR: u8 = 0xFF;

static LAPIC_BASE: AtomicU64 = AtomicU64::new(0);
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

pub fn eoi() {
    lapic_write(LAPIC_EOI, 0);
}

fn mmio(phys: u64, len: u64) -> u64 {
    vmm::map_mmio(PhysAddr::new(phys), len);
    pmm::phys_to_virt(PhysAddr::new(phys)).as_u64()
}

/// Enable the BSP's local APIC and calibrate its timer against the PIT.
pub fn init_lapic() {
    let phys = acpi::madt().lapic_address;
    LAPIC_BASE.store(mmio(phys, 0x1000), Ordering::Relaxed);

    lapic_write(LAPIC_TPR, 0);
    lapic_write(LAPIC_SVR, 0x100 | SPURIOUS_VECTOR as u32);
    lapic_write(LAPIC_LVT_TIMER, LVT_MASKED);

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
pub fn route_isa_irq(irq: u8, vector: u8) {
    let madt = acpi::madt();
    let ovr = madt.overrides.iter().find(|o| o.isa_irq == irq);
    let gsi = ovr.map(|o| o.gsi).unwrap_or(irq as u32);
    let mut low = vector as u32;
    if ovr.is_some_and(|o| o.active_low) {
        low |= 1 << 13;
    }
    if ovr.is_some_and(|o| o.level_triggered) {
        low |= 1 << 15;
    }
    let dest = (lapic_id() as u32) << 24;

    let ioapics = IOAPICS.get().expect("ioapic not initialised");
    let io = ioapics
        .iter()
        .find(|io| gsi >= io.gsi_base && gsi < io.gsi_base + io.entries)
        .expect("no ioapic covers gsi");
    io.set_redirect(gsi - io.gsi_base, low, dest);
    klog!("apic", "irq {} -> gsi {} -> vector {}", irq, gsi, vector);
}
