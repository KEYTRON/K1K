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
    pub const SEND_CAP: u64 = 7;
    pub const CAP_DROP: u64 = 8;
    pub const MEM_CREATE: u64 = 9;
    pub const MEM_MAP: u64 = 10;
}

/// Capability rights bits, as understood by the kernel.
pub mod rights {
    pub const SEND: u32 = 1 << 0;
    pub const RECV: u32 = 1 << 1;
    pub const GRANT: u32 = 1 << 2;
    pub const MAP_READ: u32 = 1 << 3;
    pub const MAP_WRITE: u32 = 1 << 4;
}

/// Word 3 of a received message when no capability was attached.
pub const NO_CAP: u64 = u64::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub enum Error {
    Perm = -1,
    Again = -2,
    Fault = -3,
    Inval = -4,
    NoSys = -5,
    NoMem = -6,
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
            -6 => Error::NoMem,
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
    /// Capability that arrived with the message, already in our table.
    pub cap: Option<Cap>,
}

pub fn send(cap: Cap, w0: u64, w1: u64, w2: u64) -> Result<()> {
    check(syscall(sys::SEND, cap.0 as u64, w0, w1, w2)).map(|_| ())
}

/// Send `w0` together with a copy of `what`, restricted to `rights`.
/// Requires `GRANT` on `what`.
pub fn send_cap(ep: Cap, what: Cap, rights: u32, w0: u64) -> Result<()> {
    check(syscall(
        sys::SEND_CAP,
        ep.0 as u64,
        what.0 as u64,
        rights as u64,
        w0,
    ))
    .map(|_| ())
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
    let cap = (words[3] != NO_CAP).then(|| Cap(words[3] as u32));
    Ok(Message {
        sender: sender as u32,
        words,
        cap,
    })
}

pub fn cap_drop(cap: Cap) -> Result<()> {
    check(syscall(sys::CAP_DROP, cap.0 as u64, 0, 0, 0)).map(|_| ())
}

/// Allocate `pages` zeroed pages as a shareable memory object.
pub fn mem_create(pages: usize) -> Result<Cap> {
    check(syscall(sys::MEM_CREATE, pages as u64, 0, 0, 0)).map(|s| Cap(s as u32))
}

/// Map a memory object into our address space; returns its base address.
pub fn mem_map(cap: Cap, writable: bool) -> Result<*mut u8> {
    check(syscall(sys::MEM_MAP, cap.0 as u64, writable as u64, 0, 0)).map(|va| va as *mut u8)
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
