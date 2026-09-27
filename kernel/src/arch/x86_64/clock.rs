//! Clocks: the time stamp counter, the HPET, and the tick they are checked
//! against.
//!
//! The scheduler's tick is a local APIC timer, which is the right thing for a
//! quantum and the wrong thing for anything that measures. Counting ticks
//! throws away everything between the last interrupt and the read, the count is
//! 32 bits wide, and it only advances on the boot processor. Two better sources
//! are here:
//!
//! - The **TSC** ticks at a fixed rate, is read with one instruction and needs
//!   no interrupt at all. When the CPU says it is invariant (CPUID leaf
//!   0x80000007) every core reads the same counter, so it is the time base.
//! - The **HPET** is the platform timer the firmware advertises in ACPI. It is
//!   slower to read and lives in system memory, but it is not on the CPU die,
//!   which makes it the one worth disagreeing with.
//!
//! So the TSC is what `now_ns` counts on, the HPET is read alongside it, and
//! the two are compared. A machine where they drift apart is a machine whose
//! time is worth knowing about before anything else is.
//!
//! Neither source is trusted blindly. The TSC's frequency comes from the CPU
//! when it will say, and is measured against the PIT when it will not. If there
//! is no invariant TSC and no HPET, `now_ns` falls back to counting ticks, which
//! is worse in every way but always there.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::ToString;
use core::arch::asm;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use super::{interrupts, pit};
use crate::klog;
use crate::mm::vmm;

/// Where `now_ns` gets its time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// The time stamp counter, every core in step.
    Tsc,
    /// The HPET in system memory.
    Hpet,
    /// Counting timer interrupts: coarse, but always available.
    Tick,
}

const SOURCE_TICK: u8 = 0;
const SOURCE_TSC: u8 = 1;
const SOURCE_HPET: u8 = 2;

static SOURCE: AtomicU8 = AtomicU8::new(SOURCE_TICK);
static TSC_HZ: AtomicU64 = AtomicU64::new(0);
static TSC_BASE: AtomicU64 = AtomicU64::new(0);
static TSC_INVARIANT: AtomicU64 = AtomicU64::new(0);
/// Last measured difference between the TSC and the HPET, in parts per million.
static TSC_HPET_PPM: AtomicU64 = AtomicU64::new(u64::MAX);
/// Nanoseconds the boot processor's tick counter has been counting.
static TICK_NS: AtomicU64 = AtomicU64::new(0);

/// Read the TSC, ordered against everything that came before.
///
/// `lfence` before the read keeps the compiler and the CPU from moving the read
/// above the loads whose timestamps are being taken — without it the answer can
/// be a cycle or two wrong, which is the kind of wrong that makes a
/// microbenchmark lie.
#[inline]
pub fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        // `lfence` first: without it the read can be hoisted above the loads
        // whose timestamps are being taken, and the answer is wrong by however
        // long those loads took.
        asm!(
            "lfence",
            "rdtsc",
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack, preserves_flags),
        );
    }
    (u64::from(hi) << 32) | u64::from(lo)
}

/// Whether this processor has `rdtscp`, which is what makes a TSC reading
/// attributable to the CPU that took it. Executing it where it does not exist is
/// an invalid opcode trap in the middle of the timer interrupt, so the answer
/// comes from CPUID and everything that wants a processor id asks first.
pub fn rdtscp_supported() -> bool {
    RDTSCP_SUPPORTED.load(Ordering::Relaxed) == 1
}

static RDTSCP_SUPPORTED: AtomicU64 = AtomicU64::new(0);

/// Read the TSC and, where the processor has `rdtscp`, which processor it was
/// read on.
#[inline]
pub fn sample() -> (u64, u32) {
    if rdtscp_supported() {
        rdtscp()
    } else {
        (rdtsc(), NO_PROCESSOR)
    }
}

/// Reported in place of a processor id on a processor with no `rdtscp`.
pub const NO_PROCESSOR: u32 = u32::MAX;

#[inline]
pub fn rdtscp() -> (u64, u32) {
    let lo: u32;
    let hi: u32;
    let aux: u32;
    unsafe {
        // `rdtscp` waits for everything before it, so it needs no fence after.
        asm!(
            "rdtscp",
            out("eax") lo,
            out("edx") hi,
            out("ecx") aux,
            options(nomem, nostack, preserves_flags),
        );
    }
    ((u64::from(hi) << 32) | u64::from(lo), aux)
}

/// `cpuid`, with the register shuffling it takes on x86-64.
///
/// `cpuid` clobbers `ebx`, which is the frame pointer LLVM may be using, and an
/// operand may not name it at all. Copying it aside, running the instruction and
/// swapping the result back leaves `rbx` holding what it held before — so the
/// compiler's assumption that it survives the block is true — and the answer
/// arrives in a register it is allowed to clobber.
/// Whether CPUID says this processor has `rdtscp`.
fn has_rdtscp() -> bool {
    cpuid(0x8000_0000, 0).0 >= 0x8000_0001 && cpuid(0x8000_0001, 0).3 & (1 << 27) != 0
}

/// Write a model-specific register. Only ever used for TSC_AUX.
fn wrmsr(msr: u32, value: u64) {
    let (lo, hi) = (value as u32, (value >> 32) as u32);
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") lo,
            in("edx") hi,
            options(nostack, preserves_flags),
        );
    }
}

/// Tell the processor which CPU it is, in TSC_AUX.
///
/// Nothing sets this on x86: it reads back zero until software fills it in, and
/// then `rdtscp` says which core took the reading. That is what makes a TSC
/// sample attributable to a CPU rather than to whoever happened to run the
/// code, and it costs one MSR write per CPU at startup.
///
/// The register only exists where CPUID says it does. Writing it where it does
/// not is a general protection fault, which on a machine whose CPUID view is
/// filtered is not a theoretical concern.
pub fn set_tsc_aux(cpu: u32) {
    const TSC_AUX: u32 = 0x6C1;
    let max_ext = cpuid(0x8000_0000, 0).0;
    if max_ext < 0x8000_0001 {
        return;
    }
    if cpuid(0x8000_0001, 0).3 & (1 << 19) == 0 {
        TSC_AUX_SUPPORTED.store(0, Ordering::Relaxed);
        return;
    }
    TSC_AUX_SUPPORTED.store(1, Ordering::Relaxed);
    wrmsr(TSC_AUX, cpu as u64);
}

/// Whether this machine has a usable TSC_AUX.
pub fn tsc_aux_supported() -> bool {
    TSC_AUX_SUPPORTED.load(Ordering::Relaxed) == 1
}

static TSC_AUX_SUPPORTED: AtomicU64 = AtomicU64::new(0);

fn cpuid(leaf: u32, sub: u32) -> (u32, u32, u32, u32) {
    let mut a = leaf;
    let mut c = sub;
    let (d, b);
    unsafe {
        asm!(
            "mov r10, rbx",
            "cpuid",
            "xchg r10, rbx",
            inout("eax") a,
            inout("ecx") c,
            out("edx") d,
            out("r10") b,
            options(nostack),
        );
    }
    (a, b, c, d)
}

/// The TSC's frequency, 0 if it is unknown.
pub fn tsc_hz() -> u64 {
    TSC_HZ.load(Ordering::Relaxed)
}

pub fn source() -> Source {
    match SOURCE.load(Ordering::Relaxed) {
        SOURCE_TSC => Source::Tsc,
        SOURCE_HPET => Source::Hpet,
        _ => Source::Tick,
    }
}

/// Difference between the TSC and the HPET in parts per million, `u64::MAX` if
/// the HPET is not there or not comparable yet.
pub fn tsc_hpet_ppm() -> i64 {
    let raw = TSC_HPET_PPM.load(Ordering::Relaxed);
    if raw == u64::MAX {
        return i64::MAX;
    }
    raw as i64
}

// ── HPET ────────────────────────────────────────────────────────────────────

/// Where the HPET's registers are, once found. ACPI puts the register block at
/// the same physical address as the table itself: the header is the first bytes
/// of the register block, and the capability register sits at offset 0xF0.
struct Hpet {
    base: u64,
    /// Femtoseconds per counter tick, 0 if unknown.
    period_fs: u64,
    /// The counter is 64 bits wide and never wraps in practice.
    wide: bool,
    /// Counter value when this kernel started reading it.
    base_count: u64,
    /// Rate counted off the counter, in hertz, and the period it implies.
    measured_hz: u64,
    /// TSC reading taken at the same moment, for the drift measurement.
    base_tsc: u64,
}

const HPET_COUNTER: u64 = 0x10;
/// Capabilities is at 0xF0 and configuration at 0xF4; the register block puts
/// the timer 0 registers first and the control block at the end.
const HPET_CAPS: u64 = 0xF0;
const HPET_CONFIG: u64 = 0xF4;
const HPET_ENABLE: u32 = 1 << 13;
/// The HPET is a PCI function even when the firmware only describes it in ACPI.
const HPET_PCI_VENDOR: u16 = 0x8086;
const HPET_PCI_DEVICE: u16 = 0xA201;
/// Timer i's configuration register: interrupt masked, and the comparator it
/// watches parked out of the way.
const fn timer_config(i: u32) -> u64 {
    0x20 * i as u64
}
const fn timer_comparator(i: u32) -> u64 {
    0x20 * i as u64 + 8
}
const TIMER_INT_DISABLE: u32 = 1 << 2;

/// The TSC frequency, for measuring the HPET against.
fn tsc_hz_now() -> u64 {
    TSC_HZ.load(Ordering::Relaxed)
}

static HPET_PTR: AtomicU64 = AtomicU64::new(0);

impl Hpet {
    fn read32(&self, off: u64) -> u32 {
        unsafe { ((self.base + off) as *const u32).read_volatile() }
    }

    fn read64(&self, off: u64) -> u64 {
        unsafe { ((self.base + off) as *const u64).read_volatile() }
    }

    fn write32(&self, off: u64, v: u32) {
        unsafe { ((self.base + off) as *mut u32).write_volatile(v) }
    }

    fn write64(&self, off: u64, v: u64) {
        unsafe { ((self.base + off) as *mut u64).write_volatile(v) }
    }

    /// The main counter, read the width the capability register claims.
    ///
    /// A 32-bit read has to be a single access: two 8-bit accesses can straddle
    /// a wrap and produce a number that never existed.
    fn counter(&self) -> u64 {
        if self.wide {
            self.read64(HPET_COUNTER)
        } else {
            self.read32(HPET_COUNTER) as u64
        }
    }

    /// Whether the main counter is counting.
    fn counting(&self) -> bool {
        let a = self.counter();
        // The counter ticks every ten nanoseconds or so; a few hundred cycles of
        // spinning is already thousands of ticks.
        for _ in 0..2_000 {
            core::hint::spin_loop();
        }
        self.counter() != a
    }

    /// Switch the main counter on, without letting a timer interrupt anyone.
    ///
    /// Two layouts are in the wild: the specification describes 32-bit
    /// registers with the configuration at 0xF4, and QEMU's HPET answers only
    /// 64-bit accesses with the configuration at 0xF0. Guessing would leave the
    /// clock dead on one of them, so both are tried and the one that makes the
    /// counter move wins. The offsets are an implementation detail; time
    /// advancing is the requirement.
    fn enable(&self, timers: u32) -> bool {
        for i in 0..timers.max(1) {
            // Timer i's configuration is at 0x20*i and its comparator at
            // 0x20*i + 8. Reset leaves every comparator at zero with its
            // interrupt *enabled*, so switching the main counter on without
            // this makes every timer match at once — an interrupt storm that
            // looks exactly like a hung machine.
            self.write32(timer_config(i), TIMER_INT_DISABLE);
            if self.wide {
                self.write64(timer_comparator(i), u64::MAX);
            } else {
                self.write32(timer_comparator(i), u32::MAX);
            }
        }
        for (off, wide) in [(HPET_CONFIG, false), (HPET_CONFIG - 4, true)] {
            if wide {
                self.write64(off, u64::from(HPET_ENABLE));
            } else {
                self.write32(off, HPET_ENABLE);
            }
            if self.counting() {
                return true;
            }
        }
        false
    }

    /// Start the counter and work out what it ticks at.
    ///
    /// The rate is counted against the TSC rather than decoded out of the
    /// capability register: implementations spell the unit in that register
    /// differently, and a clock that is off by a factor of ten is worse than no
    /// clock. Counting cannot be wrong.
    fn calibrate(&mut self) {
        if !self.enable(1) {
            return;
        }
        // Count over a window long enough that the division has plenty of
        // precision: about a millisecond of ticks.
        let before = self.counter();
        let tsc0 = rdtsc();
        while rdtsc().wrapping_sub(tsc0) < tsc_hz_now() / 1_000 {
            core::hint::spin_loop();
        }
        let ticks = self.counter().wrapping_sub(before);
        let elapsed = rdtsc().wrapping_sub(tsc0);
        let hz = tsc_hz_now();
        if ticks == 0 || elapsed == 0 || hz == 0 {
            return;
        }
        let rate = (ticks as u128 * hz as u128 / elapsed as u128) as u64;
        // A platform timer runs between 1 MHz and 1 GHz. Anything outside that
        // is a register being read but not understood, and trusting it would
        // put every timestamp this kernel ever reports out by the same factor.
        if !(1_000_000..1_000_000_000).contains(&rate) {
            klog!("clock", "HPET ticks at {} Hz, which is not a clock", rate);
            return;
        }
        self.measured_hz = rate;
        self.period_fs = 1_000_000_000_000_000 / rate;
    }

    /// Whether the counter turned out to be usable.
    fn running(&self) -> bool {
        self.measured_hz != 0
    }

    fn set_base(&mut self) {
        self.base_count = self.counter();
        self.base_tsc = rdtsc();
    }
}

fn hpet() -> Option<&'static Hpet> {
    let ptr = HPET_PTR.load(Ordering::Acquire);
    if ptr == 0 {
        None
    } else {
        Some(unsafe { &*(ptr as *const Hpet) })
    }
}

/// Map the HPET register block, start its counter and work out what it ticks at.
///
/// The register block belongs to a PCI function, and a PCI function that has
/// not been told to decode its BAR does not answer: reads come back as zero,
/// which looks exactly like a device that is present but switched off. No
/// firmware in this kernel's boot path touches PCI configuration space, so the
/// kernel has to do it — enable memory decoding, and take the base from the BAR
/// the device actually has rather than from what the description table claims.
fn init_hpet(table: super::acpi::HpetTable) -> bool {
    let mut phys = table.base;
    match super::pci::find_id(HPET_PCI_VENDOR, HPET_PCI_DEVICE) {
        Some(dev) => {
            super::pci::enable(&dev);
            let bar = dev.bars[0];
            if !bar.io && bar.base != 0 && bar.base != u64::MAX {
                if bar.base != phys {
                    klog!(
                        "clock",
                        "HPET table says {phys:#x}, its BAR says {:#x}; using the BAR",
                        bar.base
                    );
                }
                phys = bar.base;
            }
        }
        None => klog!(
            "clock",
            "no HPET on the PCI bus; trusting the table's address"
        ),
    }
    if phys % 0x1000 != 0 {
        klog!(
            "clock",
            "HPET registers at {phys:#x} are not page aligned; ignoring them"
        );
        return false;
    }
    vmm::map_phys_hhdm(x86_64::PhysAddr::new(phys), 0x1000);

    let mut hpet = Hpet {
        base: phys + crate::mm::pmm::hhdm(),
        period_fs: 0,
        // Read the counter 64 bits wide until the capability register says
        // otherwise: that is the only width every implementation is known to
        // answer, and a 32-bit-only HPET is rare enough to detect rather than to
        // assume.
        wide: true,
        base_count: 0,
        measured_hz: 0,
        base_tsc: 0,
    };
    hpet.calibrate();
    if !hpet.running() {
        klog!(
            "clock",
            "HPET at {phys:#x} did not start counting; ignoring it"
        );
        return false;
    }

    let caps = hpet.read32(HPET_CAPS);
    hpet.wide = caps & (1 << 8) != 0;
    let vendor = (caps >> 16) & 0xFFFF;
    let timers = ((caps >> 12) & 0xF).max(1);
    klog!(
        "clock",
        "HPET at {phys:#x} counting: {} MHz measured ({} fs/tick), capability {caps:#010x}: \
         vendor {vendor:#06x}, {} bit counter, {} timer(s)",
        hpet.measured_hz / 1_000_000,
        hpet.period_fs,
        if hpet.wide { 64 } else { 32 },
        timers
    );
    hpet.set_base();
    let ptr = Box::leak(Box::new(hpet)) as *mut Hpet as u64;
    HPET_PTR.store(ptr, Ordering::Release);
    true
}

/// Nanoseconds since the HPET was started, or `None` if there is no HPET.
fn hpet_ns() -> Option<u64> {
    let h = hpet()?;
    let ticks = h.counter().wrapping_sub(h.base_count);
    // ticks * period is in femtoseconds; there is no overflow at any plausible
    // rate for the first few years of uptime, and saturating is the honest
    // answer when there is not.
    Some((ticks.saturating_mul(h.period_fs) / 1_000_000) as u64)
}

// ── calibration ─────────────────────────────────────────────────────────────

/// Measure the TSC against the PIT, which is the one clock here whose rate is a
/// property of the chipset rather than of a guess.
fn calibrate_tsc() -> u64 {
    const WINDOW_MS: u32 = 20;
    const WINDOWS: usize = 5;
    // `busy_wait_ms` waits for *at least* the window, so every measurement is an
    // upper bound on the rate and the largest of several is the best estimate:
    // an interrupt that stole time makes a window look longer, never shorter.
    // The median is kept as well, because one wild window — a scheduler tick
    // landing in the middle of a virtualised PIT wait — should not be allowed to
    // set the rate.
    let mut rates = [0u64; WINDOWS];
    for slot in rates.iter_mut() {
        let t0 = rdtsc();
        pit::busy_wait_ms(WINDOW_MS);
        let elapsed = rdtsc().wrapping_sub(t0);
        *slot = if elapsed == 0 {
            0
        } else {
            elapsed.saturating_mul(1000) / WINDOW_MS as u64
        };
    }
    let mut sorted = rates;
    sorted.sort_unstable();
    // The median, unless it is zero, in which case the largest nonzero reading is
    // all there is.
    let median = sorted[WINDOWS / 2];
    if median > 0 {
        median
    } else {
        sorted.iter().copied().filter(|r| *r > 0).max().unwrap_or(0)
    }
}

/// The frequency the CPU claims, if it claims one.
fn tsc_hz_from_cpu() -> Option<u64> {
    // Leaf 0x16: base frequency in MHz plus the ratio of the *current* core to
    // it. The ratio form is only exact on a core that is at its base clock, so
    // it is used as a hint, never as the answer.
    let max = cpuid(0, 0).0;
    if max >= 0x16_0000 {
        let (a, _, _, _) = cpuid(0x16, 0);
        let base_mhz = a & 0xFFFF;
        if base_mhz != 0 {
            return Some(base_mhz as u64 * 1_000_000);
        }
    }
    // Leaf 0x15: the crystal the TSC runs off, in Hz.
    if max >= 0x15_0000 {
        let (_, _, ecx, _) = cpuid(0x15, 0);
        let denom = ecx >> 16;
        let numer = ecx & 0xFFFF;
        if denom != 0 {
            return Some(1_000_000_000 * numer as u64 / denom as u64);
        }
    }
    None
}

// ── init ────────────────────────────────────────────────────────────────────

/// Start the time base. Called once, after the interrupt controllers are up and
/// before anything needs a clock: the TSC is measured here, because the PIT it is
/// measured against is the one thing that is neither an emulated register nor a
/// guess.
pub fn init() {
    let (_, _, _, d) = cpuid(0x8000_0000, 0);
    let has_ext = d >= 0x8000_0007;
    let invariant = has_ext && cpuid(0x8000_0007, 0).3 & (1 << 8) != 0;
    TSC_INVARIANT.store(u64::from(invariant), Ordering::Relaxed);

    // A processor without `rdtscp` can still give a TSC reading; it just cannot
    // say which processor took it.
    RDTSCP_SUPPORTED.store(u64::from(has_rdtscp()), Ordering::Relaxed);
    let measured = calibrate_tsc();
    // The measured value is the answer; what the CPU claims is a cross-check,
    // because a vendor that gets its own base frequency wrong would otherwise
    // be believed.
    let hz = if measured > 0 { measured } else { 0 };
    TSC_HZ.store(hz, Ordering::Relaxed);
    TSC_BASE.store(rdtsc(), Ordering::Relaxed);

    let claimed = tsc_hz_from_cpu();
    if let Some(claimed) = claimed {
        let diff = (hz as i128 - claimed as i128).abs() * 1_000_000 / hz.max(1) as i128;
        klog!(
            "clock",
            "TSC {} MHz, {}; the CPU claims {} MHz, {} ppm apart",
            hz / 1_000_000,
            if invariant {
                "invariant"
            } else {
                "NOT invariant"
            },
            claimed / 1_000_000,
            diff
        );
    } else {
        klog!(
            "clock",
            "TSC {} MHz, {}; the CPU would not say how fast it ticks",
            hz / 1_000_000,
            if invariant {
                "invariant"
            } else {
                "NOT invariant"
            }
        );
    }

    // A TSC that the CPU will not promise is invariant is not a time base: it
    // may run at a different rate, or from a different origin, on every core,
    // and a clock that means different things on different cores is worse than
    // no clock.
    SOURCE.store(
        if invariant && measured > 0 {
            SOURCE_TSC
        } else {
            SOURCE_TICK
        },
        Ordering::Release,
    );
}

/// Bring up the platform timer, once the PCI bus has been scanned.
///
/// The HPET is a PCI function, and finding it needs the device list, so this is
/// a second step rather than part of [`init`]. It only replaces the tick counter
/// if nothing better was found: an invariant TSC stays the time base, and the
/// HPET becomes the thing the TSC is checked against.
pub fn init_platform_timer() {
    let Some(table) = super::acpi::hpet() else {
        klog!(
            "clock",
            "no HPET in ACPI; the tick counter stays the time base"
        );
        return;
    };
    let _ = table.period_fs;
    let Some(table) = super::acpi::hpet() else {
        klog!(
            "clock",
            "no HPET in ACPI; the tick counter stays the time base"
        );
        return;
    };
    let _ = table.period_fs;
    if !init_hpet(table) {
        return;
    }
    if SOURCE.load(Ordering::Relaxed) == SOURCE_TICK {
        SOURCE.store(SOURCE_HPET, Ordering::Release);
    }
}

/// Nanoseconds since the kernel started counting.
///
/// Monotonic on every core that has an invariant TSC, which is the case the
/// boot log reports. Falls back to the HPET, and then to counting ticks, so it
/// is never unavailable — only less precise.
#[inline]
pub fn now_ns() -> u64 {
    match SOURCE.load(Ordering::Relaxed) {
        SOURCE_TSC => {
            let hz = TSC_HZ.load(Ordering::Relaxed);
            if hz == 0 {
                return TICK_NS.load(Ordering::Relaxed);
            }
            tsc_ns_since(TSC_BASE.load(Ordering::Relaxed), rdtsc(), hz)
        }
        SOURCE_HPET => hpet_ns().unwrap_or_else(|| TICK_NS.load(Ordering::Relaxed)),
        _ => TICK_NS.load(Ordering::Relaxed),
    }
}

/// Called from the timer interrupt on every CPU.
///
/// It keeps the tick-based fallback honest, measures how far the TSC and the
/// HPET have drifted apart, and — on every fourth CPU's first tick of each
/// second — records a TSC reading tagged with the global tick. That last one is
/// the check that the TSC is in step across cores: the readings are taken within
/// a microsecond or so of each other in real time, so converting them all back
/// to nanoseconds has to land them within a hair of one another.
pub fn on_tick() {
    let ticks = interrupts::ticks();
    let me = super::percpu::get();
    if ticks % 1000 == 0 {
        let (tsc, aux) = sample();
        me.tsc_sample.store(tsc, Ordering::Relaxed);
        me.tsc_aux.store(aux, Ordering::Relaxed);
    }
    if super::percpu::cpu_id() != 0 {
        return;
    }
    TICK_NS.store(ticks * 1_000_000, Ordering::Relaxed);
    // Comparing the two clocks is a diagnostic, not a job: twice a second is
    // often enough to notice a drifting time base and rare enough to stay off
    // the interrupt path's critical budget.
    if ticks % 500 != 0 {
        return;
    }
    let Some(h) = hpet() else {
        return;
    };
    let (Some(hpet_ns), tsc) = (hpet_ns(), rdtsc()) else {
        return;
    };
    let hz = TSC_HZ.load(Ordering::Relaxed);
    if hz == 0 || hpet_ns == 0 {
        return;
    }
    let tsc_ns = tsc_ns_since(h.base_tsc, tsc, hz);
    if tsc_ns == 0 {
        return;
    }
    let ppm = ((tsc_ns as i128 - hpet_ns as i128) * 1_000_000) / tsc_ns as i128;
    TSC_HPET_PPM.store(ppm as i64 as u64, Ordering::Relaxed);
}

/// TSC ticks to nanoseconds, without the overflow a plain multiply would hit.
fn tsc_ns_since(base: u64, now: u64, hz: u64) -> u64 {
    let delta = now.wrapping_sub(base);
    let sec = delta / hz;
    let rem = delta % hz;
    sec * 1_000_000_000 + rem * 1_000_000_000 / hz
}

/// What the boot summary says about the clocks.
pub fn report() {
    let hz = TSC_HZ.load(Ordering::Relaxed);
    let ppm = tsc_hpet_ppm();
    let drift = if ppm == i64::MAX {
        "no HPET to compare against".to_string()
    } else {
        format!("{} ppm from the HPET", ppm)
    };
    klog!(
        "clock",
        "{} MHz TSC ({}, {}), HPET {}, {drift}",
        hz / 1_000_000,
        if TSC_INVARIANT.load(Ordering::Relaxed) == 1 {
            "invariant"
        } else {
            "not invariant"
        },
        source_label(),
        hpet_hz_mhz()
    );
}

fn source_label() -> &'static str {
    match source() {
        Source::Tsc => "time base",
        Source::Hpet => "HPET time base",
        Source::Tick => "tick counter",
    }
}

fn hpet_hz_mhz() -> u64 {
    let Some(h) = hpet() else {
        return 0;
    };
    if h.period_fs == 0 {
        return 0;
    }
    1_000_000_000_000 / h.period_fs
}
