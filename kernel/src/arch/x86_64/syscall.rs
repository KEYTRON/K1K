//! `syscall`/`sysret` fast path. The entry stub swaps to the kernel GS base,
//! moves to the current task's kernel stack, saves the user return state and
//! calls the dispatcher.

use core::arch::naked_asm;
use x86_64::VirtAddr;
use x86_64::registers::model_specific::{Efer, EferFlags, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;

use super::{gdt, percpu};

/// Program the syscall MSRs on the calling CPU.
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
/// `dispatch(nr, a0, a1, a2, a3)` in rdi, rsi, rdx, rcx, r8. Every register
/// except rax/rcx/r11 is preserved for the caller.
#[unsafe(naked)]
pub unsafe extern "C" fn syscall_entry() {
    naked_asm!(
        "swapgs",
        "mov gs:[{user_rsp}], rsp",
        "mov rsp, gs:[{kstack}]",
        "push qword ptr gs:[{user_rsp}]",
        "push rcx",
        "push r11",
        "push rax",
        "push rdi",
        "push rsi",
        "push rdx",
        "push r8",
        "push r9",
        "push r10",
        "mov r8, r10",
        "mov rcx, rdx",
        "mov rdx, rsi",
        "mov rsi, rdi",
        "mov rdi, rax",
        "sti",
        "call {dispatch}",
        "cli",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "add rsp, 8",
        "pop r11",
        "pop rcx",
        "pop rsp",
        "swapgs",
        "sysretq",
        user_rsp = const percpu::OFF_USER_RSP,
        kstack = const percpu::OFF_KSTACK_TOP,
        dispatch = sym crate::syscall::dispatch,
    );
}
