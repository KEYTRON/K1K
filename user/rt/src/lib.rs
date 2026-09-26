//! K1K user-space runtime.
//!
//! Programs are `no_std`/`no_main`, define `k1k_main` with [`main!`] and get
//! the process entry point, syscall wrappers, `log!`, a heap allocator and a
//! panic handler that reports through the kernel log and exits.

#![no_std]

extern crate alloc;

use core::arch::{asm, naked_asm};
use core::fmt::{self, Write};

pub use heap::{HeapStats, heap_free, heap_stats};

pub mod file;
pub mod fsproto;
pub mod heap;
pub mod server;

/// The page the supervisor fills before a ring-3 task starts. Must match
/// `USER_INFO_BASE` in the kernel.
pub const INFO_BASE: u64 = 0x0000_7fff_ffff_0000 - 0x0000_0000_0002_0000;
const INFO_MAGIC: u64 = 0x3148_4F4F_425A_314B;
const INFO_LAYOUT: u64 = 1;

/// The first bytes of every ring-3 task, written by the supervisor.
#[repr(C)]
pub struct BootInfo {
    pub magic: u64,
    pub layout: u64,
    pub task_id: u32,
    pub _pad: u32,
    pub heap_base: u64,
    pub heap_size: u64,
    pub stack_top: u64,
    pub arg_len: u64,
}

/// The kernel's description of this task, or `None` if the boot info page is
/// missing or was written by a kernel this runtime does not understand.
pub fn boot_info() -> Option<&'static BootInfo> {
    let p = unsafe { &*(INFO_BASE as *const BootInfo) };
    (p.magic == INFO_MAGIC && p.layout == INFO_LAYOUT).then_some(p)
}

/// The launch arguments the spawner attached, as `key=value` lines.
pub fn args() -> &'static str {
    let Some(info) = boot_info() else {
        return "";
    };
    let len = (info.arg_len as usize).min(MAX_ARGS);
    let base = unsafe { (INFO_BASE as *const u8).add(core::mem::size_of::<BootInfo>()) };
    let bytes = unsafe { core::slice::from_raw_parts(base, len) };
    core::str::from_utf8(bytes)
        .unwrap_or("")
        .trim_end_matches('\0')
}

/// One `key=value` pair from the launch arguments.
pub fn arg(key: &str) -> Option<&'static str> {
    args().lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == key).then(|| v.trim())
    })
}

/// The capability slot the manifest granted under `name`, if any.
///
/// A service spawned from disk finds its file-server endpoint with
/// `granted("fs")` instead of assuming a slot number: the supervisor numbers
/// the capabilities in the order the manifest listed them and passes the
/// mapping to the service in the `caps=` argument.
pub fn granted(name: &str) -> Option<Cap> {
    let caps = arg("caps")?;
    for item in caps.split(',') {
        if let Some((n, slot)) = item.split_once('=')
            && n.trim() == name
        {
            return slot.trim().parse::<u32>().ok().map(Cap);
        }
    }
    None
}

/// How much of the boot info page's argument area the kernel fills.
pub const MAX_ARGS: usize = 1024;

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
    pub const EP_CREATE: u64 = 11;
    pub const MEM_CREATE_DMA: u64 = 12;
    pub const MEM_PHYS: u64 = 13;
    pub const DEV_INFO: u64 = 14;
    pub const DEV_MAP: u64 = 15;
    pub const SPAWN: u64 = 16;
    pub const IRQ_BIND: u64 = 17;
    pub const IRQ_ACK: u64 = 18;
    pub const DEV_IRQ: u64 = 19;
    pub const PORT_IN: u64 = 20;
    pub const PORT_OUT: u64 = 21;
    /// `spawn` with a descriptor: image, name, capabilities and arguments.
    pub const SPAWN_DESC: u64 = 22;
}

/// Maximum capabilities one `spawn` can hand over.
pub const MAX_SPAWN_GRANTS: usize = 8;
/// Maximum launch-argument blob a `spawn` can attach.
pub const MAX_SPAWN_ARGS: usize = 1024;

/// Wire protocol of the `blk` block-device service.
pub mod blkproto {
    /// Client → blk: attach a DMA buffer (capability attached; page count in
    /// the high 32 bits of w0, since a capability message carries one word).
    pub const REGISTER_BUF: u64 = 1;
    /// Client → blk: where to send replies (endpoint capability attached).
    pub const SET_REPLY: u64 = 2;
    /// Client → blk: read `w2` blocks starting at LBA `w1` into the buffer.
    pub const READ: u64 = 3;
    /// blk → client: w0 = status (0 = ok), w1 = blocks transferred,
    /// w2 = block size in bytes.
    pub const STATUS_OK: u64 = 0;
    pub const STATUS_ERR: u64 = 1;
    /// Largest single read, in blocks of 512 bytes (two PRP entries).
    pub const MAX_BLOCKS: u64 = 16;
}

/// Capability rights bits, as understood by the kernel.
pub mod rights {
    pub const SEND: u32 = 1 << 0;
    pub const RECV: u32 = 1 << 1;
    pub const GRANT: u32 = 1 << 2;
    pub const MAP_READ: u32 = 1 << 3;
    pub const MAP_WRITE: u32 = 1 << 4;
    pub const DMA: u32 = 1 << 5;
    pub const SPAWN: u32 = 1 << 6;
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

/// Create an endpoint we own (SEND | RECV | GRANT).
pub fn ep_create() -> Result<Cap> {
    check(syscall(sys::EP_CREATE, 0, 0, 0, 0)).map(|s| Cap(s as u32))
}

/// Allocate physically contiguous zeroed pages suitable for DMA.
pub fn mem_create_dma(pages: usize) -> Result<Cap> {
    check(syscall(sys::MEM_CREATE_DMA, pages as u64, 0, 0, 0)).map(|s| Cap(s as u32))
}

/// Physical address of a DMA memory object.
pub fn mem_phys(cap: Cap) -> Result<u64> {
    check(syscall(sys::MEM_PHYS, cap.0 as u64, 0, 0, 0))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Bar {
    pub base: u64,
    pub size: u64,
    pub io: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DevInfo {
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub bus: u8,
    pub slot: u8,
    pub func: u8,
    pub bars: [Bar; 6],
}

/// Describe a PCI device capability.
pub fn dev_info(cap: Cap) -> Result<DevInfo> {
    let mut w = [0u64; 14];
    check(syscall(
        sys::DEV_INFO,
        cap.0 as u64,
        w.as_mut_ptr() as u64,
        0,
        0,
    ))?;
    let mut info = DevInfo {
        vendor: w[0] as u16,
        device: (w[0] >> 16) as u16,
        class: (w[0] >> 32) as u8,
        subclass: (w[0] >> 40) as u8,
        bus: (w[1] >> 16) as u8,
        slot: (w[1] >> 8) as u8,
        func: w[1] as u8,
        bars: [Bar::default(); 6],
    };
    for i in 0..6 {
        info.bars[i] = Bar {
            base: w[2 + 2 * i],
            size: w[3 + 2 * i] & !(1 << 63),
            io: w[3 + 2 * i] >> 63 != 0,
        };
    }
    Ok(info)
}

/// Map a device's memory BAR (uncached); returns its base address.
pub fn dev_map(cap: Cap, bar: usize) -> Result<*mut u8> {
    check(syscall(sys::DEV_MAP, cap.0 as u64, bar as u64, 0, 0)).map(|va| va as *mut u8)
}

/// Route an interrupt object's events to `ep` as messages `[vector, count]`.
pub fn irq_bind(irq: Cap, ep: Cap) -> Result<()> {
    check(syscall(sys::IRQ_BIND, irq.0 as u64, ep.0 as u64, 0, 0)).map(|_| ())
}

/// Re-arm a level-triggered interrupt after servicing the device.
pub fn irq_ack(irq: Cap) -> Result<()> {
    check(syscall(sys::IRQ_ACK, irq.0 as u64, 0, 0, 0)).map(|_| ())
}

/// Obtain an interrupt object (MSI-X entry 0) for a device we hold.
pub fn dev_irq(dev: Cap) -> Result<Cap> {
    check(syscall(sys::DEV_IRQ, dev.0 as u64, 0, 0, 0)).map(|s| Cap(s as u32))
}

/// Read `width` (1/2/4) bytes from port `offset` within a port capability.
pub fn port_in(ports: Cap, offset: u16, width: u8) -> Result<u32> {
    check(syscall(
        sys::PORT_IN,
        ports.0 as u64,
        offset as u64,
        width as u64,
        0,
    ))
    .map(|v| v as u32)
}

pub fn port_out(ports: Cap, offset: u16, width: u8, value: u32) -> Result<()> {
    check(syscall(
        sys::PORT_OUT,
        ports.0 as u64,
        offset as u64,
        width as u64,
        value as u64,
    ))
    .map(|_| ())
}

/// Start a supervised service from the ELF image in `image` (`size` bytes).
/// Requires a `Control` capability with `SPAWN`. Returns the new task id.
pub fn spawn(control: Cap, image: Cap, size: usize, name: &str) -> Result<u32> {
    spawn_with(control, image, size, name, &[], "")
}

/// A capability to hand a service being spawned, with the rights it may keep.
pub struct Grant {
    pub cap: Cap,
    pub rights: u32,
}

impl Grant {
    /// Hand `cap` over in full, minus the rights the spawner lacks anyway.
    pub fn full(cap: Cap) -> Self {
        Self {
            cap,
            rights: u32::MAX,
        }
    }

    /// Hand `cap` over with exactly these rights.
    pub fn with(cap: Cap, rights: u32) -> Self {
        Self { cap, rights }
    }
}

/// Start a supervised service with capabilities and launch arguments.
///
/// `grants` become the new task's capability slots 0..n in order, each reduced
/// to the rights given here (never more than the spawner holds). `args` is a
/// `key=value` blob the service reads with [`arg`] — it is how a manifest
/// tells a service which slot its file-server endpoint ended up in.
pub fn spawn_with(
    control: Cap,
    image: Cap,
    size: usize,
    name: &str,
    grants: &[Grant],
    args: &str,
) -> Result<u32> {
    let name = &name.as_bytes()[..name.len().min(32)];
    if grants.len() > MAX_SPAWN_GRANTS || args.len() > MAX_SPAWN_ARGS {
        return Err(Error::Inval);
    }
    // [ctl, mem, size, name_ptr, name_len, n_grants, arg_ptr, arg_len] then
    // n_grants × { slot, rights }.
    let mut desc = [0u64; 8 + 2 * MAX_SPAWN_GRANTS];
    desc[0] = control.0 as u64;
    desc[1] = image.0 as u64;
    desc[2] = size as u64;
    desc[3] = name.as_ptr() as u64;
    desc[4] = name.len() as u64;
    desc[5] = grants.len() as u64;
    desc[6] = args.as_ptr() as u64;
    desc[7] = args.len() as u64;
    for (i, g) in grants.iter().enumerate() {
        desc[8 + i * 2] = g.cap.0 as u64;
        desc[9 + i * 2] = g.rights as u64;
    }
    check(syscall(sys::SPAWN_DESC, desc.as_mut_ptr() as u64, 0, 0, 0)).map(|id| id as u32)
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
