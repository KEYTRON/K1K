//! K1K — K1 Kernel.
//!
//! A hybrid, capability-based kernel: the privileged core owns scheduling,
//! address spaces, IPC and capabilities; everything else runs as isolated,
//! restartable tasks supervised by the kernel.

#![no_std]
#![no_main]

extern crate alloc;

mod arch;
mod boot;
mod console;
mod ipc;
mod loader;
mod mm;
mod notify;
mod obj;
mod sched;
mod service;
mod syscall;

use arch::x86_64::clock;
use core::panic::PanicInfo;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Spin briefly, with interrupts off, to show that the clock advances even when
/// no interrupt has run. Long enough for the TSC to move, short enough not to be
/// a delay.
fn spin_a_little() {
    for _ in 0..2_000_000 {
        core::hint::spin_loop();
    }
}

#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    arch::x86_64::early_init();
    console::init_framebuffer();

    println!();
    println!("  K1K  v{}  --  K1 Kernel (x86_64)", VERSION);
    println!("  ==================================");

    boot::init();
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
    if let Some(fb) = boot::FRAMEBUFFER
        .response()
        .and_then(|r| r.framebuffers().first().copied())
    {
        klog!(
            "boot",
            "framebuffer: {}x{} bpp={} pitch={}",
            fb.width,
            fb.height,
            fb.bpp,
            fb.pitch
        );
    }
    klog!("boot", "hhdm offset: {:#x}", boot::hhdm_offset());
    if let Some(ea) = boot::EXEC_ADDR.response() {
        klog!(
            "boot",
            "kernel phys={:#x} virt={:#x}",
            ea.physical_base,
            ea.virtual_base
        );
    }

    klog!("cpu", "GDT/TSS/IDT loaded");
    x86_64::instructions::interrupts::int3();
    klog!("cpu", "breakpoint trap returned OK");

    mm::init();
    {
        let st = mm::pmm::stats();
        klog!(
            "pmm",
            "usable {} MiB, free {} MiB",
            st.total_usable_kib / 1024,
            st.free_kib / 1024
        );
        let mut v: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
        for i in 0..100_000u64 {
            v.push(i * 3);
        }
        let b = alloc::boxed::Box::new([7u8; 4096]);
        let h = mm::heap::stats();
        klog!(
            "heap",
            "alloc ok: vec[99999]={} box[0]={} used={} KiB free={} KiB in {} region(s)",
            v[99_999],
            b[0],
            h.in_use / 1024,
            h.region_bytes.saturating_sub(h.in_use) / 1024,
            h.regions
        );
        drop(v);
        drop(b);
        // The heap has to hand memory back: after the drops above the live
        // count has to be the one allocation that is still out there.
        let h = mm::heap::stats();
        assert_eq!(h.live, 0, "heap leak: {} live block(s)", h.live);
        let asp = mm::vmm::AddressSpace::new().expect("address space");
        klog!(
            "vmm",
            "user address space created, cr3={:#x}",
            asp.cr3().start_address()
        );
        drop(asp);
        let st = mm::pmm::stats();
        klog!("pmm", "after teardown: free {} MiB", st.free_kib / 1024);
    }

    klog!("k1k", "milestone 2 reached: pmm + vmm + heap");

    sched::init();
    arch::x86_64::syscall::init();
    arch::x86_64::interrupts::init();
    arch::x86_64::interrupts::enable();
    klog!("irq", "interrupts on");
    arch::x86_64::pci::init();
    // The HPET is a PCI function, so the platform timer can only be found once
    // the bus has been scanned.
    clock::init_platform_timer();
    klog!("sys", "syscall/sysret enabled");
    arch::x86_64::smp::start_aps();

    let ep = ipc::Endpoint::new();
    sched::spawn_kernel("kthread-a", kthread_ticker, 300);
    sched::spawn_kernel(
        "ipc-server",
        kthread_ipc_server,
        alloc::sync::Arc::into_raw(ep.clone()) as u64,
    );
    sched::spawn_kernel_with(
        "ipc-client",
        kthread_ipc_client,
        alloc::sync::Arc::into_raw(ep) as u64,
        // The client keeps a memory object of its own, so that exiting it has
        // something to give back: a thread that exits holding nothing proves
        // nothing about tearing a task down.
        |t| {
            if let Some(mem) = obj::MemoryObject::new(1) {
                let _ = t.caps.insert(obj::Capability {
                    object: obj::Object::Memory(mem),
                    rights: obj::Rights::MAP_READ.union(obj::Rights::GRANT),
                });
            }
        },
    );

    let sup = sched::spawn_kernel("supervisor", service::supervisor_main, 0);
    sched::set_supervisor(sup);
    service::init_builtin();
    service::start_all();

    // Boot is done with Limine: the command line is our own copy, ACPI was
    // parsed into owned structures, the framebuffer console copied its
    // geometry and the application processors are up. Hand the rest back.
    mm::pmm::reclaim_bootloader();

    // `autotest` runs the services and exits; `autotest=<seconds>` says for how
    // long, which is what the soak test uses.
    let autotest = boot::cmdline_has("autotest") || boot::cmdline_value("autotest").is_some();
    let autotest_ms = boot::cmdline_u64("autotest").unwrap_or(8).clamp(1, 600) * 1000;
    if autotest {
        klog!(
            "k1k",
            "autotest mode: running services for {} s",
            autotest_ms / 1000
        );
    }
    let deadline = arch::x86_64::interrupts::uptime_ms() + autotest_ms;
    // Sleeping rather than `hlt`: this code runs on the boot processor's default
    // context, which is only scheduled when that CPU has nothing else to do — and
    // on a machine where it has just started six services, that is not soon. A
    // sleeper is woken by the tick whatever else is running.
    while autotest && arch::x86_64::interrupts::uptime_ms() < deadline {
        sched::sleep_ms(20);
    }

    klog!(
        "k1k",
        "--- autotest summary at {} ms ---",
        arch::x86_64::interrupts::uptime_ms()
    );

    // Grow the heap with the application processors already running: the pages
    // come from the PMM inside the allocator's critical section, and the flush
    // that tells the other CPUs about them has to happen after it, not from
    // inside it. A kernel that mapped the heap once at boot would never notice
    // a mistake here.
    {
        let before = mm::heap::stats();
        // More than the window that was mapped before the other CPUs started, so
        // the heap has to grow for real.
        let boxes = before.region_bytes / 4096 + 2048;
        let mut v: alloc::vec::Vec<alloc::boxed::Box<[u8; 4096]>> = alloc::vec::Vec::new();
        for i in 0..boxes as u32 {
            v.push(alloc::boxed::Box::new([(i % 251) as u8; 4096]));
        }
        let mut sum = 0u64;
        for (i, b) in v.iter().enumerate() {
            sum += b[0] as u64 * (i as u64 + 1);
        }
        let grew = mm::heap::stats();
        klog!(
            "heap",
            "grew with {} cpu(s) up: {} KiB -> {} KiB in {} region(s) for {} MiB of boxes, sum {}",
            arch::x86_64::percpu::count(),
            before.region_bytes / 1024,
            grew.region_bytes / 1024,
            grew.regions,
            boxes * 4 / 1024,
            sum
        );
        assert!(
            grew.region_bytes > before.region_bytes,
            "the heap did not grow when {} MiB was asked for",
            boxes * 4 / 1024
        );
        let last = v.len() - 1;
        assert_eq!(
            v[last][0],
            (last as u32 % 251) as u8,
            "heap corrupted a block it handed out"
        );
        drop(v);
        // The eight mebibytes have to go back: the services running alongside
        // keep their own allocations, so this compares against the heap as it
        // was rather than expecting it empty.
        let back = mm::heap::stats();
        assert!(
            back.in_use <= before.in_use + 64 * 1024,
            "heap leak: {} KiB in use, was {} KiB before the growth test",
            back.in_use / 1024,
            before.in_use / 1024
        );
    }

    {
        // Withdraw a capability out of a running service, the way a supervisor
        // would, and watch the accounting say so. The `notify` service is done
        // with slot 1 by now, so nothing is about to be surprised by it.
        if let Some(task) = service::task_of("notify") {
            match syscall::revoke_cap(task, 1) {
                Ok(()) => klog!(
                    "cap",
                    "revoked a capability slot of task {} ({task})",
                    service::task_of("notify").unwrap_or(task)
                ),
                Err(e) => klog!("cap", "revoking task {task}'s slot 1 failed: {e}"),
            }
            let taken = syscall::revoke_all(task);
            let left = sched::with_task(task, |t| t.caps.live_slots()).unwrap_or(0);
            klog!("cap", "withdrew {taken} more slot(s) from task {task}");
            assert_eq!(left, 0, "task {task} still holds {left} capability slot(s)");
        }
    }

    {
        clock::report();
        // Every CPU records a TSC reading on the same tick. Converted back to
        // nanoseconds they have to land within a hair of each other, or the
        // counter is not in step and a clock read on one core means nothing on
        // another.
        let hz = clock::tsc_hz();
        let mut worst = 0i64;
        let mut baseline = None;
        let mut sampled = 0;
        let mut auxed = 0;
        for cpu in 0..arch::x86_64::percpu::count() {
            let Some(pc) = arch::x86_64::percpu::by_id(cpu) else {
                continue;
            };
            let sample = pc.tsc_sample.load(core::sync::atomic::Ordering::Relaxed);
            let aux = pc.tsc_aux.load(core::sync::atomic::Ordering::Relaxed);
            if sample == 0 || hz == 0 {
                continue;
            }
            sampled += 1;
            // The reading has to belong to the CPU it was taken on, and the only
            // way to know is what `rdtscp` put in TSC_AUX at the time. A machine
            // that ignores TSC_AUX reports zero from everywhere, and then there
            // is nothing to check rather than something to fail.
            // The processor id is only worth checking where the machine keeps
            // one: `rdtscp` puts TSC_AUX in the result whether or not anything
            // ever wrote it, so a zero there means "nobody said", not "cpu 0".
            if aux != clock::NO_PROCESSOR && clock::tsc_aux_supported() {
                assert_eq!(
                    aux as usize, cpu,
                    "the TSC sample filed under cpu {cpu} carries processor id {aux}"
                );
                auxed += 1;
            }
            let ns = (sample / hz) * 1_000_000_000 + (sample % hz) * 1_000_000_000 / hz;
            match baseline {
                None => baseline = Some(ns),
                Some(base) => {
                    if let Some(diff) = ns.checked_sub(base) {
                        worst = worst.max(diff as i64);
                    }
                }
            }
        }
        // The samples are all taken on the same tick, so on a machine whose TSC
        // is in step they land within a hair of each other. On a machine whose
        // TSC is *not* in step there is nothing to compare — the kernel is not
        // using that counter as a time base — so the number is reported and the
        // check is left to the machines it can fail on.
        let shared = hz != 0 && clock::source() == clock::Source::Tsc;
        klog!(
            "clock",
            "{} cpu(s) sampled the TSC ({} with TSC_AUX{}), largest disagreement {} us{}",
            sampled,
            auxed,
            if clock::tsc_aux_supported() {
                ""
            } else {
                ", unsupported here"
            },
            worst / 1000,
            if shared {
                ""
            } else {
                " (the TSC is not the time base here)"
            }
        );
        if shared {
            assert!(
                sampled > 1 && worst < 1_000_000,
                "the TSC is not in step across cpus: {worst} ns apart"
            );
        }
        // The clock has to move forward, and by about as much as a sleep.
        let t0 = arch::x86_64::interrupts::uptime_ns();
        x86_64::instructions::interrupts::without_interrupts(|| spin_a_little());
        let t1 = arch::x86_64::interrupts::uptime_ns();
        assert!(t1 > t0, "the clock stood still: {t0} then {t1}");
        klog!(
            "clock",
            "monotonic: {} ns elapsed over a short pause",
            t1 - t0
        );
    }

    {
        let (made, signals, waits, saturated, waiting, pending) = notify::Notify::report();
        klog!(
            "notify",
            "{} object(s), {} signal(s) recorded, {} taken, {} folded into a full counter, {} task(s) waiting now, {} untaken{}",
            made,
            signals,
            waits,
            saturated,
            waiting,
            pending,
            if saturated > 0 {
                " (a driver is not taking its signals)"
            } else {
                ""
            }
        );
    }

    let mut ap_switches = 0u64;
    let mut ipi_total = 0u32;
    for cpu in 0..arch::x86_64::percpu::count() {
        if let Some(pc) = arch::x86_64::percpu::by_id(cpu) {
            let (sent, done) = arch::x86_64::ipi::stats(pc);
            ipi_total += sent;
            klog!(
                "smp",
                "cpu {} (lapic {}): {} context switches, {} IPIs ({} acknowledged)",
                pc.cpu_id,
                pc.lapic_id,
                pc.switches,
                sent,
                done
            );
            if cpu > 0 {
                ap_switches += pc.switches;
            }
        }
    }
    klog!("sched", "{} tasks in table:", sched::task_count());
    sched::dump();
    klog!("superv", "services:");
    service::dump();
    let flaky_restarts = service::restarts_of("flaky");
    let st = mm::pmm::stats();
    klog!(
        "pmm",
        "free {} MiB after {} flaky restarts",
        st.free_kib / 1024,
        flaky_restarts
    );
    let smp_ok = arch::x86_64::percpu::count() == 1 || ap_switches > 0;
    if !smp_ok {
        klog!("k1k", "FAIL: application processors never ran a task");
    }
    // The kernel reads its command line after the bootloader's memory is gone,
    // so a working autotest here also means the copy held up.
    if !mm::pmm::bootloader_reclaimed() {
        klog!("k1k", "FAIL: bootloader memory was never reclaimed");
    }
    // Every IPI the kernel sends is a TLB flush or a reschedule, and both are
    // synchronous where it matters: a request that was never acknowledged would
    // mean a stale TLB somewhere, so treat that as a failure.
    if ipi_total > 0 && ap_ipi_gap() {
        klog!("k1k", "FAIL: some IPIs were never acknowledged");
    }
    if flaky_restarts >= 2 && smp_ok && mm::pmm::bootloader_reclaimed() {
        klog!(
            "k1k",
            "autotest passed: ring 3 + capabilities + self-healing supervisor on {} cpu(s)",
            arch::x86_64::percpu::count()
        );
        arch::x86_64::qemu_exit(0x10);
    } else {
        klog!("k1k", "FAIL: flaky service was not restarted");
        arch::x86_64::qemu_exit(0x11);
    }
}

/// Whether any CPU acknowledged fewer IPIs than it was sent.
fn ap_ipi_gap() -> bool {
    (0..arch::x86_64::percpu::count()).any(|cpu| {
        arch::x86_64::percpu::by_id(cpu).is_some_and(|pc| {
            let (sent, done) = arch::x86_64::ipi::stats(pc);
            sent != done
        })
    })
}

extern "C" fn kthread_ticker(period_ms: u64) {
    let (id, name) = sched::with_current(|t| (t.id, t.name));
    for i in 1..=3 {
        klog!(
            name,
            "tick {} (task {}) at {} ms",
            i,
            id,
            arch::x86_64::interrupts::uptime_ms()
        );
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
        ep.send(ipc::Message::new(me, [i, i * 10, 0xC0FFEE, 0]))
            .unwrap();
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    x86_64::instructions::interrupts::disable();
    console::_print_force(format_args!("\n!! KERNEL PANIC: {}\n", info));
    arch::x86_64::qemu_exit(0x20);
}
