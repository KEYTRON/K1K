//! Asynchronous notifications: a counter that remembers that something
//! happened, and the tasks waiting for it to happen again.
//!
//! An endpoint is the right shape for a conversation: a message has a sender, a
//! payload, and somewhere to go. It is the wrong shape for "the device did a
//! thing", for two reasons. Every interrupt becomes a message that has to be
//! allocated, queued and freed on a path that cannot afford any of that; and a
//! queue that is full loses the event silently — the driver never learns it
//! missed one, which is what the `dropped` counter on an interrupt object has
//! been apologising for.
//!
//! A notification has no payload to lose. The kernel bumps a counter and the
//! waiter takes one off whenever it gets round to it, so an event that arrives
//! before the wait is not lost, it is simply still there. `Wait::All` is the
//! other half of that: a driver that is going to drain the device anyway takes
//! every completion in one go instead of one wake-up per interrupt.
//!
//! The same object serves the direction the kernel needs for its own services:
//! a task waiting for something another task will do. `SIGNAL` and `WAIT` are
//! separate rights, so the waiting half of a relationship can be handed out
//! without the other, and the revocation work later only has to take the right
//! back.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;
use x86_64::instructions::interrupts;

use crate::sched::{self, task::TaskId};

/// How many signals a wait takes, and whether it sleeps.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wait {
    /// One signal, blocking until there is one.
    One,
    /// One signal, or nothing if there is none. What a driver polls with.
    Poll,
    /// Every pending signal at once, blocking until there is at least one.
    All,
}

#[derive(Default)]
struct Inner {
    pending: u32,
    waiters: VecDeque<TaskId>,
}

/// What one turn of the wait loop did.
enum Step {
    /// Signals were taken; the value is what to report to the caller.
    Ready(u32),
    /// Nothing pending and the caller asked not to sleep.
    Empty,
    /// Registered and blocked, then rescheduled. Whatever woke us may have had
    /// its signals taken by somebody else first, so the loop looks again.
    Blocked,
}

pub struct Notify {
    inner: Mutex<Inner>,
    /// Signals recorded since boot.
    pub signals: AtomicU64,
    /// Signals handed to waiting tasks since boot.
    pub waits: AtomicU64,
    /// Signals that arrived with nowhere to go: the counter was already at its
    /// maximum, so an event was folded into another one.
    pub saturated: AtomicU64,
}

/// Every notification ever created, so the boot summary can say what the
/// notification path actually did. Bounded: a service that creates and drops
/// notifications in a loop must not be able to grow this without limit, and the
/// counters it keeps are cumulative anyway, so a forgotten object costs nothing
/// but a line of diagnostics.
const TRACKED: usize = 64;
static TRACK: Mutex<Vec<Arc<Notify>>> = Mutex::new(Vec::new());
static UNTRACKED: AtomicU64 = AtomicU64::new(0);

impl Notify {
    pub fn new() -> Arc<Self> {
        let this = Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            signals: AtomicU64::new(0),
            waits: AtomicU64::new(0),
            saturated: AtomicU64::new(0),
        });
        let mut track = TRACK.lock();
        if track.len() < TRACKED {
            track.push(this.clone());
        } else {
            UNTRACKED.fetch_add(1, Ordering::Relaxed);
        }
        this
    }

    /// What the notification path has done: notifications created, signals
    /// recorded, signals taken, signals folded into an already-saturated
    /// counter, tasks blocked in a wait right now, and signals nobody has taken
    /// yet. A `pending` that keeps growing is a driver that has stopped
    /// listening, which is the failure this counter exists to make visible.
    pub fn report() -> (usize, u64, u64, u64, usize, u32) {
        let track = TRACK.lock();
        let signals = track
            .iter()
            .map(|n| n.signals.load(Ordering::Relaxed))
            .sum();
        let waits = track.iter().map(|n| n.waits.load(Ordering::Relaxed)).sum();
        let saturated = track
            .iter()
            .map(|n| n.saturated.load(Ordering::Relaxed))
            .sum::<u64>()
            + UNTRACKED.load(Ordering::Relaxed);
        let waiting = track.iter().map(|n| n.waiters()).sum();
        let pending = track.iter().map(|n| n.pending()).sum();
        (track.len(), signals, waits, saturated, waiting, pending)
    }

    /// Record `count` signals and wake up to that many waiters; returns how
    /// many were woken.
    ///
    /// Callable from an interrupt handler: the waiters are picked out under the
    /// lock and the waking happens once it is dropped, so a signal never holds
    /// the notification's lock across the scheduler's.
    pub fn signal(&self, count: u32) -> usize {
        let count = count.max(1);
        self.signals.fetch_add(count as u64, Ordering::Relaxed);
        let mut wake = Vec::new();
        {
            let mut inner = self.inner.lock();
            let room = u32::MAX - inner.pending;
            if count > room {
                self.saturated
                    .fetch_add((count - room) as u64, Ordering::Relaxed);
            }
            inner.pending = inner.pending.saturating_add(count);
            while wake.len() < count as usize {
                let Some(id) = inner.waiters.pop_front() else {
                    break;
                };
                // A task can die while blocked here — killed by a fault, or by
                // the supervisor — so a candidate is only woken if it is still
                // waiting. Its slot in the queue goes away with it.
                let waiting =
                    sched::with_task(id, |t| t.state() == crate::sched::task::State::Blocked)
                        .unwrap_or(false);
                if waiting {
                    wake.push(id);
                }
            }
        }
        for id in wake.iter() {
            sched::wake(*id);
        }
        wake.len()
    }

    /// Take a signal, blocking until there is one unless the mode says not to.
    /// Returns how many signals were taken — one, or everything pending for
    /// [`Wait::All`] — or `None` for a [`Wait::Poll`] with nothing pending.
    ///
    /// Looking for a signal and registering as a waiter happen in the same
    /// interrupts-off section, so a signal cannot slip in between and have its
    /// wake-up lost: a task either finds a signal or is on the list before a
    /// signaller can look.
    pub fn wait(&self, mode: Wait) -> Option<u32> {
        loop {
            let step = interrupts::without_interrupts(|| {
                let mut inner = self.inner.lock();
                if inner.pending > 0 {
                    let taken = match mode {
                        Wait::All => core::mem::replace(&mut inner.pending, 0),
                        Wait::One | Wait::Poll => {
                            inner.pending -= 1;
                            1
                        }
                    };
                    return Step::Ready(taken);
                }
                if mode == Wait::Poll {
                    return Step::Empty;
                }
                inner.waiters.push_back(sched::current_id());
                sched::mark_blocked();
                drop(inner);
                sched::schedule();
                Step::Blocked
            });
            match step {
                Step::Ready(n) => {
                    self.waits.fetch_add(n as u64, Ordering::Relaxed);
                    return Some(n);
                }
                Step::Empty => return None,
                Step::Blocked => {}
            }
        }
    }

    /// Signals waiting to be taken, for a diagnostic.
    pub fn pending(&self) -> u32 {
        self.inner.lock().pending
    }

    /// Tasks blocked in [`Notify::wait`], for a diagnostic.
    pub fn waiters(&self) -> usize {
        self.inner.lock().waiters.len()
    }
}
