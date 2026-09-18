//! K1K user-space runtime.
//!
//! Programs are `no_std`/`no_main`, define `k1k_main` with [`main!`] and get
//! the process entry point, syscall wrappers, `log!` and a panic handler that
//! reports through the kernel log and exits.

#![no_std]

use core::arch::{asm, naked_asm};
use core::fmt::{self, Write};

pub mod sys {
    pub const LOG: u64 = 0;
    pub const EXIT: u64 = 1;
    pub const YIELD: u64 = 2;
    pub const SLEEP: u64 = 3;
    pub const SEND: u64 = 4;
    pub const RECV: u64 = 5;
    pub const INFO: u64 = 6;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub enum Error {
    Perm = -1,
    Again = -2,
    Fault = -3,
    Inval = -4,
    NoSys = -5,
    Unknown = -1000,
}

impl Error {
    fn from_raw(v: i64) -> Self {
        match v {
            -1 => Error::Perm,
            -2 => Error::Again,
            -3 => Error::Fault,
            -4 => Error::Inval,
            -5 => Error::NoSys,
            _ => Error::Unknown,
        }
    }
}

pub type Result<T> = core::result::Result<T, Error>;

fn check(v: i64) -> Result<u64> {
    if v < 0 {
        Err(Error::from_raw(v))
    } else {
        Ok(v as u64)
    }
}

#[inline(always)]
pub fn syscall(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") nr as i64 => ret,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            in("r10") a3,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

pub fn log_bytes(s: &[u8]) {
    syscall(sys::LOG, s.as_ptr() as u64, s.len() as u64, 0, 0);
}

pub fn exit(code: i64) -> ! {
    syscall(sys::EXIT, code as u64, 0, 0, 0);
    loop {}
}

pub fn yield_now() {
    syscall(sys::YIELD, 0, 0, 0, 0);
}

pub fn sleep_ms(ms: u64) {
    syscall(sys::SLEEP, ms, 0, 0, 0);
}

/// A capability slot in this task's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cap(pub u32);

#[derive(Debug, Clone, Copy, Default)]
pub struct Message {
    pub sender: u32,
    pub words: [u64; 4],
}

pub fn send(cap: Cap, w0: u64, w1: u64, w2: u64) -> Result<()> {
    check(syscall(sys::SEND, cap.0 as u64, w0, w1, w2)).map(|_| ())
}

pub fn recv(cap: Cap) -> Result<Message> {
    let mut words = [0u64; 4];
    let sender = check(syscall(
        sys::RECV,
        cap.0 as u64,
        words.as_mut_ptr() as u64,
        0,
        0,
    ))?;
    Ok(Message {
        sender: sender as u32,
        words,
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Info {
    pub uptime_ms: u64,
    pub task_id: u32,
}

pub fn info() -> Info {
    let mut buf = [0u64; 2];
    syscall(sys::INFO, buf.as_mut_ptr() as u64, 0, 0, 0);
    Info {
        uptime_ms: buf[0],
        task_id: buf[1] as u32,
    }
}

/// Fixed-size formatting buffer so `log!` needs no allocator.
pub struct LogBuf {
    buf: [u8; 512],
    len: usize,
}

impl LogBuf {
    pub const fn new() -> Self {
        Self {
            buf: [0; 512],
            len: 0,
        }
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl Default for LogBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl Write for LogBuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = self.buf.len() - self.len;
        let n = s.len().min(room);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

#[doc(hidden)]
pub fn _log(args: fmt::Arguments) {
    let mut b = LogBuf::new();
    let _ = b.write_fmt(args);
    log_bytes(b.as_bytes());
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::_log(format_args!($($arg)*)) };
}

/// Declare the program's entry function: `fn() -> !`.
#[macro_export]
macro_rules! main {
    ($f:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn k1k_main() -> ! {
            let f: fn() -> ! = $f;
            f()
        }
    };
}

unsafe extern "C" {
    fn k1k_main() -> !;
}

/// Process entry. The kernel enters here via `iretq` with an arbitrary
/// 16-byte-aligned stack; realign and call the program.
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.entry")]
pub unsafe extern "C" fn _start() -> ! {
    naked_asm!(
        "xor ebp, ebp",
        "and rsp, -16",
        "call {main}",
        "ud2",
        main = sym k1k_main,
    );
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    log!("panic: {}", info);
    exit(101)
}
