//! Preemptive round-robin scheduler with kernel threads and user tasks.
//!
//! Every task owns a kernel stack; switching is done on kernel stacks only
//! (`context::switch_context`). Ring-3 tasks additionally own an address
//! space and are entered via `iretq` from their kernel thread.

pub mod task;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU32, Ordering};
use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::instructions::interrupts;
use x86_64::registers::control::{Cr3, Cr3Flags};

use crate::arch::x86_64::{context, gdt, interrupts as irq};
use crate::klog;
use task::{State, Task, TaskId};

pub use task::UserEntry;

const QUANTUM_TICKS: u32 = 4;
pub const IDLE_ID: TaskId = 0;

pub struct Scheduler {
    tasks: BTreeMap<TaskId, Box<Task>>,
    ready: VecDeque<TaskId>,
    reap: VecDeque<TaskId>,
    next_id: TaskId,
}

pub static SCHED: Mutex<Scheduler> = Mutex::new(Scheduler {
    tasks: BTreeMap::new(),
    ready: VecDeque::new(),
    reap: VecDeque::new(),
    next_id: 1,
});

static CURRENT: AtomicU32 = AtomicU32::new(IDLE_ID);

/// Top of the running task's kernel stack; read by the syscall entry stub.
#[unsafe(no_mangle)]
pub static mut CURRENT_KSTACK_TOP: u64 = 0;

/// Task that reaps dead tasks and restarts supervised services.
static SUPERVISOR: AtomicU32 = AtomicU32::new(0);

pub fn current_id() -> TaskId {
    CURRENT.load(Ordering::Relaxed)
}

pub fn init() {
    let mut s = SCHED.lock();
    s.tasks.insert(IDLE_ID, Task::boot(IDLE_ID));
    klog!("sched", "initialised (quantum {} ticks @ {} Hz)", QUANTUM_TICKS, irq::TIMER_HZ);
}

pub fn spawn_kernel(name: &'static str, entry: extern "C" fn(u64), arg: u64) -> TaskId {
    interrupts::without_interrupts(|| {
        let mut s = SCHED.lock();
        let id = s.next_id;
        s.next_id += 1;
        let t = Task::new_kernel(id, name, entry, arg);
        s.tasks.insert(id, t);
        s.ready.push_back(id);
        id
    })
}

/// Insert an already-built task (used for user tasks) and make it runnable.
pub fn add_task(mut t: Box<Task>) -> TaskId {
    interrupts::without_interrupts(|| {
        let mut s = SCHED.lock();
        let id = s.next_id;
        s.next_id += 1;
        t.id = id;
        t.state = State::Ready;
        s.tasks.insert(id, t);
        s.ready.push_back(id);
        id
    })
}

pub fn set_supervisor(id: TaskId) {
    SUPERVISOR.store(id, Ordering::Relaxed);
}

pub fn with_task<R>(id: TaskId, f: impl FnOnce(&mut Task) -> R) -> Option<R> {
    interrupts::without_interrupts(|| SCHED.lock().tasks.get_mut(&id).map(|t| f(t)))
}

pub fn with_current<R>(f: impl FnOnce(&mut Task) -> R) -> R {
    with_task(current_id(), f).expect("current task vanished")
}

/// Pick the next task and switch to it. Must be called with interrupts disabled.
pub fn schedule() {
    let cur_id = current_id();
    let (prev_sp_ptr, next_sp) = {
        let mut s = SCHED.lock();

        let next_id = loop {
            match s.ready.pop_front() {
                Some(id) => {
                    if matches!(s.tasks.get(&id).map(|t| t.state), Some(State::Ready)) {
                        break id;
                    }
                }
                None => {
                    let cur_state = s.tasks.get(&cur_id).map(|t| t.state);
                    break if cur_state == Some(State::Running) { cur_id } else { IDLE_ID };
                }
            }
        };

        if next_id == cur_id {
            let t = s.tasks.get_mut(&cur_id).unwrap();
            t.quantum_left = QUANTUM_TICKS;
            return;
        }

        if let Some(cur) = s.tasks.get_mut(&cur_id)
            && cur.state == State::Running
        {
            cur.state = State::Ready;
            if cur_id != IDLE_ID {
                s.ready.push_back(cur_id);
            }
        }

        let prev_sp_ptr = s.tasks.get_mut(&cur_id).map(|t| addr_of_mut!(t.ctx_sp)).unwrap_or(core::ptr::null_mut());

        let next = s.tasks.get_mut(&next_id).unwrap();
        next.state = State::Running;
        next.quantum_left = QUANTUM_TICKS;
        let next_sp = next.ctx_sp;
        let kstack_top = next.kstack_top();
        let cr3 = next.addr_space.as_ref().map(|a| a.cr3()).unwrap_or_else(crate::mm::vmm::kernel_pml4);

        CURRENT.store(next_id, Ordering::Relaxed);
        if kstack_top != 0 {
            gdt::set_kernel_stack(VirtAddr::new(kstack_top));
            unsafe { *addr_of_mut!(CURRENT_KSTACK_TOP) = kstack_top };
        }
        if Cr3::read().0 != cr3 {
            unsafe { Cr3::write(cr3, Cr3Flags::empty()) };
        }
        (prev_sp_ptr, next_sp)
    };

    let mut scratch = 0u64;
    let prev = if prev_sp_ptr.is_null() { &mut scratch as *mut u64 } else { prev_sp_ptr };
    unsafe { context::switch_context(prev, next_sp) };
}

pub fn yield_now() {
    interrupts::without_interrupts(schedule);
}

pub fn sleep_ms(ms: u64) {
    let until = irq::ticks() + (ms * irq::TIMER_HZ as u64).div_ceil(1000).max(1);
    interrupts::without_interrupts(|| {
        with_current(|t| {
            t.state = State::Sleeping;
            t.sleep_until = until;
        });
        schedule();
    });
}

/// Block the current task until `wake` is called on it.
pub fn block_current() {
    interrupts::without_interrupts(|| {
        with_current(|t| t.state = State::Blocked);
        schedule();
    });
}

pub fn wake(id: TaskId) {
    interrupts::without_interrupts(|| {
        let mut s = SCHED.lock();
        if let Some(t) = s.tasks.get_mut(&id)
            && matches!(t.state, State::Blocked | State::Sleeping)
        {
            t.state = State::Ready;
            s.ready.push_back(id);
        }
    });
}

pub fn exit_current(code: i64) -> ! {
    interrupts::disable();
    let id = current_id();
    {
        let mut s = SCHED.lock();
        if let Some(t) = s.tasks.get_mut(&id) {
            t.state = State::Dead;
            t.exit_code = Some(code);
        }
        s.reap.push_back(id);
    }
    let sup = SUPERVISOR.load(Ordering::Relaxed);
    if sup != 0 {
        wake(sup);
    }
    schedule();
    unreachable!("dead task was scheduled");
}

pub extern "C" fn thread_exit_hook() -> ! {
    exit_current(0)
}

/// Pop one dead task for the supervisor to inspect and free.
pub fn take_dead() -> Option<Box<Task>> {
    interrupts::without_interrupts(|| {
        let mut s = SCHED.lock();
        let id = s.reap.pop_front()?;
        s.tasks.remove(&id)
    })
}

pub fn on_tick() {
    let now = irq::ticks();
    let mut s = SCHED.lock();
    let mut woke = alloc::vec::Vec::new();
    for (id, t) in s.tasks.iter_mut() {
        if t.state == State::Sleeping && t.sleep_until <= now {
            t.state = State::Ready;
            woke.push(*id);
        }
    }
    s.ready.extend(woke);

    let cur = current_id();
    let preempt = match s.tasks.get_mut(&cur) {
        Some(t) => {
            t.quantum_left = t.quantum_left.saturating_sub(1);
            t.quantum_left == 0 || cur == IDLE_ID
        }
        None => true,
    };
    let has_ready = !s.ready.is_empty();
    drop(s);
    if preempt && has_ready {
        schedule();
    }
}

pub fn on_keyboard(scancode: u8) {
    crate::ipc::on_keyboard(scancode);
}

pub fn on_user_fault(what: &str, code: u64, rip: u64) -> ! {
    let (id, name) = with_current(|t| (t.id, t.name));
    klog!("fault", "task {} '{}' {} code={:#x} rip={:#x} -> killed", id, name, what, code, rip);
    exit_current(-1)
}

pub fn on_user_page_fault(addr: u64, code: u64, rip: u64) -> ! {
    let (id, name) = with_current(|t| (t.id, t.name));
    klog!("fault", "task {} '{}' #PF addr={:#x} code={:#x} rip={:#x} -> killed", id, name, addr, code, rip);
    exit_current(-1)
}

pub fn task_count() -> usize {
    interrupts::without_interrupts(|| SCHED.lock().tasks.len())
}

pub fn dump() {
    interrupts::without_interrupts(|| {
        let s = SCHED.lock();
        for (id, t) in s.tasks.iter() {
            klog!("sched", "  #{:<3} {:<12} {:?}{}", id, t.name, t.state, if t.is_user() { " (ring3)" } else { "" });
        }
    });
}
