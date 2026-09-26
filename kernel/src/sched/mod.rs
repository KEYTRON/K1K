//! Preemptive round-robin scheduler for many CPUs.
//!
//! One global run queue guarded by `SCHED`; each CPU has its own idle task
//! and `current` in its per-CPU block. A task that is being switched away
//! from is *not* requeued until the CPU has actually left its stack
//! (`finish_switch`), and `wake` never enqueues a task that is still on a
//! CPU — together these keep another core from resuming a half-saved context.

pub mod task;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU32, Ordering};
use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::instructions::interrupts;
use x86_64::registers::control::{Cr3, Cr3Flags};

use crate::arch::x86_64::{context, gdt, interrupts as irq, percpu};
use crate::klog;
use task::{State, Task, TaskId};

pub use task::UserEntry;

const QUANTUM_TICKS: u32 = 10;
pub const NO_TASK: TaskId = percpu::NO_TASK;

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
    next_id: 0,
});

/// Task that reaps dead tasks and restarts supervised services.
static SUPERVISOR: AtomicU32 = AtomicU32::new(NO_TASK);

pub fn current_id() -> TaskId {
    percpu::get().current
}

/// Turn the calling CPU's boot context into its idle task.
pub fn register_idle_cpu() -> TaskId {
    interrupts::without_interrupts(|| {
        let mut s = SCHED.lock();
        let id = s.next_id;
        s.next_id += 1;
        let mut t = Task::boot(id);
        t.on_cpu = Some(percpu::cpu_id());
        s.tasks.insert(id, t);
        let pc = percpu::get();
        pc.current = id;
        pc.idle_task = id;
        id
    })
}

pub fn init() {
    let id = register_idle_cpu();
    klog!(
        "sched",
        "initialised on cpu 0 (idle task {}, quantum {} ms)",
        id,
        QUANTUM_TICKS * 1000 / irq::TIMER_HZ
    );
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

/// Take the next task id without publishing a task. A ring-3 service reserves
/// its id first so it can finish the address space (boot info page) before the
/// task becomes reachable by any CPU.
pub fn reserve_task_id() -> TaskId {
    interrupts::without_interrupts(|| {
        let mut s = SCHED.lock();
        let id = s.next_id;
        s.next_id += 1;
        id
    })
}

/// Publish a task whose id came from [`reserve_task_id`]: insert it and make
/// it runnable.
pub fn publish_task(mut t: Box<Task>, id: TaskId) {
    interrupts::without_interrupts(|| {
        let mut s = SCHED.lock();
        t.id = id;
        t.state = State::Ready;
        s.tasks.insert(id, t);
        s.ready.push_back(id);
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

fn pick_next(s: &mut Scheduler) -> Option<TaskId> {
    while let Some(id) = s.ready.pop_front() {
        if let Some(t) = s.tasks.get(&id)
            && t.state == State::Ready
            && t.on_cpu.is_none()
        {
            return Some(id);
        }
    }
    None
}

/// Pick the next task and switch to it. Must be called with interrupts disabled.
pub fn schedule() {
    let pc = percpu::get();
    let cpu = pc.cpu_id;
    let cur_id = pc.current;

    let (prev_sp_ptr, next_sp) = {
        let mut s = SCHED.lock();

        let next_id = match pick_next(&mut s) {
            Some(id) => id,
            None => {
                let cur_running = s.tasks.get(&cur_id).map(|t| t.state) == Some(State::Running);
                if cur_running { cur_id } else { pc.idle_task }
            }
        };

        if next_id == cur_id {
            if let Some(t) = s.tasks.get_mut(&cur_id) {
                t.quantum_left = QUANTUM_TICKS;
                if t.state == State::Ready {
                    t.state = State::Running;
                }
            }
            return;
        }

        let cur = s.tasks.get_mut(&cur_id).expect("current task vanished");
        if cur.state == State::Running {
            cur.state = State::Ready;
        }
        let prev_sp_ptr = addr_of_mut!(cur.ctx_sp);

        let next = s.tasks.get_mut(&next_id).expect("next task vanished");
        next.state = State::Running;
        next.on_cpu = Some(cpu);
        next.quantum_left = QUANTUM_TICKS;
        let next_sp = next.ctx_sp;
        let kstack_top = next.kstack_top();
        let cr3 = next
            .addr_space
            .as_ref()
            .map(|a| a.cr3())
            .unwrap_or_else(crate::mm::vmm::kernel_pml4);

        pc.current = next_id;
        pc.prev_pending = cur_id;
        pc.switches += 1;
        if kstack_top != 0 {
            gdt::set_kernel_stack(VirtAddr::new(kstack_top));
            pc.kstack_top = kstack_top;
        }
        if Cr3::read().0 != cr3 {
            unsafe { Cr3::write(cr3, Cr3Flags::empty()) };
        }
        (prev_sp_ptr, next_sp)
    };

    unsafe { context::switch_context(prev_sp_ptr, next_sp) };
    finish_switch();
}

/// Runs on the new context right after a switch: the previous task has left
/// its stack, so it may now be picked up by any CPU.
pub extern "C" fn finish_switch() {
    let pc = percpu::get();
    let prev = pc.prev_pending;
    pc.prev_pending = NO_TASK;
    if prev == NO_TASK {
        return;
    }
    let mut s = SCHED.lock();
    if let Some(t) = s.tasks.get_mut(&prev) {
        t.on_cpu = None;
        if t.state == State::Ready && !t.is_idle {
            s.ready.push_back(prev);
        }
    }
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

/// Mark the current task blocked. Interrupts must already be disabled; the
/// caller follows up with `schedule()` after releasing its own locks.
pub fn mark_blocked() {
    with_current(|t| t.state = State::Blocked);
}

pub fn wake(id: TaskId) {
    interrupts::without_interrupts(|| {
        let mut s = SCHED.lock();
        if let Some(t) = s.tasks.get_mut(&id)
            && matches!(t.state, State::Blocked | State::Sleeping)
        {
            t.state = State::Ready;
            if t.on_cpu.is_none() {
                s.ready.push_back(id);
            }
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
    if sup != NO_TASK {
        wake(sup);
    }
    schedule();
    unreachable!("dead task was scheduled");
}

pub extern "C" fn thread_exit_hook() -> ! {
    exit_current(0)
}

/// Pop one dead task that has fully left its CPU, for the supervisor to free.
pub fn take_dead() -> Option<Box<Task>> {
    interrupts::without_interrupts(|| {
        let mut s = SCHED.lock();
        let pos = s
            .reap
            .iter()
            .position(|id| s.tasks.get(id).is_some_and(|t| t.on_cpu.is_none()))?;
        let id = s.reap.remove(pos)?;
        s.tasks.remove(&id)
    })
}

pub fn on_tick() {
    let now = irq::ticks();
    let cur = current_id();
    let mut s = SCHED.lock();
    // Disjoint field borrows: wake sleepers without allocating in IRQ context.
    let sched = &mut *s;
    for (id, t) in sched.tasks.iter_mut() {
        if t.state == State::Sleeping && t.sleep_until <= now {
            t.state = State::Ready;
            if t.on_cpu.is_none() {
                sched.ready.push_back(*id);
            }
        }
    }

    let preempt = match s.tasks.get_mut(&cur) {
        Some(t) => {
            t.quantum_left = t.quantum_left.saturating_sub(1);
            t.quantum_left == 0 || t.is_idle
        }
        None => true,
    };
    let has_ready = !s.ready.is_empty();
    drop(s);
    if preempt && has_ready {
        schedule();
    }
}

pub fn on_user_fault(what: &str, code: u64, rip: u64) -> ! {
    let (id, name) = with_current(|t| (t.id, t.name));
    klog!(
        "fault",
        "task {} '{}' {} code={:#x} rip={:#x} cpu={} -> killed",
        id,
        name,
        what,
        code,
        rip,
        percpu::cpu_id()
    );
    exit_current(-1)
}

pub fn on_user_page_fault(addr: u64, code: u64, rip: u64) -> ! {
    let (id, name) = with_current(|t| (t.id, t.name));
    klog!(
        "fault",
        "task {} '{}' #PF addr={:#x} code={:#x} rip={:#x} cpu={} -> killed",
        id,
        name,
        addr,
        code,
        rip,
        percpu::cpu_id()
    );
    exit_current(-1)
}

pub fn task_count() -> usize {
    interrupts::without_interrupts(|| SCHED.lock().tasks.len())
}

pub fn dump() {
    interrupts::without_interrupts(|| {
        let s = SCHED.lock();
        for (id, t) in s.tasks.iter() {
            klog!(
                "sched",
                "  #{:<3} {:<12} {:?}{}{}",
                id,
                t.name,
                t.state,
                if t.is_user() { " (ring3)" } else { "" },
                match t.on_cpu {
                    Some(c) => alloc::format!(" on cpu {}", c),
                    None => alloc::string::String::new(),
                }
            );
        }
    });
}
