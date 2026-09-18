//! Synchronous message-passing endpoints — the only way tasks talk to each
//! other or to the kernel's device services.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use spin::Mutex;
use x86_64::instructions::interrupts;

use crate::sched::{self, task::TaskId};

pub const MSG_WORDS: usize = 4;
const QUEUE_LIMIT: usize = 64;

#[derive(Clone, Copy, Debug, Default)]
pub struct Message {
    pub sender: TaskId,
    pub words: [u64; MSG_WORDS],
}

#[derive(Default)]
struct EndpointInner {
    queue: VecDeque<Message>,
    receivers: VecDeque<TaskId>,
}

pub struct Endpoint {
    inner: Mutex<EndpointInner>,
    pub name: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcError {
    QueueFull,
}

impl Endpoint {
    pub fn new(name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(EndpointInner::default()),
            name,
        })
    }

    /// Deliver a message: straight into a waiting receiver's inbox if there is
    /// one (and wake it), otherwise queue it. Never blocks the sender.
    pub fn send(&self, msg: Message) -> Result<(), IpcError> {
        interrupts::without_interrupts(|| {
            let mut inner = self.inner.lock();
            while let Some(rx) = inner.receivers.pop_front() {
                let delivered = sched::with_task(rx, |t| {
                    if t.state == sched::task::State::Blocked {
                        t.ipc_inbox = Some(msg);
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false);
                if delivered {
                    drop(inner);
                    sched::wake(rx);
                    return Ok(());
                }
            }
            if inner.queue.len() >= QUEUE_LIMIT {
                return Err(IpcError::QueueFull);
            }
            inner.queue.push_back(msg);
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

    pub fn try_recv(&self) -> Option<Message> {
        interrupts::without_interrupts(|| self.inner.lock().queue.pop_front())
    }

    pub fn pending(&self) -> usize {
        interrupts::without_interrupts(|| self.inner.lock().queue.len())
    }
}

/// Kernel-owned endpoint fed by the keyboard IRQ: a tiny "driver as a
/// message source" so user tasks can receive scancodes over IPC.
static KEYBOARD_EP: Mutex<Option<Arc<Endpoint>>> = Mutex::new(None);

pub fn init() {
    *KEYBOARD_EP.lock() = Some(Endpoint::new("keyboard"));
}

pub fn keyboard_endpoint() -> Arc<Endpoint> {
    KEYBOARD_EP.lock().as_ref().expect("ipc not initialised").clone()
}

pub fn on_keyboard(scancode: u8) {
    if let Some(ep) = KEYBOARD_EP.lock().as_ref() {
        let _ = ep.send(Message {
            sender: 0,
            words: [scancode as u64, 0, 0, 0],
        });
    }
}
