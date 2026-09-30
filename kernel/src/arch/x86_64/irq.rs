//! Interrupt objects: a device interrupt becomes a message on an endpoint or a
//! signal on a notification, whichever the ring-3 driver bound. The kernel only
//! routes and acknowledges at the APIC; what the interrupt *means* is the
//! driver's business.
//!
//! The notification is the better of the two for a driver that only needs to
//! know that *something* happened: it allocates nothing, and a signal that
//! arrives before the driver is listening is still there when it comes back.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use spin::Mutex;
use x86_64::instructions::interrupts;

use super::{apic, pci};
use crate::ipc::{Endpoint, Message};
use crate::klog;
use crate::notify::Notify;
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
    notify: Mutex<Option<Arc<Notify>>>,
    pub count: AtomicU64,
    pub dropped: AtomicU64,
}

impl IrqObject {
    /// Deliver interrupts as messages. One message each, so a full queue loses
    /// events and [`IrqObject::dropped`] counts them.
    pub fn bind(&self, ep: Arc<Endpoint>) {
        interrupts::without_interrupts(|| {
            *self.notify.lock() = None;
            *self.endpoint.lock() = Some(ep);
        });
    }

    /// Deliver interrupts as signals. Nothing is allocated and nothing is lost:
    /// the count only saturates if nobody waits for four billion of them.
    pub fn bind_notify(&self, notify: Arc<Notify>) {
        interrupts::without_interrupts(|| {
            *self.endpoint.lock() = None;
            *self.notify.lock() = Some(notify);
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
        notify: Mutex::new(None),
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
    let target = apic::bsp_lapic_id();
    if target > 0xFF {
        klog!(
            "irq",
            "WARNING: this processor is lapic {target}, and MSI-X only names 255 of them; \
             the device would deliver to processor 0 instead"
        );
    }
    pci::msix_enable(dev, 0, vector, target)?;
    let obj = Arc::new(IrqObject {
        vector,
        gsi: None,
        level: false,
        endpoint: Mutex::new(None),
        notify: Mutex::new(None),
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
    // The guard is dropped before the delivery: signalling wakes a task, which
    // takes the scheduler's lock, and there is no reason to hold this one.
    let notify = obj.notify.lock().clone();
    let ep = obj.endpoint.lock().clone();
    match (notify, ep) {
        (Some(notify), _) => {
            notify.signal(1);
        }
        (None, Some(ep)) => {
            if ep
                .send(Message::new(0, [vector as u64, n, 0, NO_CAP]))
                .is_err()
            {
                obj.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        (None, None) => {
            obj.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    apic::eoi();
}
