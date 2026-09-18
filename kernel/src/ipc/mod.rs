//! Synchronous message-passing endpoints — the only way tasks talk to each
//! other or to the kernel's device services. A message may carry one
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcError {
    QueueFull,
}

impl Endpoint {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(EndpointInner::default()),
        })
    }

    /// Deliver a message: straight into a waiting receiver's inbox if there is
    /// one (and wake it), otherwise queue it. Never blocks the sender.
    pub fn send(&self, msg: Message) -> Result<(), IpcError> {
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
    pub fn recv(&self) -> Message {
        loop {
            let queued = interrupts::without_interrupts(|| {
                let mut inner = self.inner.lock();
                if let Some(m) = inner.queue.pop_front() {
                    return Some(m);
                }
                let me = sched::current_id();
                inner.receivers.push_back(me);
                sched::with_current(|t| t.ipc_inbox = None);
                None
            });
            if let Some(m) = queued {
                return m;
            }
            sched::block_current();
            if let Some(m) = sched::with_current(|t| t.ipc_inbox.take()) {
                return m;
            }
        }
    }
}

/// Kernel-owned endpoint fed by the keyboard IRQ: a tiny "driver as a
/// message source" so tasks can receive scancodes over IPC.
static KEYBOARD_EP: Mutex<Option<Arc<Endpoint>>> = Mutex::new(None);

pub fn init() {
    *KEYBOARD_EP.lock() = Some(Endpoint::new());
}

pub fn keyboard_endpoint() -> Arc<Endpoint> {
    KEYBOARD_EP
        .lock()
        .as_ref()
        .expect("ipc not initialised")
        .clone()
}

pub fn on_keyboard(scancode: u8) {
    if let Some(ep) = KEYBOARD_EP.lock().as_ref() {
        let _ = ep.send(Message::new(0, [scancode as u64, 0, 0, 0]));
    }
}
