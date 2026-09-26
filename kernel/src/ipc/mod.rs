//! Synchronous message-passing endpoints — the only way tasks talk to each
//! other, to drivers, or receive device interrupts. A message may carry one
//! capability, which is how authority is delegated between tasks.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use spin::Mutex;
use x86_64::instructions::interrupts;

use crate::obj::Capability;
use crate::sched::{self, task::TaskId};

pub const MSG_WORDS: usize = 4;
const QUEUE_LIMIT: usize = 64;

#[derive(Clone, Default)]
pub struct Message {
    pub sender: TaskId,
    pub words: [u64; MSG_WORDS],
    pub cap: Option<Capability>,
}

impl Message {
    pub fn new(sender: TaskId, words: [u64; MSG_WORDS]) -> Self {
        Self {
            sender,
            words,
            cap: None,
        }
    }
}

#[derive(Default)]
struct EndpointInner {
    queue: VecDeque<Message>,
    receivers: VecDeque<TaskId>,
}

pub struct Endpoint {
    inner: Mutex<EndpointInner>,
    /// The other half of a connected pair, if this endpoint has one. Sending
    /// delivers to the peer's queue; the lock is only held long enough to clone
    /// the `Arc`, so a send and a receive on opposite halves cannot deadlock.
    peer: Mutex<Option<Arc<Endpoint>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcError {
    QueueFull,
}

impl Endpoint {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(EndpointInner::default()),
            peer: Mutex::new(None),
        })
    }

    /// A connected pair: sending on one half delivers to the other half's
    /// queue. This is what lets a server keep the receiving half and hand out
    /// the sending half — two different objects, so the rights on each can
    /// stay one-directional instead of being intersected into nothing.
    pub fn new_pair() -> (Arc<Self>, Arc<Self>) {
        let a = Endpoint::new();
        let b = Endpoint::new();
        *a.peer.lock() = Some(b.clone());
        *b.peer.lock() = Some(a.clone());
        (a, b)
    }

    /// Deliver a message: straight into a waiting receiver's inbox if there is
    /// one (and wake it), otherwise queue it. Never blocks the sender.
    pub fn send(&self, msg: Message) -> Result<(), IpcError> {
        let peer = self.peer.lock().clone();
        match peer {
            Some(p) => p.deliver(msg),
            None => self.deliver(msg),
        }
    }

    fn deliver(&self, msg: Message) -> Result<(), IpcError> {
        interrupts::without_interrupts(|| {
            let mut inner = self.inner.lock();
            let mut pending = Some(msg);
            while let Some(rx) = inner.receivers.pop_front() {
                let msg = pending.take().unwrap();
                let leftover = sched::with_task(rx, |t| {
                    if t.state == sched::task::State::Blocked {
                        t.ipc_inbox = Some(msg);
                        None
                    } else {
                        Some(msg)
                    }
                })
                .unwrap_or(None);
                match leftover {
                    None => {
                        drop(inner);
                        sched::wake(rx);
                        return Ok(());
                    }
                    Some(m) => pending = Some(m),
                }
            }
            if inner.queue.len() >= QUEUE_LIMIT {
                return Err(IpcError::QueueFull);
            }
            inner.queue.push_back(pending.take().unwrap());
            Ok(())
        })
    }

    /// Receive a message, blocking the current task until one arrives.
    ///
    /// Registering as a receiver, marking ourselves blocked and switching away
    /// happen in one interrupts-off section so a sender on another CPU can
    /// never slip a message in between and have its wake-up lost.
    pub fn recv(&self) -> Message {
        loop {
            let got = interrupts::without_interrupts(|| {
                let mut inner = self.inner.lock();
                if let Some(m) = inner.queue.pop_front() {
                    return Some(m);
                }
                let me = sched::current_id();
                inner.receivers.push_back(me);
                sched::with_current(|t| {
                    t.ipc_inbox = None;
                });
                sched::mark_blocked();
                drop(inner);
                sched::schedule();
                sched::with_current(|t| t.ipc_inbox.take())
            });
            if let Some(m) = got {
                return m;
            }
        }
    }
}
