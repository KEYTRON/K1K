//! Per-CPU state, reached through the GS base. In kernel mode GS_BASE points
//! at this CPU's `PerCpu`; while a task runs in ring 3 the two GS bases are
//! swapped (`swapgs` on every entry/exit), so user code can never observe or
//! clobber the kernel pointer.

use alloc::boxed::Box;
use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use x86_64::VirtAddr;
use x86_64::registers::model_specific::{GsBase, KernelGsBase};
use x86_64::structures::tss::TaskStateSegment;

use crate::sched::task::TaskId;

/// Logical processors one socket can hold.
///
/// The ceiling taken for the kernel: the biggest x86 part available today has 256
/// cores and 512 threads (AMD EPYC 9996, Venice), so anything smaller is a cap
/// the hardware cannot justify. What is actually *usable* at this size is a
/// separate question, and a separate line in the roadmap: the local apic id
/// becomes 32 bits, the firmware describes those processors with MADT entry
/// type 9, and starting them needs a wake-up sequence this kernel does not yet
/// do. Under QEMU the tests run with a handful.
pub const MAX_CPUS: usize = 512;
pub const NO_TASK: TaskId = u32::MAX;

/// Offsets used by the syscall entry stub (`syscall.rs`).
pub const OFF_KSTACK_TOP: usize = 8;
pub const OFF_USER_RSP: usize = 16;

#[repr(C)]
pub struct PerCpu {
    /// Points at itself so `gs:[0]` yields the struct address.
    self_ptr: *const PerCpu,
    /// Top of the running task's kernel stack (syscall entry loads rsp from here).
    pub kstack_top: u64,
    /// Scratch slot for the user rsp during syscall entry.
    pub user_rsp: u64,
    pub cpu_id: u32,
    pub lapic_id: u32,
    pub current: TaskId,
    pub idle_task: TaskId,
    /// The running task, as a pointer, so the scheduler's hot path never has to
    /// look a task up in the table behind a lock.
    pub current_task: *mut crate::sched::task::Task,
    /// Task we just switched away from; `sched::finish_switch` requeues it.
    pub prev_pending: TaskId,
    /// That same task as a pointer, handed to `sched::finish_switch` by the new
    /// task before it runs a single instruction.
    pub prev_task: *mut crate::sched::task::Task,
    /// This CPU's fallback context, used when nothing is runnable anywhere.
    pub idle_ptr: *mut crate::sched::task::Task,
    /// Tasks waiting for a deadline to pass, with the tick they are due at.
    pub sleepers: spin::Mutex<alloc::vec::Vec<(TaskId, u64)>>,
    /// Where the next steal attempt starts, so two CPUs do not both try the same
    /// empty queue.
    pub steal_cursor: u32,
    /// How many times work was taken off another CPU's queue.
    pub steals: u64,
    pub switches: u64,
    pub tss: *mut TaskStateSegment,
    /// Work handed to this CPU by an IPI; see [`super::ipi`].
    pub ipi_slot: spin::Mutex<Option<super::ipi::Pending>>,
    /// Bumped when a request is published, acknowledged when it is done.
    pub ipi_seq: AtomicU32,
    pub ipi_done: AtomicU32,
    /// Set when a reschedule request arrives, cleared when it is acted on.
    pub resched: AtomicBool,
    /// Set while a trap is being handled that was taken from ring 3.
    ///
    /// A task interrupted in user code cannot be suspended from inside the trap
    /// and resumed by the context switch: the switch saves a stack pointer into
    /// the middle of the trap's frame, and the `ret` that ends the switch would
    /// jump to the trap stub's saved frame pointer instead of a return address.
    /// The scheduler checks this and leaves such a task alone until it is back in
    /// ordinary kernel code, where returning is a normal `ret`.
    pub in_user_trap: bool,
    /// TSC reading taken on the same tick as every other CPU's, and the tick it
    /// was taken on. Two CPUs that read the counter seconds apart in real time
    /// must still agree once the readings are converted back, which is what
    /// makes the TSC usable as a clock every core can read.
    pub tsc_sample: AtomicU64,
    /// The processor id `rdtscp` reported with that sample.
    pub tsc_aux: AtomicU32,
}

const _: () = {
    assert!(core::mem::offset_of!(PerCpu, kstack_top) == OFF_KSTACK_TOP);
    assert!(core::mem::offset_of!(PerCpu, user_rsp) == OFF_USER_RSP);
};

static CPUS: [AtomicPtr<PerCpu>; MAX_CPUS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_CPUS];
static CPU_COUNT: AtomicUsize = AtomicUsize::new(0);

static mut BSP: PerCpu = PerCpu::empty();

impl PerCpu {
    const fn empty() -> Self {
        Self {
            self_ptr: core::ptr::null(),
            kstack_top: 0,
            user_rsp: 0,
            cpu_id: 0,
            lapic_id: 0,
            current: NO_TASK,
            idle_task: NO_TASK,
            current_task: core::ptr::null_mut(),
            prev_pending: NO_TASK,
            prev_task: core::ptr::null_mut(),
            idle_ptr: core::ptr::null_mut(),
            sleepers: spin::Mutex::new(alloc::vec::Vec::new()),
            steal_cursor: 0,
            steals: 0,
            switches: 0,
            tss: core::ptr::null_mut(),
            ipi_slot: spin::Mutex::new(None),
            ipi_seq: AtomicU32::new(0),
            ipi_done: AtomicU32::new(0),
            resched: AtomicBool::new(false),
            in_user_trap: false,
            tsc_sample: AtomicU64::new(0),
            tsc_aux: AtomicU32::new(u32::MAX),
        }
    }
}

/// Install `pc` as this CPU's per-CPU block and publish it in the CPU table.
unsafe fn install(pc: *mut PerCpu) {
    unsafe {
        (*pc).self_ptr = pc;
        GsBase::write(VirtAddr::from_ptr(pc));
        KernelGsBase::write(VirtAddr::zero());
        CPUS[(*pc).cpu_id as usize].store(pc, Ordering::Release);
    }
    CPU_COUNT.fetch_add(1, Ordering::AcqRel);
}

/// Bootstrap processor: static storage, usable before the heap exists.
pub fn init_bsp(lapic_id: u32, tss: *mut TaskStateSegment) {
    unsafe {
        let pc = core::ptr::addr_of_mut!(BSP);
        (*pc).cpu_id = 0;
        (*pc).lapic_id = lapic_id;
        (*pc).tss = tss;
        install(pc);
    }
}

/// Allocate the block for an application processor (called on the BSP).
pub fn alloc_ap(cpu_id: u32, lapic_id: u32, tss: *mut TaskStateSegment) -> *mut PerCpu {
    let mut pc = Box::new(PerCpu::empty());
    pc.cpu_id = cpu_id;
    pc.lapic_id = lapic_id;
    pc.tss = tss;
    Box::leak(pc)
}

/// Called on the AP itself once it runs on its own GDT.
pub unsafe fn install_ap(pc: *mut PerCpu) {
    unsafe { install(pc) };
}

#[inline]
pub fn get() -> &'static mut PerCpu {
    let p: *mut PerCpu;
    unsafe {
        asm!("mov {}, gs:[0]", out(reg) p, options(nomem, nostack, preserves_flags));
        &mut *p
    }
}

#[inline]
pub fn cpu_id() -> u32 {
    get().cpu_id
}

pub fn count() -> usize {
    CPU_COUNT.load(Ordering::Acquire)
}

pub fn by_id(id: usize) -> Option<&'static PerCpu> {
    let p = CPUS.get(id)?.load(Ordering::Acquire);
    (!p.is_null()).then(|| unsafe { &*p })
}
