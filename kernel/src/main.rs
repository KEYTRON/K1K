//! K1K — K1 Kernel.
//!
//! A hybrid, capability-based kernel: the privileged core owns scheduling,
//! address spaces, IPC and capabilities; everything else runs as isolated,
//! restartable tasks supervised by the kernel.

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![feature(allocator_api)]

extern crate alloc;

mod arch;
mod boot;
mod console;
mod mm;
mod sched;

use core::panic::PanicInfo;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[unsafe(no_mangle)]
unsafe extern "C" fn kmain() -> ! {
    arch::x86_64::early_init();
    console::init_framebuffer();

    println!();
    println!("  K1K  v{}  --  K1 Kernel (x86_64)", VERSION);
    println!("  ==================================");

    if !boot::BASE_REVISION.is_supported() {
        klog!(
            "boot",
            "limine base revision 3 not supported (actual: {:?})",
            boot::BASE_REVISION.actual_revision()
        );
    }
    if let Some(info) = boot::BOOTLOADER_INFO.response() {
        klog!("boot", "bootloader: {} {}", info.name(), info.version());
    }
    if let Some(fb) = boot::FRAMEBUFFER.response().and_then(|r| r.framebuffers().first().copied()) {
        klog!("boot", "framebuffer: {}x{} bpp={} pitch={}", fb.width, fb.height, fb.bpp, fb.pitch);
    }
    klog!("boot", "hhdm offset: {:#x}", boot::hhdm_offset());
    if let Some(ea) = boot::EXEC_ADDR.response() {
        klog!("boot", "kernel phys={:#x} virt={:#x}", ea.physical_base, ea.virtual_base);
    }

    klog!("cpu", "GDT/TSS/IDT loaded");
    x86_64::instructions::interrupts::int3();
    klog!("cpu", "breakpoint trap returned OK");

    mm::init();
    {
        let st = mm::pmm::stats();
        klog!("pmm", "usable {} MiB, free {} MiB", st.total_usable_kib / 1024, st.free_kib / 1024);
        let mut v: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
        for i in 0..100_000u64 {
            v.push(i * 3);
        }
        let b = alloc::boxed::Box::new([7u8; 4096]);
        let (used, free) = mm::heap::stats();
        klog!("heap", "alloc ok: vec[99999]={} box[0]={} used={} KiB free={} KiB", v[99_999], b[0], used / 1024, free / 1024);
        drop(v);
        drop(b);
        let asp = mm::vmm::AddressSpace::new().expect("address space");
        klog!("vmm", "user address space created, cr3={:#x}", asp.cr3().start_address());
        drop(asp);
        let st = mm::pmm::stats();
        klog!("pmm", "after teardown: free {} MiB", st.free_kib / 1024);
    }

    klog!("k1k", "milestone 2 reached: pmm + vmm + heap");
    arch::x86_64::qemu_exit(0x10);
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    x86_64::instructions::interrupts::disable();
    console::_print_force(format_args!("\n!! KERNEL PANIC: {}\n", info));
    arch::x86_64::qemu_exit(0x20);
}
