//! Kernel-stack context switching.
//!
//! A suspended task's callee-saved registers live on its own kernel stack in
//! the order below; `ctx_sp` in the task points at the lowest slot. Switching
//! is therefore: push ours, save rsp, load theirs, pop theirs, `ret`.

use core::arch::naked_asm;

/// Layout of the frame `switch_context` pushes/pops (low address first).
#[repr(C)]
pub struct SavedFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbx: u64,
    pub rbp: u64,
    pub rip: u64,
}

#[unsafe(naked)]
pub unsafe extern "C" fn switch_context(_prev_sp: *mut u64, _next_sp: u64) {
    naked_asm!(
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",
        "mov rsp, rsi",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
    );
}

/// First code a fresh task runs, reached via the `ret` in `switch_context`.
/// r12 = entry fn, r13 = argument. Interrupts are off at this point.
#[unsafe(naked)]
pub unsafe extern "C" fn task_trampoline() {
    naked_asm!(
        "sti",
        "mov rdi, r13",
        "call r12",
        "call {exit}",
        "ud2",
        exit = sym crate::sched::thread_exit_hook,
    );
}

/// Prepare a fresh kernel stack so that `switch_context` into it lands in
/// `task_trampoline` with `entry`/`arg` in r12/r13. Returns the initial sp.
pub unsafe fn init_stack(stack_top: u64, entry: u64, arg: u64) -> u64 {
    let top = stack_top & !0xF;
    let sp = top - core::mem::size_of::<SavedFrame>() as u64;
    let frame = sp as *mut SavedFrame;
    unsafe {
        frame.write(SavedFrame {
            r15: 0,
            r14: 0,
            r13: arg,
            r12: entry,
            rbx: 0,
            rbp: 0,
            rip: task_trampoline as *const () as u64,
        });
    }
    sp
}

/// Drop to ring 3 at `rip` with stack `rsp`. Never returns.
#[unsafe(naked)]
pub unsafe extern "C" fn enter_user(_rip: u64, _rsp: u64, _user_cs: u64, _user_ss: u64) -> ! {
    naked_asm!(
        "mov ax, cx",
        "mov ds, ax",
        "mov es, ax",
        "xor eax, eax",
        "mov fs, ax",
        "mov gs, ax",
        "push rcx",   // ss
        "push rsi",   // rsp
        "push 0x202", // rflags: IF set
        "push rdx",   // cs
        "push rdi",   // rip
        "xor eax, eax",
        "xor ebx, ebx",
        "xor ecx, ecx",
        "xor edx, edx",
        "xor esi, esi",
        "xor edi, edi",
        "xor ebp, ebp",
        "xor r8d, r8d",
        "xor r9d, r9d",
        "xor r10d, r10d",
        "xor r11d, r11d",
        "xor r12d, r12d",
        "xor r13d, r13d",
        "xor r14d, r14d",
        "xor r15d, r15d",
        "iretq",
    );
}
