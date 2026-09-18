; pong — IPC server. slot 0: request endpoint (RECV), slot 1: reply endpoint (SEND).
BITS 64
%include "k1k.inc"
ORG USER_BASE

%define REQ 0
%define REP 1

_start:
    sys_log msg_start, msg_start_len
.loop:
    sys_recv REQ, buf
    test rax, rax
    js .err
    mov r13, rax                   ; sender task id

    mov rdi, [rel buf]             ; request word 0 = ping counter
    lea rsi, [rel numbuf_end]
    call itoa
    PASTE_NUMBER msg_got, msg_got_prefix_len
    mov rax, SYS_LOG
    lea rdi, [rel msg_got]
    mov rsi, rdx
    syscall

    mov rsi, [rel buf]
    inc rsi                        ; reply = counter + 1
    sys_send REP, rsi, r13, 0x504F4E47   ; "PONG"
    jmp .loop

.err:
    sys_log msg_err, msg_err_len
    sys_exit 2

ITOA_ROUTINE

msg_start:          db "pong server listening"
msg_start_len       equ $ - msg_start
msg_got:            db "request #"
msg_got_prefix_len  equ $ - msg_got
                    times 24 db 0
msg_err:            db "recv failed"
msg_err_len         equ $ - msg_err
buf:                dq 0, 0, 0, 0
numbuf:             times 24 db 0
numbuf_end:
