//! Preemptive round-robin scheduler for many CPUs.
//!
//! Every CPU has its own run queue and its own list of sleepers, each behind its
//! own lock. The run queues are what a switch and an idle CPU touch, and neither
//! of those takes a lock another CPU is waiting for; the one structure left
//! shared is the task table, which is only a lookup — id to task — for the paths
//! that start from an id rather than from the CPU you are on.
//!
//! The rule everything else rests on: **a task is either running on exactly one
//! CPU, or sitting in exactly one run queue, or on one CPU's sleeper list, or
//! dead in the reap list, and never in two of those at once.** A task that is
//! being switched away from is not on any queue until the CPU has actually left
//! its stack, so no other CPU can pick up a half-saved context, and a woken task
//! is on one queue before the waker lets go of anything.
//!
//! A CPU with nothing to run does not go to sleep straight away: it takes one
//! task off a neighbour's queue, starting from a cursor that moves each time, so
//! that four CPUs waiting on one busy CPU do not all hammer the same queue and
//! end up fighting over the same victim.

pub mod task;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use core::sync::atomic::{AtomicU32, Ordering};
use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::instructions::interrupts;
use x86_64::registers::control::{Cr3, Cr3Flags};

use crate::arch::x86_64::{context, gdt, interrupts as irq, ipi, percpu};
use crate::klog;
use task::{State, Task, TaskId};

pub use task::UserEntry;

const QUANTUM_TICKS: u32 = 10;
pub const NO_TASK: TaskId = percpu::NO_TASK;

/// One CPU's run queue.
///
/// Only the owning CPU takes this lock to switch, and an idle CPU takes a
/// neighbour's for as long as it takes to pop one task. A `*mut Task` rather
/// than an id, because the switching path must not have to look the task up in
/// the table to find its stack: `Box<Task>` gives a stable address, and the
/// ownership rule above is what makes reading through that pointer safe.
struct RunQueue {
    inner: Mutex<VecDeque<*mut Task>>,
    /// Tasks on this queue, for the boot summary. Only ever a hint.
    queued: AtomicU32,
}

// Safety: a task is on exactly one run queue, and that queue is only ever popped
// by the CPU that owns it — or, for a steal, popped by an idle CPU that then owns
// the task. Nothing is ever on two queues, and a task in the table is a `Box`,
// so the address a queue holds stays valid until the task is reaped, which cannot
// happen while the task is queued.
unsafe impl Send for RunQueue {}
unsafe impl Sync for RunQueue {}

impl RunQueue {
    fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }
}

static RUNS: [RunQueue; percpu::MAX_CPUS] = [const {
    RunQueue {
        inner: Mutex::new(VecDeque::new()),
        queued: AtomicU32::new(0),
    }
}; percpu::MAX_CPUS];

fn run_queue(cpu: u32) -> &'static RunQueue {
    &RUNS[cpu as usize % percpu::MAX_CPUS]
}

/// The task table: id to task, and where dead tasks wait to be freed.
///
/// This is a lookup table, not a scheduler: nothing in the switching path takes
/// it, and the work done under it is a map search and a state change.
pub struct Scheduler {
    tasks: BTreeMap<TaskId, Box<Task>>,
    reap: VecDeque<TaskId>,
    next_id: TaskId,
}

pub static SCHED: Mutex<Scheduler> = Mutex::new(Scheduler {
    tasks: BTreeMap::new(),
    reap: VecDeque::new(),
    next_id: 0,
});

/// Task that reaps dead tasks and restarts supervised services.
static SUPERVISOR: AtomicU32 = AtomicU32::new(NO_TASK);

pub fn current_id() -> TaskId {
    percpu::get().current
}

/// The task this CPU is running, without touching the table.
#[inline]
fn running() -> &'static Task {
    unsafe { &*percpu::get().current_task }
}

/// The top of the stack this code is running on, which is the stack a boot
/// context belongs to.
#[inline]
fn here() -> u64 {
    let sp: u64;
    unsafe {
        core::arch::asm!("mov {0}, rsp", out(reg) sp, options(nomem, nostack, preserves_flags))
    };
    sp | 15
}

/// The body of a CPU's idle task: halt, and look for work after every interrupt.
extern "C" fn idle_entry(_arg: u64) {
    interrupts::enable();
    loop {
        idle_wait();
    }
}

/// Set the calling CPU up for scheduling.
///
/// The per-CPU block and the table are changed together, with interrupts off: a
/// timer interrupt on this CPU in between would find `current_task` null and
/// dereference it. Returns the id of the CPU's idle task.
pub fn register_idle_cpu() -> TaskId {
    interrupts::without_interrupts(|| {
        let cpu = percpu::cpu_id();
        let mut s = SCHED.lock();
        // An application processor's boot code is a halt loop and nothing else,
        // so that context is its idle task.
        if cpu != 0 {
            let id = s.next_id;
            s.next_id += 1;
            let mut t = Task::boot(id, "idle", true, here());
            t.claim_cpu(cpu);
            // It is running before any switch: it *is* this CPU's context.
            t.set_running();
            let ptr: *mut Task = &mut *t;
            s.tasks.insert(id, t);
            drop(s);
            let pc = percpu::get();
            pc.current = id;
            pc.idle_task = id;
            pc.current_task = ptr;
            pc.idle_ptr = ptr;
            return id;
        }

        // The boot processor is the odd one out: its context is `kmain`, which
        // still starts the services and then waits out the test. A context cannot
        // be both that task and the CPU's fallback — `sleep` on the fallback has
        // nothing to switch to and would return at once — so the CPU gets an idle
        // task of its own and the boot context becomes an ordinary task that can
        // be queued, blocked and woken like any other.
        let idle_id = s.next_id;
        s.next_id += 1;
        let mut idle = Task::new_kernel(idle_id, "idle", idle_entry, 0);
        idle.is_idle = true;
        // Not running: this is the context the CPU falls back on, and the boot
        // processor is running `kmain`.
        idle.claim_cpu(cpu);
        let idle_ptr: *mut Task = &mut *idle;
        s.tasks.insert(idle_id, idle);

        let boot_id = s.next_id;
        s.next_id += 1;
        let mut boot = Task::boot(boot_id, "kmain", false, here());
        boot.claim_cpu(cpu);
        boot.set_running();
        let boot_ptr: *mut Task = &mut *boot;
        s.tasks.insert(boot_id, boot);
        drop(s);

        let pc = percpu::get();
        pc.current = boot_id;
        pc.current_task = boot_ptr;
        pc.idle_task = idle_id;
        pc.idle_ptr = idle_ptr;
        idle_id
    })
}

pub fn init() {
    let id = register_idle_cpu();
    klog!(
        "sched",
        "initialised on cpu 0 (idle task {id}, quantum {} ms), one run queue per cpu",
        QUANTUM_TICKS * 1000 / irq::TIMER_HZ
    );
}

pub fn spawn_kernel(name: &'static str, entry: extern "C" fn(u64), arg: u64) -> TaskId {
    spawn_kernel_with(name, entry, arg, |_| {})
}

/// [`spawn_kernel`], with the new task's capability table filled in first.
///
/// A kernel thread is not a service, so it has no capabilities unless somebody
/// gives it one — but the ones that are handed something to talk through do get
/// a real capability, and when such a thread exits the supervisor has to strip
/// it, which is only worth testing if a thread ever holds one.
pub fn spawn_kernel_with(
    name: &'static str,
    entry: extern "C" fn(u64),
    arg: u64,
    caps: impl FnOnce(&mut Task),
) -> TaskId {
    // The task is built before the table is locked, and that is not tidiness: a
    // task's stack comes from the kernel heap, and a heap that has to grow
    // announces the new pages to the other CPUs and waits for them to say they
    // have seen it. An interrupt handler on another CPU that wants this table
    // holds it with interrupts off, so it could never answer — and the two would
    // wait for each other for good. Nothing that can wait for another CPU may
    // run under this lock, which means nothing that can allocate.
    let id = table(|s| {
        let id = s.next_id;
        s.next_id += 1;
        id
    });
    let mut t = Task::new_kernel(id, name, entry, arg);
    caps(&mut t);
    let ptr: *mut Task = &mut *t;
    table(|s| {
        s.tasks.insert(id, t);
    });
    // On the CPU that spawned it, which is the one whose cache holds what it is
    // about to touch.
    enqueue_from("spawn", percpu::cpu_id(), ptr);
    id
}

/// Take the next task id without publishing a task. A ring-3 service reserves
/// its id first so it can finish the address space (boot info page) before the
/// task becomes reachable by any CPU.
pub fn reserve_task_id() -> TaskId {
    table(|s| {
        let id = s.next_id;
        s.next_id += 1;
        id
    })
}

/// Publish a task whose id came from [`reserve_task_id`]: insert it and make
/// it runnable.
pub fn publish_task(mut t: Box<Task>, id: TaskId) {
    t.id = id;
    t.set_state(State::Ready);
    let ptr: *mut Task = &mut *t;
    table(|s| {
        s.tasks.insert(id, t);
    });
    enqueue_from("publish", percpu::cpu_id(), ptr);
}

pub fn set_supervisor(id: TaskId) {
    SUPERVISOR.store(id, Ordering::Relaxed);
}

pub fn with_task<R>(id: TaskId, f: impl FnOnce(&mut Task) -> R) -> Option<R> {
    table(|s| s.tasks.get_mut(&id).map(|t| f(t)))
}

pub fn with_current<R>(f: impl FnOnce(&mut Task) -> R) -> R {
    with_task(current_id(), f).expect("current task vanished")
}

/// Whether it is safe to switch away from whatever this CPU is running.
///
/// It is safe from ordinary kernel code, where the switch saves a stack pointer
/// into a call frame and returning to it later is a plain `ret`. It is *not* safe
/// from inside a trap that was taken from ring 3: the saved pointer lands in the
/// middle of the trap's frame, and the `ret` at the end of the switch would jump
/// to whatever the stub happened to save there. So a tick or a reschedule request
/// that arrives while a task is in user code is remembered and acted on at the
/// next syscall, which is ordinary kernel code.
pub fn may_switch_here() -> bool {
    !percpu::get().in_user_trap
}

/// Act on a reschedule request that arrived while this CPU was in user code. The
/// caller has to be in ordinary kernel code — a syscall handler will do.
pub fn take_deferred_resched() -> bool {
    percpu::get().resched.swap(false, Ordering::AcqRel)
}

/// Whether a pointer could be a `Task` or a task stack: something the kernel
/// heap handed out. A pointer from anywhere else — a static, a page table, an
/// integer that used to be an id — is what a queue hands to the CPU when the
/// bookkeeping has gone wrong, and the CPU then executes the scheduler's own
/// tables.
fn from_kernel_heap(p: *const u8) -> bool {
    let a = p as usize;
    let lo = crate::mm::heap::HEAP_START as usize;
    a >= lo && a < lo + 1024 * 1024 * 1024
}

/// Report a broken invariant, the first few times only.
///
/// A print takes locks, and a lock is often what a stuck machine is holding, so
/// this stays quiet after the first handful: a scheduler that logs every
/// violation of a rule it breaks a thousand times a second logs nothing at all.
static REPORTS: AtomicU32 = AtomicU32::new(0);

fn report(what: &str, detail: alloc::string::String) {
    let n = REPORTS.fetch_add(1, Ordering::Relaxed);
    if n < 6 {
        klog!("sched", "INVARIANT BROKEN ({n}): {what}: {detail}");
    }
}

fn task_ptr_ok(task: *mut Task) -> bool {
    if task.is_null() || !from_kernel_heap(task as *const u8) {
        report(
            "task pointer is not from the kernel heap",
            alloc::format!("{:?}", task as usize),
        );
        return false;
    }
    let t = unsafe { &*task };
    let sp = t.ctx_sp as usize;
    // A task runs on its own stack, except a ring-3 task, which is entered on
    // the user stack it was left on and reached through the HHDM.
    if sp != 0
        && !from_kernel_heap(sp as *const u8)
        && !(sp >= 0xffff_8000_0000_0000 && sp < 0xffff_9000_0000_0000)
    {
        report(
            "task context stack is not a stack",
            alloc::format!(
                "task {} '{}' ctx_sp {sp:#x} place {} on_cpu {:?} state {:?}",
                t.id,
                t.name,
                t.where_is(),
                t.cpu(),
                t.state()
            ),
        );
        return false;
    }
    true
}

/// The word at the end of a suspended task's frame: where `switch_context`'s
/// final `ret` will take the CPU. `SavedFrame` is
/// `[r15, r14, r13, r12, rbx, rbp, rip]`, so the return address is 48 bytes in.
const RIP_SLOT: u64 = 48;

/// Check that a task's frame really has a return address in it.
///
/// A task that is suspended where returning is not a plain `ret` — inside a trap
/// taken from ring 3, say — resumes at whatever the frame happens to hold, which
/// the CPU then reports as a fault at an address nobody wrote code to. Saying so
/// here, where the task is still intact, is worth the two loads.
fn check_rip(t: &Task, sp: usize) {
    if sp == 0 || !from_kernel_heap(sp as *const u8) {
        return;
    }
    let rip = unsafe {
        core::ptr::read_unaligned((sp as *const u8).add(RIP_SLOT as usize) as *const u64)
    };
    let fresh = rip == crate::arch::x86_64::context::task_trampoline as *const () as u64;
    // Where the kernel's own statics begin. Code is below that and data is
    // above it, so a "return address" that points into the run queue arrays is
    // exactly as wrong as one that points at address 4 — and only the first is
    // even in the kernel's address range.
    let data_start = core::ptr::addr_of!(SUPERVISOR) as u64;
    let kernel_code = rip != 0 && rip < data_start && rip >= 0xffff_8000_0000_0000;
    // A ring-3 task is entered on the address it was last running at, or on its
    // entry point, both of which are in the user half.
    let user_code = t.is_user() && rip != 0 && rip < 0x0000_8000_0000_0000;
    if !(fresh || kernel_code || user_code) {
        report(
            "task frame has no return address in it",
            alloc::format!(
                "task {} '{}' ctx_sp {sp:#x} would jump to {rip:#x} (state {:?} on_cpu {:?})",
                t.id,
                t.name,
                t.state(),
                t.cpu()
            ),
        );
    }
}

/// The task table, with interrupts off.
///
/// Every lock in the scheduler is taken this way, and that is the whole point:
/// a lock held with interrupts on can be interrupted on the same CPU, and the
/// interrupt handler will then wait for the very lock its own thread is holding —
/// on one CPU, with interrupts off, forever. It is also what lets the kernel
/// wait for another CPU (a TLB shootdown has to be acknowledged) without the
/// other CPUs being stuck behind a lock this one is holding.
fn table<R>(f: impl FnOnce(&mut Scheduler) -> R) -> R {
    interrupts::without_interrupts(|| {
        let mut s = SCHED.lock();
        f(&mut s)
    })
}

/// A CPU's run queue, with interrupts off, for the same reason.
fn run_queue_locked<R>(cpu: u32, f: impl FnOnce(&mut VecDeque<*mut Task>) -> R) -> R {
    interrupts::without_interrupts(|| {
        let mut q = run_queue(cpu).inner.lock();
        f(&mut q)
    })
}

/// Put a task on a CPU's run queue.
///
/// The caller must already own the task: it has to be running nowhere and be
/// ready, or the queue would hand the same task to two CPUs.
fn enqueue_from(from: &'static str, cpu: u32, task: *mut Task) {
    if !task_ptr_ok(task) {
        return;
    }
    let t = unsafe { &*task };
    // A task that is running anywhere is not up for grabs: it is on a stack, and
    // a second CPU to switch to it would run two tasks on one stack.
    if let Some(c) = t.cpu()
        && c != percpu::cpu_id()
    {
        report(
            "queueing a task that is already running",
            alloc::format!(
                "{from} puts task {} '{}' on cpu {cpu} while cpu {c} is running it (state {:?})",
                t.id,
                t.name,
                t.state()
            ),
        );
        return;
    }
    if !t.move_to(task::NOWHERE, task::QUEUED) {
        report(
            "task was not nowhere when it was queued",
            alloc::format!(
                "{from} queues task {} '{}', found in place {} (on cpu {:?}, state {:?})",
                t.id,
                t.name,
                t.where_is(),
                t.cpu(),
                t.state()
            ),
        );
        return;
    }
    run_queue_locked(cpu, |q| q.push_back(task));
    run_queue(cpu).queued.fetch_add(1, Ordering::Relaxed);
}

/// Take a task off a CPU's run queue, or `None` if it is empty.
fn dequeue(cpu: u32) -> Option<*mut Task> {
    let mut task = run_queue_locked(cpu, |q| q.pop_front());
    // A pointer that is not a task is thrown away rather than returned: a queue
    // that hands one to the CPU is a queue that will be run.
    while let Some(ptr) = task {
        if task_ptr_ok(ptr) {
            let t = unsafe { &*ptr };
            if !t.move_to(task::QUEUED, task::NOWHERE) {
                report(
                    "task was not queued when it was taken off a queue",
                    alloc::format!(
                        "cpu {cpu} takes task {} '{}' out of its queue, found in place {}",
                        t.id,
                        t.name,
                        t.where_is()
                    ),
                );
            }
            run_queue(cpu).queued.fetch_sub(1, Ordering::Relaxed);
            return task;
        }
        report(
            "a run queue held something that is not a task",
            alloc::format!("cpu {cpu} dropped {ptr:?}"),
        );
        run_queue(cpu).queued.fetch_sub(1, Ordering::Relaxed);
        task = run_queue_locked(cpu, |q| q.pop_front());
    }
    None
}

/// Take one task off somebody else's queue.
///
/// The cursor moves on every attempt so that CPUs which are all looking for work
/// start at different places, and only a bounded number of queues is tried: a CPU
/// with nothing to do is better off going idle and being kicked than trying every
/// queue in the machine to find one that is nearly as empty as this one.
fn steal(from: u32) -> Option<*mut Task> {
    const MAX_TRIES: u32 = 3;
    let pc = percpu::get();
    let online = percpu::count() as u32;
    if online < 2 {
        return None;
    }
    for _ in 0..MAX_TRIES.min(online - 1) {
        let start = pc.steal_cursor;
        pc.steal_cursor = start.wrapping_add(1) % online;
        let mut cpu = start % online;
        let mut tries = 0;
        while tries < online - 1 {
            if cpu != from {
                if let Some(t) = dequeue(cpu) {
                    pc.steals += 1;
                    return Some(t);
                }
            }
            cpu = (cpu + 1) % online;
            tries += 1;
        }
    }
    None
}

/// Pick the next task and switch to it. Must be called with interrupts disabled.
pub fn schedule() {
    let pc = percpu::get();
    let cpu = pc.cpu_id;
    let cur = running();

    if !task_ptr_ok(pc.current_task) {
        report(
            "this CPU's current task pointer is not a task",
            alloc::format!(
                "cpu {cpu} current id {} pointer {:#x}",
                pc.current,
                pc.current_task as usize
            ),
        );
    }
    let next: &'static Task = match dequeue(cpu) {
        Some(ptr) => unsafe { &*ptr },
        None => match steal(cpu) {
            Some(ptr) => unsafe { &*ptr },
            None => {
                // Nothing anywhere. Keep running what we are running if it is
                // still runnable, and otherwise fall back to this CPU's idle task.
                if cur.state() == State::Running {
                    return;
                }
                unsafe { &*pc.idle_ptr }
            }
        },
    };

    if core::ptr::eq(next, cur as *const Task) {
        next.reset_quantum();
        return;
    }

    // `on_cpu` is the task's own record of who is running it, and it is the one
    // place that does not depend on reading somebody else's memory. A task
    // another CPU has claimed cannot be started here: two CPUs on one stack is
    // what every other symptom of this comes from.
    if let Some(owner) = next.cpu()
        && owner != cpu
    {
        report(
            "starting a task that another cpu has claimed",
            alloc::format!(
                "cpu {cpu} takes task {} '{}' (state {:?}, queued {}) from cpu {owner}",
                next.id,
                next.name,
                next.state(),
                next.is_queued()
            ),
        );
    }

    let prev = cur as *const Task as *mut Task;
    // A task that is being switched away from while it was still running goes
    // back on the queue — as Ready, which is what `finish_switch` looks for once
    // this CPU has left its stack. A task that blocked or exited on purpose is
    // not running any more, and is not requeued.
    if cur.state() == State::Running {
        cur.set_state(State::Ready);
    }
    pc.prev_pending = cur.id;
    pc.prev_task = prev;
    next.set_state(State::Running);
    if !next.move_to(task::NOWHERE, task::RUNNING) {
        report(
            "task was not nowhere when a cpu started running it",
            alloc::format!(
                "cpu {cpu} starts task {} '{}', found in place {} (state {:?})",
                next.id,
                next.name,
                next.where_is(),
                next.state()
            ),
        );
    }
    next.claim_cpu(cpu);
    next.reset_quantum();
    pc.current = next.id;
    pc.current_task = next as *const Task as *mut Task;
    pc.switches += 1;

    let kstack_top = next.kstack_top();
    if kstack_top != 0 {
        gdt::set_kernel_stack(VirtAddr::new(kstack_top));
        pc.kstack_top = kstack_top;
    }
    let cr3 = next
        .addr_space
        .as_ref()
        .map(|a| a.cr3())
        .unwrap_or_else(crate::mm::vmm::kernel_pml4);
    if Cr3::read().0 != cr3 {
        unsafe { Cr3::write(cr3, Cr3Flags::empty()) };
    }
    check_rip(next, next.ctx_sp as usize);
    let prev_sp_ptr = unsafe { core::ptr::addr_of_mut!((*prev).ctx_sp) };
    let next_sp = next.ctx_sp;
    {
        // The value being switched to has to be a stack pointer of some task, and
        // the word at the top of it has to be a return address. A task whose
        // context was never set up — or one whose context was overwritten by
        // something else — is caught here rather than by the CPU two instructions
        // later, on whatever address the task struct happened to start with.
        let a = next_sp as usize;
        // A task's own stack is in the kernel heap; a ring-3 task is entered on
        // its own stack, seen through the HHDM.
        let ok = (a >= crate::mm::heap::HEAP_START as usize
            && a < crate::mm::heap::HEAP_START as usize + 1024 * 1024 * 1024)
            || (a >= 0xffff_8000_0000_0000 && a < 0xffff_9000_0000_0000);
        if !ok {
            klog!(
                "sched",
                "BAD CONTEXT: switching to task {} '{}' with context stack {:#x}",
                next.id,
                next.name,
                a
            );
        }
    }
    unsafe { context::switch_context(prev_sp_ptr, next_sp) };
    finish_switch();
}

/// Runs on the new context right after a switch: the previous task has left
/// its stack, so it may now be picked up by any CPU.
///
/// It goes on *this* CPU's queue rather than whichever CPU spawned it, because
/// its context was just written by this CPU's stack switch and is warmest here.
pub extern "C" fn finish_switch() {
    let pc = percpu::get();
    let prev = pc.prev_pending;
    let ptr = pc.prev_task;
    pc.prev_pending = NO_TASK;
    pc.prev_task = core::ptr::null_mut();
    if prev == NO_TASK || ptr.is_null() {
        return;
    }
    // Safety: `prev_task` was set by this CPU immediately before the switch, so
    // the task is still in the table and still owned by nobody.
    let task = unsafe { &*ptr };
    if !task.move_to(task::RUNNING, task::NOWHERE) {
        report(
            "task was not running when a cpu left it",
            alloc::format!(
                "cpu {} finishes switching away from task {} '{}', found in place {} (state {:?})",
                percpu::cpu_id(),
                task.id,
                task.name,
                task.where_is(),
                task.state()
            ),
        );
    }
    task.release_cpu();
    if task.state() == State::Ready && !task.is_idle {
        enqueue_from("finish_switch", percpu::cpu_id(), ptr);
    }
}

pub fn yield_now() {
    // A yield means "somebody else next", which is what switching away from a
    // running task already does: it goes to the back of this CPU's own queue.
    interrupts::without_interrupts(schedule);
}

/// What a CPU's idle loop runs.
///
/// After an interrupt the CPU looks for work itself rather than waiting for
/// somebody to wake it: that is what makes a per-CPU run queue worth having,
/// because a CPU with an empty queue of its own can take one off a neighbour
/// instead of sitting idle next to a busy one. Interrupts go back on for the
/// `hlt`, since a `schedule` that finds nothing returns rather than switching.
pub fn idle_wait() {
    x86_64::instructions::hlt();
    interrupts::without_interrupts(schedule);
    interrupts::enable();
}

pub fn sleep_ms(ms: u64) {
    let until = irq::ticks() + (ms * irq::TIMER_HZ as u64).div_ceil(1000).max(1);
    interrupts::without_interrupts(|| {
        let me = percpu::get().current;
        with_current(|t| {
            t.set_state(State::Sleeping);
            t.sleep_until = until;
        });
        // On this CPU's list: its own tick is the one that will notice.
        percpu::get().sleepers.lock().push((me, until));
        schedule();
    });
}

/// Mark the current task blocked. Interrupts must already be disabled; the
/// caller follows up with `schedule()` after releasing its own locks.
pub fn mark_blocked() {
    with_current(|t| t.set_state(State::Blocked));
}

pub fn wake(id: TaskId) {
    let (ptr, target, still_on_a_cpu) = table(|s| {
        let Some(t) = s.tasks.get_mut(&id) else {
            return (core::ptr::null_mut(), 0, false);
        };
        if !matches!(t.state(), State::Blocked | State::Sleeping) {
            return (core::ptr::null_mut(), 0, false);
        }
        t.set_state(State::Ready);
        // A task that is still on a CPU is not this CPU's to queue. It is on
        // that CPU's stack until that CPU finishes switching away from it, and
        // the switch itself will notice that the task is Ready now and put it
        // back on a run queue. Queuing it here as well is how one task ends up
        // on two queues and gets run by two CPUs at once.
        let owner = t.cpu();
        if owner.is_some() {
            return (core::ptr::null_mut(), 0, true);
        }
        // A task goes back to the CPU it last ran on if that CPU is still here:
        // its address space and its data are warmest in that CPU's cache. A
        // sleeping task has no such affinity — it slept on some CPU and its cache
        // is cold wherever you put it.
        let last = t.last_cpu();
        let cpu = match last {
            Some(c) if c < percpu::count() as u32 => c,
            _ => percpu::cpu_id(),
        };
        // The `Box<Task>` is stable in the table, and this task is on no queue
        // and no CPU, so the pointer stays valid until it is reaped.
        ((&mut **t as *mut Task), cpu, false)
    });
    if still_on_a_cpu {
        return;
    }
    if ptr.is_null() {
        return;
    }
    // A task that was on a sleeper list has to come off it, whichever CPU's list
    // it is on, or the list would hold a task that is already running.
    unsleep(id);
    enqueue_from("wake", target, ptr);
    // Somebody on the target CPU may be in `hlt` with nothing to do.
    kick_idle_cpu(target);
}

/// Whether a task is on any CPU's sleeper list.
fn sleeping_on(id: TaskId) -> bool {
    for cpu in 0..percpu::count() as u32 {
        if let Some(pc) = percpu::by_id(cpu as usize) {
            let found =
                interrupts::without_interrupts(|| pc.sleepers.lock().iter().any(|(t, _)| *t == id));
            if found {
                return true;
            }
        }
    }
    false
}

/// Take a task off whichever CPU's sleeper list it is on.
fn unsleep(id: TaskId) {
    // This CPU's own list first, and it is not skipped: a task can sleep and then
    // be woken on the same CPU, and leaving its entry behind would put a task
    // that is already running on a list that the tick will try to wake again.
    interrupts::without_interrupts(|| {
        percpu::get().sleepers.lock().retain(|(t, _)| *t != id);
    });
    for cpu in 0..percpu::count() as u32 {
        if cpu == percpu::cpu_id() {
            continue;
        }
        // Another CPU's list, with its interrupts off: it may be sweeping that
        // list from its own tick handler right now.
        interrupts::without_interrupts(|| {
            if let Some(pc) = percpu::by_id(cpu as usize) {
                pc.sleepers.lock().retain(|(t, _)| *t != id);
            }
        });
    }
}

/// Nudge an idle CPU out of `hlt`. Fire and forget: if the CPU is busy after all,
/// the task is on its run queue and it will pick the task up when it next runs
/// out of work.
fn kick_idle_cpu(cpu: u32) {
    let me = percpu::cpu_id();
    if cpu == me {
        return;
    }
    for candidate in [cpu, me] {
        if let Some(pc) = percpu::by_id(candidate as usize)
            && pc.current == pc.idle_task
        {
            ipi::kick(candidate as usize);
            return;
        }
    }
}

pub fn exit_current(code: i64) -> ! {
    interrupts::disable();
    let id = current_id();
    {
        let mut s = SCHED.lock();
        if let Some(t) = s.tasks.get_mut(&id) {
            t.set_state(State::Dead);
            t.exit_code = Some(code);
        }
        s.reap.push_back(id);
    }
    let sup = SUPERVISOR.load(Ordering::Relaxed);
    if sup != NO_TASK {
        wake(sup);
    }
    // The dead task is on nobody's queue, so this cannot come back to it.
    schedule();
    unreachable!("dead task was scheduled");
}

pub extern "C" fn thread_exit_hook() -> ! {
    exit_current(0)
}

/// Pop one dead task that has fully left its CPU, for the supervisor to free.
///
/// A dead task's authority goes with it, and it is taken while the task is still
/// reachable: a task that has exited but not been reaped yet is still something
/// the hardware can be pointed at, and "it is dead" is not the same as "it can
/// do nothing".
pub fn take_dead() -> Option<(Box<Task>, usize)> {
    table(|s| {
        // A task is only free once it is nowhere: off every CPU, off every run
        // queue and off every sleeper list. Reaping it while a queue still holds
        // a pointer to it hands that pointer's memory to the next allocation, and
        // the next task to be switched to is then somebody else's task.
        let pos = s.reap.iter().position(|id| {
            s.tasks
                .get(id)
                .is_some_and(|t| t.cpu().is_none() && !t.is_queued() && !sleeping_on(*id))
        })?;
        let id = s.reap.remove(pos)?;
        if let Some(t) = s.tasks.get(&id) {
            if t.is_queued() {
                klog!(
                    "sched",
                    "QUEUED AND DEAD: task {} '{}' is being reaped while still on a run queue",
                    t.id,
                    t.name
                );
            }
        }
        let mut taken = 0;
        if let Some(t) = s.tasks.get_mut(&id) {
            // Only the table is touched here, so this stays inside the lock it
            // is already holding.
            while let Some(slot) =
                (0..t.caps.slot_count()).find(|s| t.caps.get(*s as u32).is_some())
            {
                if t.caps.revoke(slot as u32).is_some() {
                    taken += 1;
                }
            }
        }
        s.tasks.remove(&id).map(|t| (t, taken))
    })
}

/// The timer interrupt: this CPU's own sleepers, this CPU's own quantum.
///
/// Nothing here takes a lock another CPU might be holding for long, which is the
/// whole point of per-CPU sleeper lists: the old code walked every task in the
/// machine on every tick of every CPU, under one lock.
pub fn on_tick() {
    // This runs from the timer interrupt with interrupts off, on the CPU whose
    // quantum is being counted down, and it is the one piece of scheduler code
    // every CPU has to be able to get through promptly: a CPU stuck in here with
    // interrupts off cannot answer a TLB shootdown, so whoever is sending one
    // would wait for good. That rules out anything that allocates, and anything
    // that prints, and it is why the sleeper list is swept in place into arrays
    // on this stack rather than into a `Vec`.
    const MAX_DUE: usize = 64;
    let now = irq::ticks();
    let pc = percpu::get();
    let cur = running();

    // Wake whatever of this CPU's sleepers is due. The list is compacted in
    // place: entries that are not due yet move to the front, and the ones that
    // are are named in `expired`.
    let mut expired = [percpu::NO_TASK; MAX_DUE];
    let mut n_expired = 0usize;
    {
        let mut sleepers = pc.sleepers.lock();
        let mut w = 0;
        for r in 0..sleepers.len() {
            let entry = sleepers[r];
            if entry.1 > now {
                sleepers[w] = entry;
                w += 1;
            } else if n_expired < MAX_DUE {
                expired[n_expired] = entry.0;
                n_expired += 1;
            } else {
                // More sleepers than one tick can name: leave the rest for the
                // next one rather than losing them.
                sleepers[w] = entry;
                w += 1;
            }
        }
        sleepers.truncate(w);
    }

    // The tasks are collected under the table lock and queued after it, because
    // the queue lock is a different lock and this code does not need both.
    let mut due = [core::ptr::null_mut(); MAX_DUE];
    let mut n_due = 0usize;
    if n_expired > 0 {
        table(|s| {
            for i in 0..n_expired {
                let Some(t) = s.tasks.get_mut(&expired[i]) else {
                    continue;
                };
                if t.state() != State::Sleeping {
                    continue;
                }
                t.set_state(State::Ready);
                // Same as `wake`: a sleeper that is somehow still on a CPU is
                // that CPU's to requeue, not ours.
                if t.cpu().is_some() {
                    continue;
                }
                due[n_due] = &mut **t as *mut Task;
                n_due += 1;
            }
        });
    }
    let woken_any = n_due > 0;
    for i in 0..n_due {
        enqueue_from("tick", percpu::cpu_id(), due[i]);
    }

    let preempt = {
        let left = cur.quantum_left().saturating_sub(1);
        cur.set_quantum(left);
        left == 0 || cur.is_idle
    };
    if !preempt {
        return;
    }
    if !may_switch_here() {
        // Interrupted in user code: remember it, and let the next syscall switch.
        pc.resched.store(true, Ordering::Release);
        return;
    }
    if woken_any || !run_queue(percpu::cpu_id()).is_empty() {
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
    table(|s| s.tasks.len())
}

pub fn dump() {
    // A lock held across a print is a lock held across an allocation, and an
    // allocation that has to grow the heap waits for the other CPUs to answer a
    // shootdown — which they cannot do while this CPU holds the table. So the
    // lines are copied out first, and printed with nothing held.
    const MAX: usize = 64;
    let mut rows: [(TaskId, &'static str, State, Option<u32>, bool, bool); MAX] =
        [(0, "", State::Dead, None, false, false); MAX];
    let mut n = 0;
    table(|s| {
        for (id, t) in s.tasks.iter() {
            if n == MAX {
                break;
            }
            rows[n] = (*id, t.name, t.state(), t.cpu(), t.is_idle, t.is_user());
            n += 1;
        }
    });
    for (id, name, state, cpu, is_idle, user) in rows.into_iter().take(n) {
        klog!(
            "sched",
            "  #{:<3} {:<12} {:?}{}{}{}",
            id,
            name,
            state,
            if user { " (ring3)" } else { "" },
            if is_idle { " (idle)" } else { "" },
            match cpu {
                Some(c) => alloc::format!(" on cpu {}", c),
                None => alloc::string::String::new(),
            }
        );
    }
}
