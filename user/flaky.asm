; flaky — a deliberately buggy service. It works for a few iterations, then
; dereferences a null pointer. The kernel kills it and the supervisor
; restarts it: the system keeps running without a reboot.
BITS 64
%include "k1k.inc"
ORG USER_BASE

_start:
    sys_log msg_start, msg_start_len
    xor r12, r12
.loop:
    inc r12
    mov rdi, r12
    lea rsi, [rel numbuf_end]
    call itoa
    PASTE_NUMBER msg_work, msg_work_prefix_len
    mov rax, SYS_LOG
    lea rdi, [rel msg_work]
    mov rsi, rdx
    syscall

    sys_sleep 400

    cmp r12, 3
    jne .loop

    sys_log msg_crash, msg_crash_len
    xor rax, rax
    mov rax, [rax]                 ; #PF at address 0 -> task is killed
    jmp .loop

ITOA_ROUTINE

msg_start:          db "flaky service started"
msg_start_len       equ $ - msg_start
msg_work:           db "working, iteration "
msg_work_prefix_len equ $ - msg_work
                    times 24 db 0
msg_crash:          db "about to dereference NULL..."
msg_crash_len       equ $ - msg_crash
numbuf:             times 24 db 0
numbuf_end:
