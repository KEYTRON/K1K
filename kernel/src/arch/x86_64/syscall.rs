//! `syscall`/`sysret` fast path. The entry stub swaps to the current task's
//! kernel stack, saves the user return state, and calls the dispatcher.

use core::arch::naked_asm;
use x86_64::VirtAddr;
use x86_64::registers::model_specific::{Efer, EferFlags, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;

use super::gdt;

#[unsafe(no_mangle)]
static mut USER_RSP_SCRATCH: u64 = 0;

pub fn init() {
    let sel = gdt::selectors();
    unsafe {
        Efer::update(|f| f.insert(EferFlags::SYSTEM_CALL_EXTENSIONS));
        Star::write(
            sel.user_code,
            sel.user_data,
            sel.kernel_code,
            sel.kernel_data,
        )
        .expect("GDT layout incompatible with sysret");
        LStar::write(VirtAddr::new(syscall_entry as *const () as u64));
        SFMask::write(RFlags::INTERRUPT_FLAG | RFlags::DIRECTION_FLAG | RFlags::TRAP_FLAG);
    }
}

/// User ABI: rax = number, args rdi, rsi, rdx, r10. Kernel dispatcher ABI:
/// `dispatch(nr, a0, a1, a2, a3)` in rdi, rsi, rdx, rcx, r8.
#[unsafe(naked)]
pub unsafe extern "C" fn syscall_entry() {
    naked_asm!(
        "mov [rip + {scratch}], rsp",
        "mov rsp, [rip + {kstack}]",
        "push qword ptr [rip + {scratch}]",
        "push rcx",
        "push r11",
        "push rax",
        "mov r8, r10",
        "mov rcx, rdx",
        "mov rdx, rsi",
        "mov rsi, rdi",
        "mov rdi, rax",
        "sti",
        "call {dispatch}",
        "cli",
        "add rsp, 8",
        "pop r11",
        "pop rcx",
        "pop rsp",
        "sysretq",
        scratch = sym USER_RSP_SCRATCH,
        kstack = sym crate::sched::CURRENT_KSTACK_TOP,
        dispatch = sym crate::syscall::dispatch,
    );
}
