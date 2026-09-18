//! Per-CPU state, reached through the GS base. In kernel mode GS_BASE points
//! at this CPU's `PerCpu`; while a task runs in ring 3 the two GS bases are
//! swapped (`swapgs` on every entry/exit), so user code can never observe or
//! clobber the kernel pointer.

use alloc::boxed::Box;
use core::arch::asm;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use x86_64::VirtAddr;
use x86_64::registers::model_specific::{GsBase, KernelGsBase};
use x86_64::structures::tss::TaskStateSegment;

use crate::sched::task::TaskId;

pub const MAX_CPUS: usize = 64;
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
    /// Task we just switched away from; `sched::finish_switch` requeues it.
    pub prev_pending: TaskId,
    pub switches: u64,
    pub tss: *mut TaskStateSegment,
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
            prev_pending: NO_TASK,
            switches: 0,
            tss: core::ptr::null_mut(),
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
