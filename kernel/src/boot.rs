//! Limine boot protocol requests. All requests live in `.requests` so the
//! bootloader can find them; the markers bound the scan region.

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

pub fn hhdm_offset() -> u64 {
    HHDM.response().expect("limine: no HHDM response").offset
}

pub fn cmdline() -> &'static str {
    CMDLINE.response().map(|c| c.cmdline()).unwrap_or("")
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
