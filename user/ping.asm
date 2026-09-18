; ping — IPC client. slot 0: request endpoint (SEND only), slot 1: reply endpoint (RECV).
BITS 64
%include "k1k.inc"
ORG USER_BASE

%define REQ 0
%define REP 1

_start:
    ; Capability check: slot 0 is SEND-only, so receiving on it must be refused.
    sys_recv REQ, buf
    cmp rax, K1K_EPERM
    jne .bad_rights
    sys_log msg_denied, msg_denied_len

    xor r12, r12
.loop:
    inc r12
    sys_send REQ, r12, 0, 0x50494E47      ; "PING"
    test rax, rax
    js .fail

    sys_recv REP, buf
    test rax, rax
    js .fail

    mov rdi, [rel buf]
    lea rsi, [rel numbuf_end]
    call itoa
    PASTE_NUMBER msg_reply, msg_reply_prefix_len
    mov rax, SYS_LOG
    lea rdi, [rel msg_reply]
    mov rsi, rdx
    syscall

    sys_sleep 600
    jmp .loop

.bad_rights:
    sys_log msg_bad, msg_bad_len
    sys_exit 3
.fail:
    sys_log msg_fail, msg_fail_len
    sys_exit 4

ITOA_ROUTINE

msg_denied:           db "recv on send-only cap denied (EPERM) - rights enforced"
msg_denied_len        equ $ - msg_denied
msg_bad:              db "BUG: recv on send-only cap was allowed"
msg_bad_len           equ $ - msg_bad
msg_fail:             db "ipc failure"
msg_fail_len          equ $ - msg_fail
msg_reply:            db "got reply "
msg_reply_prefix_len  equ $ - msg_reply
                      times 24 db 0
buf:                  dq 0, 0, 0, 0
numbuf:               times 24 db 0
numbuf_end:
