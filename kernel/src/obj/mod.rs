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

use crate::ipc::Endpoint;
use crate::mm::pmm;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rights(pub u32);

impl Rights {
    pub const SEND: Rights = Rights(1 << 0);
    pub const RECV: Rights = Rights(1 << 1);
    pub const GRANT: Rights = Rights(1 << 2);
    pub const MAP_READ: Rights = Rights(1 << 3);
    pub const MAP_WRITE: Rights = Rights(1 << 4);

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
        Some(Arc::new(Self { frames }))
    }

    pub fn frames(&self) -> &[PhysFrame] {
        &self.frames
    }

    pub fn pages(&self) -> usize {
        self.frames.len()
    }
}

impl Drop for MemoryObject {
    fn drop(&mut self) {
        for f in self.frames.drain(..) {
            pmm::free_frame(f);
        }
    }
}

#[derive(Clone)]
pub enum Object {
    Endpoint(Arc<Endpoint>),
    Memory(Arc<MemoryObject>),
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
    pub fn memory(&self) -> Option<&Arc<MemoryObject>> {
        match &self.object {
            Object::Memory(m) => Some(m),
            _ => None,
        }
    }
}

pub struct CapTable {
    slots: Vec<Option<Capability>>,
}

pub type CapSlot = u32;

const MAX_SLOTS: usize = 256;

impl CapTable {
    pub const fn new() -> Self {
        Self { slots: Vec::new() }
    }

    pub fn insert(&mut self, cap: Capability) -> Option<CapSlot> {
        if let Some(i) = self.slots.iter().position(Option::is_none) {
            self.slots[i] = Some(cap);
            return Some(i as CapSlot);
        }
        if self.slots.len() >= MAX_SLOTS {
            return None;
        }
        self.slots.push(Some(cap));
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

    pub fn remove(&mut self, slot: CapSlot) -> Option<Capability> {
        self.slots.get_mut(slot as usize)?.take()
    }
}
