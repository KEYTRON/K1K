//! Application-processor bring-up through the Limine MP protocol. Limine parks
//! every AP in long mode with our page tables; we hand each one its per-CPU
//! block and it joins the scheduler as its own idle task.

use core::sync::atomic::{AtomicUsize, Ordering};
use limine::mp::MpInfo;

use super::{acpi, gdt, idt, interrupts, percpu, syscall};
use crate::{boot, klog, sched};

static APS_ONLINE: AtomicUsize = AtomicUsize::new(0);

pub fn start_aps() {
    let Some(resp) = boot::MP.response() else {
        klog!("smp", "no MP response from bootloader, staying single-core");
        return;
    };
    let bsp = resp.bsp_lapic_id;
    let cpus = resp.cpus();
    klog!("smp", "{} cpu(s) reported, bsp lapic {}", cpus.len(), bsp);

    // What the firmware describes and what can actually be started are not the
    // same list. A machine with more than 255 logical processors is described
    // with MADT entries that carry 32-bit x2APIC ids, while the MP tables the
    // bootloader hands over name processors with eight-bit ones, so the extra
    // processors are visible in ACPI and unreachable through this path. Saying
    // so, with both numbers, is better than starting 255 of 256 quietly.
    let described = acpi::madt().cpus.len();
    let startable = cpus.len();
    if described > startable {
        klog!(
            "smp",
            "WARNING: ACPI describes {described} logical processor(s) but {startable} can be \
             started: ids past 255 need the x2apic startup protocol, which this kernel does \
             not do yet"
        );
    }

    let mut next_id = 1u32;
    for info in cpus {
        if info.lapic_id == bsp {
            continue;
        }
        if next_id as usize >= percpu::MAX_CPUS {
            klog!(
                "smp",
                "cpu limit reached, not starting lapic {}",
                info.lapic_id
            );
            break;
        }
        let tables = gdt::alloc_ap();
        let tss = gdt::tss_ptr(tables);
        let pc = percpu::alloc_ap(next_id, info.lapic_id, tss);
        let arg = ApBoot { percpu: pc, tables };
        let arg = alloc::boxed::Box::leak(alloc::boxed::Box::new(arg));

        let before = APS_ONLINE.load(Ordering::Acquire);
        info.bootstrap(ap_entry, arg as *mut ApBoot as u64);
        let mut spins = 0u64;
        while APS_ONLINE.load(Ordering::Acquire) == before {
            core::hint::spin_loop();
            spins += 1;
            if spins > 200_000_000 {
                klog!(
                    "smp",
                    "cpu {} (lapic {}) did not come up",
                    next_id,
                    info.lapic_id
                );
                break;
            }
        }
        next_id += 1;
    }
    let online = percpu::count();
    klog!("smp", "{online} cpu(s) online");
    if super::ipi::selftest() {
        klog!("smp", "IPI round trip: all {online} cpu(s) answered");
    } else {
        klog!("smp", "WARNING: some cpus did not answer an IPI");
    }
}

struct ApBoot {
    percpu: *mut percpu::PerCpu,
    tables: *mut gdt::CpuTables,
}

unsafe extern "C" fn ap_entry(info: &MpInfo) -> ! {
    let boot = unsafe { &*(info.extra_argument() as *const ApBoot) };
    unsafe {
        gdt::load_ap(boot.tables);
        idt::load();
        percpu::install_ap(boot.percpu);
    }
    syscall::init();
    sched::register_idle_cpu();
    interrupts::init_local();
    klog!(
        "smp",
        "cpu {} online (lapic {}, acpi id {})",
        percpu::cpu_id(),
        info.lapic_id,
        info.processor_id
    );
    APS_ONLINE.fetch_add(1, Ordering::AcqRel);
    interrupts::enable();
    // Halting forever would leave this CPU out of the scheduler entirely: it
    // would never see work handed to it, and its own run queue would be dead
    // weight. The idle wait is where a CPU with nothing to do picks work up.
    loop {
        sched::idle_wait();
    }
}
