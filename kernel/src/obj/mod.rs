//! Kernel objects and capabilities.
//!
//! A capability is an unforgeable reference to a kernel object plus a rights
//! mask. User tasks never see object pointers — only slot indices into their
//! own `CapTable`, checked on every syscall. This is the whole access-control
//! model: no UIDs, no ambient authority. Capabilities move between tasks only
//! by being sent over an endpoint, and only if the sender holds `GRANT`.

use alloc::sync::Arc;
use alloc::vec::Vec;
use x86_64::structures::paging::PhysFrame;

use crate::arch::x86_64::irq::IrqObject;
use crate::arch::x86_64::pci::PciDevice;
use crate::ipc::Endpoint;
use crate::mm::pmm;
use crate::notify::Notify;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rights(pub u32);

impl Rights {
    pub const SEND: Rights = Rights(1 << 0);
    pub const RECV: Rights = Rights(1 << 1);
    pub const GRANT: Rights = Rights(1 << 2);
    pub const MAP_READ: Rights = Rights(1 << 3);
    pub const MAP_WRITE: Rights = Rights(1 << 4);
    /// May learn the physical address of a memory object (for DMA).
    pub const DMA: Rights = Rights(1 << 5);
    /// May create new services from ELF images (on a `Control` capability).
    pub const SPAWN: Rights = Rights(1 << 6);
    /// May record a signal on a `Notify`. Separate from `WAIT` so one half of a
    /// relationship can be handed out without the other.
    pub const SIGNAL: Rights = Rights(1 << 7);
    /// May take signals from a `Notify`.
    pub const WAIT: Rights = Rights(1 << 8);

    pub const fn contains(self, other: Rights) -> bool {
        self.0 & other.0 == other.0
    }
    pub const fn intersect(self, other: Rights) -> Rights {
        Rights(self.0 & other.0)
    }
    pub const fn union(self, other: Rights) -> Rights {
        Rights(self.0 | other.0)
    }
}

/// A set of physical frames that can be mapped into address spaces. Frames
/// are returned to the PMM when the last capability and mapping are gone.
pub struct MemoryObject {
    frames: Vec<PhysFrame>,
    contiguous: bool,
}

impl MemoryObject {
    pub fn new(pages: usize) -> Option<Arc<Self>> {
        let mut frames = Vec::with_capacity(pages);
        for _ in 0..pages {
            match pmm::alloc_zeroed_frame() {
                Some(f) => frames.push(f),
                None => {
                    for f in frames {
                        pmm::free_frame(f);
                    }
                    return None;
                }
            }
        }
        Some(Arc::new(Self {
            frames,
            contiguous: false,
        }))
    }

    /// Physically contiguous, zeroed — what a DMA engine wants.
    pub fn new_contiguous(pages: usize) -> Option<Arc<Self>> {
        let first = pmm::alloc_contiguous_zeroed(pages)?;
        let frames = (0..pages as u64).map(|i| first + i).collect();
        Some(Arc::new(Self {
            frames,
            contiguous: true,
        }))
    }

    pub fn frames(&self) -> &[PhysFrame] {
        &self.frames
    }

    pub fn pages(&self) -> usize {
        self.frames.len()
    }

    pub fn is_contiguous(&self) -> bool {
        self.contiguous
    }
}

impl Drop for MemoryObject {
    fn drop(&mut self) {
        for f in self.frames.drain(..) {
            pmm::free_frame(f);
        }
    }
}

/// A PCI function handed to a ring-3 driver: its BARs may be mapped.
pub struct DeviceObject {
    pub pci: PciDevice,
}

/// A range of x86 I/O ports a driver may touch.
#[derive(Clone, Copy, Debug)]
pub struct PortRange {
    pub base: u16,
    pub len: u16,
}

#[derive(Clone)]
pub enum Object {
    Endpoint(Arc<Endpoint>),
    /// A counter of things that happened, with tasks waiting for the next one.
    Notify(Arc<Notify>),
    Memory(Arc<MemoryObject>),
    Device(Arc<DeviceObject>),
    Irq(Arc<IrqObject>),
    Port(PortRange),
    /// Kernel control authority (spawning services); held by init-like tasks.
    Control,
}

#[derive(Clone)]
pub struct Capability {
    pub object: Object,
    pub rights: Rights,
}

impl Capability {
    pub fn endpoint(&self) -> Option<&Arc<Endpoint>> {
        match &self.object {
            Object::Endpoint(e) => Some(e),
            _ => None,
        }
    }
    pub fn notify(&self) -> Option<&Arc<Notify>> {
        match &self.object {
            Object::Notify(n) => Some(n),
            _ => None,
        }
    }
    pub fn memory(&self) -> Option<&Arc<MemoryObject>> {
        match &self.object {
            Object::Memory(m) => Some(m),
            _ => None,
        }
    }
    pub fn device(&self) -> Option<&Arc<DeviceObject>> {
        match &self.object {
            Object::Device(d) => Some(d),
            _ => None,
        }
    }
    pub fn irq(&self) -> Option<&Arc<IrqObject>> {
        match &self.object {
            Object::Irq(i) => Some(i),
            _ => None,
        }
    }
    pub fn port(&self) -> Option<PortRange> {
        match &self.object {
            Object::Port(p) => Some(*p),
            _ => None,
        }
    }
    pub fn is_control(&self) -> bool {
        matches!(self.object, Object::Control)
    }
}

/// A task's capabilities.
///
/// A revoked slot stays revoked. The capability is gone — the object may live on
/// in somebody else's table — but the *slot* is not handed out again, because a
/// program that kept the old slot number would otherwise find its next call
/// quietly reaching a different object. A tombstone is the honest answer: the
/// authority is gone for good, and the number that named it is dead.
pub struct CapTable {
    slots: Vec<Option<Capability>>,
    revoked: Vec<bool>,
}

pub type CapSlot = u32;

const MAX_SLOTS: usize = 256;

impl CapTable {
    pub const fn new() -> Self {
        Self {
            slots: Vec::new(),
            revoked: Vec::new(),
        }
    }

    /// The first slot that is free and has never been revoked.
    fn first_usable(&self) -> Option<usize> {
        (0..self.slots.len()).find(|i| self.slots[*i].is_none() && !self.revoked[*i])
    }

    pub fn insert(&mut self, cap: Capability) -> Option<CapSlot> {
        if let Some(i) = self.first_usable() {
            self.slots[i] = Some(cap);
            return Some(i as CapSlot);
        }
        if self.slots.len() >= MAX_SLOTS {
            return None;
        }
        self.slots.push(Some(cap));
        self.revoked.push(false);
        Some((self.slots.len() - 1) as CapSlot)
    }

    pub fn get(&self, slot: CapSlot) -> Option<&Capability> {
        self.slots.get(slot as usize)?.as_ref()
    }

    /// Look up a slot and check that it carries `rights`.
    pub fn lookup(&self, slot: CapSlot, rights: Rights) -> Option<&Capability> {
        let cap = self.get(slot)?;
        cap.rights.contains(rights).then_some(cap)
    }

    /// Copy a capability for another table with (possibly reduced) rights.
    /// Requires `GRANT`; the copy never carries more than the original.
    pub fn derive(&self, slot: CapSlot, mask: Rights) -> Option<Capability> {
        let cap = self.lookup(slot, Rights::GRANT)?;
        Some(Capability {
            object: cap.object.clone(),
            rights: cap.rights.intersect(mask),
        })
    }

    /// Whether any slot in the table carries `Control` authority.
    pub fn has_control(&self) -> bool {
        self.iter().any(|c| c.is_control())
    }

    /// Every live capability in the table, for iterating over it.
    pub fn iter(&self) -> impl Iterator<Item = &Capability> {
        self.slots.iter().filter_map(Option::as_ref)
    }

    /// Give a slot up for good. Returns what was in it, or `None` if the slot
    /// was already empty or already revoked.
    pub fn revoke(&mut self, slot: CapSlot) -> Option<Capability> {
        let i = slot as usize;
        if self.revoked.get(i).copied().unwrap_or(false) {
            return None;
        }
        let cap = self.slots.get_mut(i)?.take()?;
        if i < self.revoked.len() {
            self.revoked[i] = true;
        }
        Some(cap)
    }

    /// The slot number of the `n`th capability in the table, counting from zero.
    /// The spawner uses it to tell a grantor where its grants landed.
    pub fn slot_at(&self, n: usize) -> Option<CapSlot> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_some())
            .nth(n)
            .map(|(i, _)| i as CapSlot)
    }

    /// How many slots the table has ever used, live or dead.
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// Slots that hold a capability.
    pub fn live_slots(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    /// Slots that have been revoked and are dead for good.
    pub fn revoked_slots(&self) -> usize {
        self.revoked.iter().filter(|r| **r).count()
    }

    /// Slots that are free and have never been revoked, plus the ones left over
    /// under the limit.
    pub fn free_slots(&self) -> usize {
        let used = self.slots.len();
        let taken = self
            .slots
            .iter()
            .zip(&self.revoked)
            .filter(|(s, r)| s.is_none() && !**r)
            .count();
        (MAX_SLOTS - used).saturating_add(taken)
    }

    /// Give a slot up without revoking it: the number can be reused. This is
    /// what `cap_drop` does, and the difference from [`CapTable::revoke`] is the
    /// whole point of having both.
    pub fn remove(&mut self, slot: CapSlot) -> Option<Capability> {
        self.slots.get_mut(slot as usize)?.take()
    }
}
