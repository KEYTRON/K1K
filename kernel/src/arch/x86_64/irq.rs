//! Interrupt objects: a device interrupt becomes a message on an endpoint a
//! ring-3 driver chose. The kernel only routes and acknowledges at the APIC;
//! what the interrupt *means* is the driver's business.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use spin::Mutex;
use x86_64::instructions::interrupts;

use super::{apic, pci};
use crate::ipc::{Endpoint, Message};
use crate::klog;
use crate::syscall::NO_CAP;

/// Vector 32 is the scheduler timer; ISA IRQs 0..15 map to 34..49; MSI/MSI-X
/// vectors are handed out from 64 upward.
pub const ISA_VECTOR_BASE: u8 = 34;
const MSI_VECTOR_FIRST: u8 = 64;
const MSI_VECTOR_LAST: u8 = 127;

pub struct IrqObject {
    pub vector: u8,
    /// IOAPIC input for level-triggered lines (masked until `ack`).
    gsi: Option<u32>,
    level: bool,
    endpoint: Mutex<Option<Arc<Endpoint>>>,
    pub count: AtomicU64,
    pub dropped: AtomicU64,
}

impl IrqObject {
    pub fn bind(&self, ep: Arc<Endpoint>) {
        interrupts::without_interrupts(|| {
            *self.endpoint.lock() = Some(ep);
        });
    }

    /// Re-enable a level-triggered line after the driver serviced the device.
    pub fn ack(&self) {
        if let Some(gsi) = self.gsi
            && self.level
        {
            apic::set_gsi_mask(gsi, false);
        }
    }
}

static TABLE: Mutex<[Option<Arc<IrqObject>>; 256]> = Mutex::new([const { None }; 256]);
static NEXT_MSI: AtomicU8 = AtomicU8::new(MSI_VECTOR_FIRST);

fn install(obj: Arc<IrqObject>) {
    let vector = obj.vector as usize;
    interrupts::without_interrupts(|| {
        TABLE.lock()[vector] = Some(obj);
    });
}

/// Interrupt object for a legacy ISA line, routed through the I/O APIC.
pub fn isa(irq: u8) -> Arc<IrqObject> {
    let vector = ISA_VECTOR_BASE + irq;
    if let Some(existing) = interrupts::without_interrupts(|| TABLE.lock()[vector as usize].clone())
    {
        return existing;
    }
    let route = apic::route_isa_irq(irq, vector);
    let obj = Arc::new(IrqObject {
        vector,
        gsi: Some(route.gsi),
        level: route.level,
        endpoint: Mutex::new(None),
        count: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
    });
    install(obj.clone());
    obj
}

/// Interrupt object for a PCI function, delivered through MSI-X entry 0.
pub fn msix(dev: &pci::PciDevice) -> Option<Arc<IrqObject>> {
    let vector = NEXT_MSI.fetch_add(1, Ordering::AcqRel);
    if vector > MSI_VECTOR_LAST {
        return None;
    }
    pci::msix_enable(dev, 0, vector, apic::bsp_lapic_id())?;
    let obj = Arc::new(IrqObject {
        vector,
        gsi: None,
        level: false,
        endpoint: Mutex::new(None),
        count: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
    });
    install(obj.clone());
    klog!(
        "irq",
        "{:02x}:{:02x}.{} msi-x entry 0 -> vector {}",
        dev.bus,
        dev.slot,
        dev.func,
        vector
    );
    Some(obj)
}

/// Called from the trap path for every vector above the timer.
pub fn on_vector(vector: u8) {
    let obj = TABLE.lock()[vector as usize].clone();
    let Some(obj) = obj else {
        apic::eoi();
        return;
    };
    if obj.level
        && let Some(gsi) = obj.gsi
    {
        apic::set_gsi_mask(gsi, true);
    }
    let n = obj.count.fetch_add(1, Ordering::Relaxed) + 1;
    let ep = obj.endpoint.lock().clone();
    match ep {
        Some(ep) => {
            if ep
                .send(Message::new(0, [vector as u64, n, 0, NO_CAP]))
                .is_err()
            {
                obj.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        None => {
            obj.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    apic::eoi();
}
