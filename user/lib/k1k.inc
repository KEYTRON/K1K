; K1K user-space ABI (x86_64)
;
;   rax = syscall number, args in rdi, rsi, rdx, r10; result in rax.
;   rcx and r11 are clobbered by the `syscall` instruction itself.
;   Negative results are errors (see K1K_E*).

%define SYS_LOG     0     ; (ptr, len)                  -> 0
%define SYS_EXIT    1     ; (code)                      -> never returns
%define SYS_YIELD   2     ; ()                          -> 0
%define SYS_SLEEP   3     ; (ms)                        -> 0
%define SYS_SEND    4     ; (slot, w0, w1, w2)          -> 0 | error
%define SYS_RECV    5     ; (slot, buf[4 qwords])       -> sender id | error
%define SYS_INFO    6     ; (buf[2 qwords]: uptime_ms, task_id) -> 0

%define K1K_EPERM   -1
%define K1K_EAGAIN  -2
%define K1K_EFAULT  -3
%define K1K_EINVAL  -4
%define K1K_ENOSYS  -5

%define USER_BASE   0x400000

%macro sys_log 2
    mov rax, SYS_LOG
    lea rdi, [rel %1]
    mov rsi, %2
    syscall
%endmacro

%macro sys_sleep 1
    mov rax, SYS_SLEEP
    mov rdi, %1
    syscall
%endmacro

%macro sys_exit 1
    mov rax, SYS_EXIT
    mov rdi, %1
    syscall
%endmacro

%macro sys_yield 0
    mov rax, SYS_YIELD
    syscall
%endmacro

; sys_send slot, w0, w1, w2
%macro sys_send 4
    mov rax, SYS_SEND
    mov rdi, %1
    mov rsi, %2
    mov rdx, %3
    mov r10, %4
    syscall
%endmacro

; sys_recv slot, buf
%macro sys_recv 2
    mov rax, SYS_RECV
    mov rdi, %1
    lea rsi, [rel %2]
    syscall
%endmacro

; Write the decimal representation of rdi into the buffer ending at rsi
; (exclusive). Returns rax = pointer to first digit, rdx = length.
%macro ITOA_ROUTINE 0
itoa:
    mov rax, rdi
    mov r8, rsi
    mov rcx, 10
.next:
    xor rdx, rdx
    div rcx
    add dl, '0'
    dec rsi
    mov [rsi], dl
    test rax, rax
    jnz .next
    mov rax, rsi
    mov rdx, r8
    sub rdx, rsi
    ret
%endmacro

; Copy rdx bytes from rax into the buffer at %1 + %2 (fixed offset), leaving
; the total message length in rdx.
%macro PASTE_NUMBER 2
    lea rdi, [rel %1 + %2]
    mov rsi, rax
    mov rcx, rdx
    rep movsb
    mov rdx, rdi
    lea rdi, [rel %1]
    sub rdx, rdi
%endmacro
