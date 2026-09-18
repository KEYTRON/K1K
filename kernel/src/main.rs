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
mod ipc;
mod mm;
mod obj;
mod sched;
mod service;
mod syscall;

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

    sched::init();
    ipc::init();
    arch::x86_64::syscall::init();
    arch::x86_64::interrupts::init();
    arch::x86_64::interrupts::enable();
    klog!("irq", "PIC remapped, PIT @ {} Hz, interrupts on", arch::x86_64::interrupts::TIMER_HZ);
    klog!("sys", "syscall/sysret enabled");

    let ep = ipc::Endpoint::new("demo");
    sched::spawn_kernel("kthread-a", kthread_ticker, 300);
    sched::spawn_kernel("ipc-server", kthread_ipc_server, alloc::sync::Arc::into_raw(ep.clone()) as u64);
    sched::spawn_kernel("ipc-client", kthread_ipc_client, alloc::sync::Arc::into_raw(ep) as u64);

    let sup = sched::spawn_kernel("supervisor", service::supervisor_main, 0);
    sched::set_supervisor(sup);
    service::init_builtin();
    service::start_all();

    let autotest = boot::cmdline_has("autotest");
    if autotest {
        klog!("k1k", "autotest mode: running services for 6 s");
    }
    let deadline = arch::x86_64::interrupts::uptime_ms() + 6000;
    loop {
        x86_64::instructions::hlt();
        if autotest && arch::x86_64::interrupts::uptime_ms() >= deadline {
            break;
        }
    }

    klog!("k1k", "--- autotest summary at {} ms ---", arch::x86_64::interrupts::uptime_ms());
    klog!("sched", "{} tasks in table:", sched::task_count());
    sched::dump();
    klog!("superv", "services:");
    service::dump();
    let flaky_restarts = service::SERVICES.lock().iter().find(|s| s.spec.name == "flaky").map(|s| s.restarts).unwrap_or(0);
    let st = mm::pmm::stats();
    klog!("pmm", "free {} MiB after {} flaky restarts", st.free_kib / 1024, flaky_restarts);
    if flaky_restarts >= 2 {
        klog!("k1k", "milestone 4 reached: ring 3 + syscalls + capabilities + self-healing supervisor");
        arch::x86_64::qemu_exit(0x10);
    } else {
        klog!("k1k", "FAIL: flaky service was not restarted");
        arch::x86_64::qemu_exit(0x11);
    }
}

extern "C" fn kthread_ticker(period_ms: u64) {
    let (id, name) = sched::with_current(|t| (t.id, t.name));
    for i in 1..=3 {
        klog!(name, "tick {} (task {}) at {} ms", i, id, arch::x86_64::interrupts::uptime_ms());
        sched::sleep_ms(period_ms);
    }
    klog!(name, "done, exiting");
}

extern "C" fn kthread_ipc_server(ep_raw: u64) {
    let ep = unsafe { alloc::sync::Arc::from_raw(ep_raw as *const ipc::Endpoint) };
    for _ in 0..3 {
        let m = ep.recv();
        klog!("kserver", "got {:?} from task {}", m.words, m.sender);
    }
}

extern "C" fn kthread_ipc_client(ep_raw: u64) {
    let ep = unsafe { alloc::sync::Arc::from_raw(ep_raw as *const ipc::Endpoint) };
    let me = sched::current_id();
    for i in 0..3u64 {
        sched::sleep_ms(200);
        ep.send(ipc::Message { sender: me, words: [i, i * 10, 0xC0FFEE, 0] }).unwrap();
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    x86_64::instructions::interrupts::disable();
    console::_print_force(format_args!("\n!! KERNEL PANIC: {}\n", info));
    arch::x86_64::qemu_exit(0x20);
}
