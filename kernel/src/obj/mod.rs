//! Kernel objects and capabilities.
//!
//! A capability is an unforgeable reference to a kernel object plus a rights
//! mask. User tasks never see object pointers — only slot indices into their
//! own `CapTable`, checked on every syscall. This is the whole access-control
//! model: no UIDs, no ambient authority.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::ipc::Endpoint;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rights(pub u32);

impl Rights {
    pub const SEND: Rights = Rights(1 << 0);
    pub const RECV: Rights = Rights(1 << 1);
    pub const GRANT: Rights = Rights(1 << 2);
    pub const ALL: Rights = Rights(0b111);

    pub const fn contains(self, other: Rights) -> bool {
        self.0 & other.0 == other.0
    }
    pub const fn intersect(self, other: Rights) -> Rights {
        Rights(self.0 & other.0)
    }
}

#[derive(Clone)]
pub enum Object {
    Endpoint(Arc<Endpoint>),
}

#[derive(Clone)]
pub struct Capability {
    pub object: Object,
    pub rights: Rights,
}

pub struct CapTable {
    slots: Vec<Option<Capability>>,
}

pub type CapSlot = u32;

impl CapTable {
    pub const fn new() -> Self {
        Self { slots: Vec::new() }
    }

    pub fn insert(&mut self, cap: Capability) -> CapSlot {
        if let Some(i) = self.slots.iter().position(Option::is_none) {
            self.slots[i] = Some(cap);
            return i as CapSlot;
        }
        self.slots.push(Some(cap));
        (self.slots.len() - 1) as CapSlot
    }

    pub fn get(&self, slot: CapSlot) -> Option<&Capability> {
        self.slots.get(slot as usize)?.as_ref()
    }

    /// Look up a slot and check that it carries `rights`.
    pub fn lookup(&self, slot: CapSlot, rights: Rights) -> Option<&Capability> {
        let cap = self.get(slot)?;
        cap.rights.contains(rights).then_some(cap)
    }

    /// Copy a capability into another table with (possibly reduced) rights.
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

    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }
}
