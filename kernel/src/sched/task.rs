use alloc::boxed::Box;
use alloc::vec;
use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use crate::arch::x86_64::context;
use crate::mm::vmm::AddressSpace;
use crate::obj::CapTable;

pub type TaskId = u32;

pub const KSTACK_SIZE: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Ready,
    Running,
    Blocked,
    Sleeping,
    Dead,
}

#[derive(Clone, Copy, Debug)]
pub struct UserEntry {
    pub rip: u64,
    pub rsp: u64,
}

pub struct Task {
    pub id: TaskId,
    pub name: &'static str,
    /// Where the task is in its life, and which CPU it is on, are read by other
    /// CPUs without the table lock: a task on a run queue is owned by that queue,
    /// so the flags describing it are atomic rather than locked.
    state: AtomicU8,
    on_cpu: AtomicU32,
    pub kstack: Box<[u8]>,
    pub ctx_sp: u64,
    pub addr_space: Option<AddressSpace>,
    pub user: Option<UserEntry>,
    pub caps: CapTable,
    pub sleep_until: u64,
    pub exit_code: Option<i64>,
    quantum_left: AtomicU32,
    /// Message slot for IPC: filled by a sender while we are blocked in recv.
    pub ipc_inbox: Option<crate::ipc::Message>,
    /// Which service (if any) this task instantiates — for supervision.
    pub service: Option<usize>,
    /// A CPU's idle task: never queued, run only when nothing else is ready.
    pub is_idle: bool,
    /// The last CPU this task ran on, kept so that a woken task goes back where
    /// its cache is warm even when it is not on a CPU right now.
    last_seen: AtomicU32,
    /// Where this task is, in one word: nowhere, on a run queue, or running on a
    /// CPU. A runnable task is in exactly one place, and this is the one place
    /// that says where — one compare-and-swap, so every transition can be checked
    /// against the one before it.
    placement: AtomicU8,
    /// The stack a boot context runs on, for the CPUs whose boot context is not
    /// a task with a stack of its own. Without it, switching back to a boot
    /// context would leave the CPU's ring-0 stack pointing at whichever task ran
    /// before it, and the next interrupt would land on a foreign stack.
    boot_stack: u64,
}

impl Task {
    /// Move the task from one place to another, saying what it was found in
    /// rather than assuming it. The scheduler checks every transition, so a task
    /// that is ever in two places at once is caught at the transition that made
    /// it happen, not two CPUs later.
    pub fn move_to(&self, from: u8, to: u8) -> bool {
        self.placement
            .compare_exchange(from, to, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Mark a context as running without a switch: a CPU's boot context is
    /// running from the moment it is registered.
    pub fn set_running(&self) {
        self.placement.store(RUNNING, Ordering::Release);
    }

    /// Where the task is: `NOWHERE`, `QUEUED` or `RUNNING`.
    pub fn where_is(&self) -> u8 {
        self.placement.load(Ordering::Acquire)
    }

    /// Whether the task is on a run queue right now.
    pub fn is_queued(&self) -> bool {
        self.where_is() == QUEUED
    }
}

/// Where a task is: nowhere at all, on a run queue, or running on a CPU.
pub const NOWHERE: u8 = 0;
pub const QUEUED: u8 = 1;
pub const RUNNING: u8 = 2;

/// `on_cpu` when the task is on nobody's CPU.
const NO_CPU: u32 = u32::MAX;

impl State {
    fn into(self) -> AtomicU8 {
        AtomicU8::new(self as u8)
    }
}

impl Task {
    /// Where the task is in its life.
    #[inline]
    pub fn state(&self) -> State {
        match self.state.load(Ordering::Acquire) {
            0 => State::Ready,
            1 => State::Running,
            2 => State::Blocked,
            3 => State::Sleeping,
            _ => State::Dead,
        }
    }

    #[inline]
    pub fn set_state(&self, state: State) {
        self.state.store(state as u8, Ordering::Release);
    }

    /// The CPU whose stack this task is on, if any. `None` between a switch and
    /// the `finish_switch` that follows it.
    #[inline]
    pub fn cpu(&self) -> Option<u32> {
        match self.on_cpu.load(Ordering::Acquire) {
            NO_CPU => None,
            c => Some(c),
        }
    }

    /// The CPU this task last ran on, which is where its data is warmest.
    #[inline]
    pub fn last_cpu(&self) -> Option<u32> {
        let seen = self.last_seen.load(Ordering::Relaxed);
        if seen == NO_CPU { None } else { Some(seen) }
    }

    #[inline]
    pub fn claim_cpu(&self, cpu: u32) {
        self.last_seen.store(cpu, Ordering::Relaxed);
        self.on_cpu.store(cpu, Ordering::Release);
    }

    #[inline]
    pub fn release_cpu(&self) {
        self.on_cpu.store(NO_CPU, Ordering::Release);
    }

    #[inline]
    pub fn quantum_left(&self) -> u32 {
        self.quantum_left.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn set_quantum(&self, ticks: u32) {
        self.quantum_left.store(ticks, Ordering::Relaxed);
    }

    #[inline]
    pub fn reset_quantum(&self) {
        self.set_quantum(crate::sched::QUANTUM_TICKS);
    }

    pub fn new_kernel(
        id: TaskId,
        name: &'static str,
        entry: extern "C" fn(u64),
        arg: u64,
    ) -> Box<Self> {
        let kstack = vec![0u8; KSTACK_SIZE].into_boxed_slice();
        let top = kstack.as_ptr() as u64 + KSTACK_SIZE as u64;
        let ctx_sp = unsafe { context::init_stack(top, entry as usize as u64, arg) };
        Box::new(Self {
            id,
            name,
            state: State::Ready.into(),
            kstack,
            ctx_sp,
            addr_space: None,
            user: None,
            caps: CapTable::new(),
            sleep_until: 0,
            exit_code: None,
            quantum_left: AtomicU32::new(0),
            ipc_inbox: None,
            service: None,
            on_cpu: AtomicU32::new(NO_CPU),
            last_seen: AtomicU32::new(NO_CPU),
            placement: AtomicU8::new(NOWHERE),
            boot_stack: 0,
            is_idle: false,
        })
    }

    /// A CPU's boot context: no owned stack, already running.
    ///
    /// On a processor whose boot code has nothing left to do after this, that
    /// context *is* the CPU's idle task. On the boot processor it still has the
    /// kernel's own work to finish, so it is an ordinary task and the CPU gets an
    /// idle task of its own to fall back on — otherwise a boot processor that
    /// waited for anything could never be put to sleep, since the only context
    /// that could run while it waits is itself.
    pub fn boot(id: TaskId, name: &'static str, is_idle: bool, stack_top: u64) -> Box<Self> {
        Box::new(Self {
            id,
            name,
            state: State::Running.into(),
            kstack: Box::new([]),
            ctx_sp: 0,
            addr_space: None,
            user: None,
            caps: CapTable::new(),
            sleep_until: 0,
            exit_code: None,
            quantum_left: AtomicU32::new(0),
            ipc_inbox: None,
            service: None,
            on_cpu: AtomicU32::new(NO_CPU),
            last_seen: AtomicU32::new(NO_CPU),
            placement: AtomicU8::new(NOWHERE),
            boot_stack: stack_top,
            is_idle,
        })
    }

    pub fn kstack_top(&self) -> u64 {
        if self.kstack.is_empty() {
            self.boot_stack
        } else {
            self.kstack.as_ptr() as u64 + self.kstack.len() as u64
        }
    }

    pub fn is_user(&self) -> bool {
        self.user.is_some()
    }
}
