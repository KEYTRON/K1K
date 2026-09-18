//! Minimal ACPI: locate the RSDP handed over by Limine, walk RSDT/XSDT and
//! parse the MADT for Local APIC / I/O APIC / interrupt-override information.

use alloc::vec::Vec;
use spin::Once;
use x86_64::PhysAddr;

use crate::klog;
use crate::mm::vmm;

#[derive(Debug, Clone, Copy)]
pub struct IoApicInfo {
    pub id: u8,
    pub address: u32,
    pub gsi_base: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct IrqOverride {
    pub isa_irq: u8,
    pub gsi: u32,
    pub active_low: bool,
    pub level_triggered: bool,
}

#[derive(Debug, Default)]
pub struct Madt {
    pub lapic_address: u64,
    pub cpus: Vec<(u8, u8)>, // (acpi processor id, apic id) — enabled only
    pub ioapics: Vec<IoApicInfo>,
    pub overrides: Vec<IrqOverride>,
    pub has_legacy_pics: bool,
}

static MADT: Once<Madt> = Once::new();

pub fn madt() -> &'static Madt {
    MADT.get().expect("acpi not initialised")
}

/// Map `len` bytes at physical `phys` through the HHDM and return them.
fn map(phys: u64, len: usize) -> &'static [u8] {
    vmm::map_phys_hhdm(PhysAddr::new(phys), len as u64);
    let va = crate::mm::pmm::phys_to_virt(PhysAddr::new(phys));
    unsafe { core::slice::from_raw_parts(va.as_ptr::<u8>(), len) }
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

fn checksum_ok(b: &[u8]) -> bool {
    b.iter().fold(0u8, |a, &x| a.wrapping_add(x)) == 0
}

/// Read a standard table header at `phys`, returning (signature, whole table).
fn table(phys: u64) -> Option<(&'static [u8; 4], &'static [u8])> {
    let hdr = map(phys, 36);
    let len = u32_at(hdr, 4) as usize;
    if !(36..0x10_0000).contains(&len) {
        return None;
    }
    let full = map(phys, len);
    let sig: &[u8; 4] = full[..4].try_into().ok()?;
    checksum_ok(full).then_some((sig, full))
}

pub fn init(rsdp_phys: u64) {
    let rsdp = map(rsdp_phys, 36);
    assert!(&rsdp[..8] == b"RSD PTR ", "bad RSDP signature");
    let revision = rsdp[15];
    let (root, entry_size) = if revision >= 2 && checksum_ok(&rsdp[..36]) {
        (u64_at(rsdp, 24), 8)
    } else {
        assert!(checksum_ok(&rsdp[..20]), "bad RSDP checksum");
        (u32_at(rsdp, 16) as u64, 4)
    };

    let (sig, sdt) = table(root).expect("invalid RSDT/XSDT");
    klog!(
        "acpi",
        "RSDP rev {} -> {} at {:#x}, {} entries",
        revision,
        core::str::from_utf8(sig).unwrap_or("?"),
        root,
        (sdt.len() - 36) / entry_size
    );

    let mut madt = Madt::default();
    let mut found = false;
    for i in 0..(sdt.len() - 36) / entry_size {
        let off = 36 + i * entry_size;
        let addr = if entry_size == 8 {
            u64_at(sdt, off)
        } else {
            u32_at(sdt, off) as u64
        };
        let Some((sig, body)) = table(addr) else {
            continue;
        };
        if sig == b"APIC" {
            parse_madt(body, &mut madt);
            found = true;
        }
    }
    assert!(found, "no MADT found");

    klog!(
        "acpi",
        "MADT: lapic {:#x}, {} cpu(s), {} ioapic(s), {} override(s){}",
        madt.lapic_address,
        madt.cpus.len(),
        madt.ioapics.len(),
        madt.overrides.len(),
        if madt.has_legacy_pics {
            ", legacy PICs"
        } else {
            ""
        }
    );
    MADT.call_once(|| madt);
}

fn parse_madt(t: &[u8], out: &mut Madt) {
    out.lapic_address = u32_at(t, 36) as u64;
    out.has_legacy_pics = u32_at(t, 40) & 1 != 0;
    let mut off = 44;
    while off + 2 <= t.len() {
        let kind = t[off];
        let len = t[off + 1] as usize;
        if len < 2 || off + len > t.len() {
            break;
        }
        let e = &t[off..off + len];
        match kind {
            0 if len >= 8 => {
                if u32_at(e, 4) & 1 != 0 {
                    out.cpus.push((e[2], e[3]));
                }
            }
            1 if len >= 12 => out.ioapics.push(IoApicInfo {
                id: e[2],
                address: u32_at(e, 4),
                gsi_base: u32_at(e, 8),
            }),
            2 if len >= 10 => {
                let flags = u16::from_le_bytes([e[8], e[9]]);
                out.overrides.push(IrqOverride {
                    isa_irq: e[3],
                    gsi: u32_at(e, 4),
                    active_low: flags & 0b11 == 0b11,
                    level_triggered: (flags >> 2) & 0b11 == 0b11,
                });
            }
            5 if len >= 12 => out.lapic_address = u64_at(e, 4),
            _ => {}
        }
        off += len;
    }
}
