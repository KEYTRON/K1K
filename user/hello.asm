; hello — the simplest ring-3 service: greets periodically and reports uptime.
BITS 64
%include "k1k.inc"
ORG USER_BASE

_start:
    sys_log msg_start, msg_start_len
    xor r12, r12
.loop:
    mov rax, SYS_INFO
    lea rdi, [rel info]
    syscall

    mov rdi, [rel info]            ; uptime ms
    lea rsi, [rel numbuf_end]
    call itoa
    PASTE_NUMBER msg, msg_prefix_len
    mov rax, SYS_LOG
    lea rdi, [rel msg]
    mov rsi, rdx
    syscall

    inc r12
    sys_sleep 900
    jmp .loop

ITOA_ROUTINE

msg_start:      db "hello service up (ring 3)"
msg_start_len   equ $ - msg_start
msg:            db "alive, uptime ms = "
msg_prefix_len  equ $ - msg
                times 24 db 0
numbuf:         times 24 db 0
numbuf_end:
info:           dq 0, 0
