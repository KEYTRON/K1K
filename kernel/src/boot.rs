//! Limine boot protocol requests. All requests live in `.requests` so the
//! bootloader can find them; the markers bound the scan region.
//!
//! Everything the bootloader answers lives in memory it marks reclaimable, so
//! [`init`] copies the parts the kernel still needs — today the command line —
//! out of Limine, and [`crate::mm::pmm::reclaim_bootloader`] hands the rest back
//! once boot is finished with it. Reading a response after that point would be a
//! use-after-free, which is why nothing here does.

use core::ptr::{addr_of, addr_of_mut};
use limine::request::{
    BootloaderInfoRequest, ExecutableAddressRequest, ExecutableCmdlineRequest, FramebufferRequest,
    HhdmRequest, MemmapRequest, MpRequest, RsdpRequest, StackSizeRequest,
};
use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker};

pub const KERNEL_STACK_SIZE: u64 = 256 * 1024;

#[used]
#[unsafe(link_section = ".requests_start_marker")]
static START_MARKER: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[unsafe(link_section = ".requests_end_marker")]
static END_MARKER: RequestsEndMarker = RequestsEndMarker::new();

#[used]
#[unsafe(link_section = ".requests")]
pub static BASE_REVISION: BaseRevision = BaseRevision::with_revision(3);

#[used]
#[unsafe(link_section = ".requests")]
pub static FRAMEBUFFER: FramebufferRequest = FramebufferRequest::new();

#[used]
#[unsafe(link_section = ".requests")]
pub static HHDM: HhdmRequest = HhdmRequest::new();

#[used]
#[unsafe(link_section = ".requests")]
pub static MEMMAP: MemmapRequest = MemmapRequest::new();

#[used]
#[unsafe(link_section = ".requests")]
pub static STACK_SIZE: StackSizeRequest = StackSizeRequest::new(KERNEL_STACK_SIZE);

#[used]
#[unsafe(link_section = ".requests")]
pub static EXEC_ADDR: ExecutableAddressRequest = ExecutableAddressRequest::new();

#[used]
#[unsafe(link_section = ".requests")]
pub static BOOTLOADER_INFO: BootloaderInfoRequest = BootloaderInfoRequest::new();

#[used]
#[unsafe(link_section = ".requests")]
pub static CMDLINE: ExecutableCmdlineRequest = ExecutableCmdlineRequest::new();

/// Base revision 3: the RSDP address is physical.
#[used]
#[unsafe(link_section = ".requests")]
pub static BOOT_RSDP: RsdpRequest = RsdpRequest::new();

/// Ask the bootloader to park the application processors for us (xAPIC mode).
#[used]
#[unsafe(link_section = ".requests")]
pub static MP: MpRequest = MpRequest::new(0);

/// Longest command line kept; Limine's own limit is 4096 including the NUL.
const CMDLINE_MAX: usize = 4096;

static mut CMDLINE_COPY: [u8; CMDLINE_MAX] = [0; CMDLINE_MAX];
static mut CMDLINE_LEN: usize = 0;

/// Copy everything we still need out of the bootloader's memory. Must run
/// before [`crate::mm::pmm::reclaim_bootloader`].
pub fn init() {
    let raw = CMDLINE.response().map(|c| c.cmdline()).unwrap_or("");
    let n = raw.len().min(CMDLINE_MAX - 1);
    unsafe {
        let dst = addr_of_mut!(CMDLINE_COPY).cast::<u8>();
        core::ptr::copy_nonoverlapping(raw.as_ptr(), dst, n);
        dst.add(n).write(0);
        CMDLINE_LEN = n;
    }
}

pub fn hhdm_offset() -> u64 {
    HHDM.response().expect("limine: no HHDM response").offset
}

/// The command line, from our own copy — safe after the bootloader's memory has
/// been reclaimed.
pub fn cmdline() -> &'static str {
    unsafe {
        let bytes = &*addr_of!(CMDLINE_COPY);
        let n = CMDLINE_LEN.min(bytes.len());
        core::str::from_utf8(&bytes[..n]).unwrap_or("")
    }
}

pub fn cmdline_has(flag: &str) -> bool {
    cmdline().split_whitespace().any(|f| f == flag)
}

/// The value of `key=value` on the kernel command line.
pub fn cmdline_value(key: &str) -> Option<&'static str> {
    let prefix = alloc::format!("{key}=");
    cmdline()
        .split_whitespace()
        .find_map(|f| f.strip_prefix(prefix.as_str()))
}

/// `key=value` as a number, if it is one.
pub fn cmdline_u64(key: &str) -> Option<u64> {
    cmdline_value(key).and_then(|v| v.parse().ok())
}
