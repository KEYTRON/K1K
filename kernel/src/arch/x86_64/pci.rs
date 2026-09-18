//! PCI configuration-space scan over the legacy `0xCF8/0xCFC` mechanism.
//! The kernel only enumerates and hands functions to ring-3 drivers as
//! `Device` capabilities; it never talks to the devices itself.

use alloc::vec::Vec;
use spin::Once;
use x86_64::PhysAddr;
use x86_64::instructions::port::Port;

use crate::klog;

#[derive(Debug, Clone, Copy, Default)]
pub struct Bar {
    pub base: u64,
    pub size: u64,
    pub io: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct PciDevice {
    pub bus: u8,
    pub slot: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub bars: [Bar; 6],
}

static DEVICES: Once<Vec<PciDevice>> = Once::new();

fn cfg_addr(bus: u8, slot: u8, func: u8, off: u8) -> u32 {
    0x8000_0000
        | (bus as u32) << 16
        | (slot as u32) << 11
        | (func as u32) << 8
        | (off as u32 & 0xFC)
}

fn read32(bus: u8, slot: u8, func: u8, off: u8) -> u32 {
    unsafe {
        Port::<u32>::new(0xCF8).write(cfg_addr(bus, slot, func, off));
        Port::<u32>::new(0xCFC).read()
    }
}

fn write32(bus: u8, slot: u8, func: u8, off: u8, v: u32) {
    unsafe {
        Port::<u32>::new(0xCF8).write(cfg_addr(bus, slot, func, off));
        Port::<u32>::new(0xCFC).write(v);
    }
}

fn read_bars(bus: u8, slot: u8, func: u8) -> [Bar; 6] {
    let mut bars = [Bar::default(); 6];
    let mut i = 0;
    while i < 6 {
        let off = 0x10 + 4 * i as u8;
        let orig = read32(bus, slot, func, off);
        if orig == 0 {
            i += 1;
            continue;
        }
        write32(bus, slot, func, off, 0xFFFF_FFFF);
        let probe = read32(bus, slot, func, off);
        write32(bus, slot, func, off, orig);

        if orig & 1 != 0 {
            let mask = probe & !0x3;
            bars[i] = Bar {
                base: (orig & !0x3) as u64,
                size: (!mask).wrapping_add(1) as u64 & 0xFFFF,
                io: true,
            };
            i += 1;
        } else if (orig >> 1) & 0x3 == 0x2 {
            let off_hi = off + 4;
            let orig_hi = read32(bus, slot, func, off_hi);
            write32(bus, slot, func, off_hi, 0xFFFF_FFFF);
            let probe_hi = read32(bus, slot, func, off_hi);
            write32(bus, slot, func, off_hi, orig_hi);
            let mask = ((probe_hi as u64) << 32 | (probe & !0xF) as u64) as u64;
            bars[i] = Bar {
                base: (orig_hi as u64) << 32 | (orig & !0xF) as u64,
                size: (!mask).wrapping_add(1),
                io: false,
            };
            i += 2;
        } else {
            let mask = probe & !0xF;
            bars[i] = Bar {
                base: (orig & !0xF) as u64,
                size: (!mask).wrapping_add(1) as u64 & 0xFFFF_FFFF,
                io: false,
            };
            i += 1;
        }
    }
    bars
}

fn probe(bus: u8, slot: u8, func: u8, out: &mut Vec<PciDevice>) -> bool {
    let id = read32(bus, slot, func, 0);
    let vendor = (id & 0xFFFF) as u16;
    if vendor == 0xFFFF {
        return false;
    }
    let class = read32(bus, slot, func, 8);
    let header = (read32(bus, slot, func, 0x0C) >> 16) as u8 & 0x7F;
    let bars = if header == 0 {
        read_bars(bus, slot, func)
    } else {
        [Bar::default(); 6]
    };
    out.push(PciDevice {
        bus,
        slot,
        func,
        vendor,
        device: (id >> 16) as u16,
        class: (class >> 24) as u8,
        subclass: (class >> 16) as u8,
        bars,
    });
    true
}

pub fn init() {
    let mut list = Vec::new();
    for bus in 0..=255u8 {
        let mut any = false;
        for slot in 0..32u8 {
            if !probe(bus, slot, 0, &mut list) {
                continue;
            }
            any = true;
            let header = (read32(bus, slot, 0, 0x0C) >> 16) as u8;
            if header & 0x80 != 0 {
                for func in 1..8u8 {
                    probe(bus, slot, func, &mut list);
                }
            }
        }
        if !any && bus > 8 {
            break;
        }
    }
    for d in &list {
        klog!(
            "pci",
            "{:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}.{:02x} ({})",
            d.bus,
            d.slot,
            d.func,
            d.vendor,
            d.device,
            d.class,
            d.subclass,
            class_name(d.class, d.subclass)
        );
        for (i, b) in d.bars.iter().enumerate() {
            if b.size != 0 {
                klog!(
                    "pci",
                    "    bar{} {} {:#x} size {:#x}",
                    i,
                    if b.io { "io " } else { "mem" },
                    b.base,
                    b.size
                );
            }
        }
    }
    klog!("pci", "{} function(s) found", list.len());
    DEVICES.call_once(|| list);
}

pub fn find(class: u8, subclass: u8) -> Option<PciDevice> {
    DEVICES
        .get()?
        .iter()
        .copied()
        .find(|d| d.class == class && d.subclass == subclass)
}

/// Turn on memory decoding and bus mastering so a ring-3 driver can use the
/// function's MMIO registers and DMA.
pub fn enable(d: &PciDevice) {
    let cmd = read32(d.bus, d.slot, d.func, 0x04);
    write32(d.bus, d.slot, d.func, 0x04, cmd | 0x6);
}

fn read16(d: &PciDevice, off: u8) -> u16 {
    (read32(d.bus, d.slot, d.func, off) >> ((off & 2) * 8)) as u16
}

fn write16(d: &PciDevice, off: u8, v: u16) {
    let shift = (off & 2) * 8;
    let old = read32(d.bus, d.slot, d.func, off);
    let new = (old & !(0xFFFF << shift)) | (v as u32) << shift;
    write32(d.bus, d.slot, d.func, off, new);
}

/// Walk the capability list for `id`; returns the capability's offset.
fn find_capability(d: &PciDevice, id: u8) -> Option<u8> {
    let status = read16(d, 0x06);
    if status & 0x10 == 0 {
        return None;
    }
    let mut ptr = (read32(d.bus, d.slot, d.func, 0x34) & 0xFC) as u8;
    for _ in 0..48 {
        if ptr == 0 {
            return None;
        }
        let hdr = read32(d.bus, d.slot, d.func, ptr);
        if (hdr & 0xFF) as u8 == id {
            return Some(ptr);
        }
        ptr = ((hdr >> 8) & 0xFC) as u8;
    }
    None
}

/// Point MSI-X table entry `index` at `vector` on `lapic_id` and enable MSI-X.
/// The table lives in one of the function's BARs, reached through the HHDM.
pub fn msix_enable(d: &PciDevice, index: u32, vector: u8, lapic_id: u8) -> Option<()> {
    let cap = find_capability(d, 0x11)?;
    let ctrl = read16(d, cap + 2);
    let table_size = (ctrl & 0x7FF) as u32 + 1;
    if index >= table_size {
        return None;
    }
    let table = read32(d.bus, d.slot, d.func, cap + 4);
    let bir = (table & 0x7) as usize;
    let offset = (table & !0x7) as u64;
    let bar = d.bars.get(bir)?;
    if bar.size == 0 || bar.io {
        return None;
    }
    let phys = bar.base + offset;
    crate::mm::vmm::map_mmio(PhysAddr::new(phys), table_size as u64 * 16);
    let entry =
        crate::mm::pmm::phys_to_virt(PhysAddr::new(phys + index as u64 * 16)).as_mut_ptr::<u32>();
    unsafe {
        entry
            .add(0)
            .write_volatile(0xFEE0_0000 | (lapic_id as u32) << 12);
        entry.add(1).write_volatile(0);
        entry.add(2).write_volatile(vector as u32);
        entry.add(3).write_volatile(0); // unmasked
    }
    // Enable MSI-X, clear the function mask; INTx is no longer needed.
    write16(d, cap + 2, (ctrl | 0x8000) & !0x4000);
    let cmd = read32(d.bus, d.slot, d.func, 0x04);
    write32(d.bus, d.slot, d.func, 0x04, cmd | 1 << 10);
    Some(())
}

fn class_name(class: u8, sub: u8) -> &'static str {
    match (class, sub) {
        (0x01, 0x01) => "IDE controller",
        (0x01, 0x06) => "SATA/AHCI controller",
        (0x01, 0x08) => "NVMe controller",
        (0x01, _) => "storage",
        (0x02, 0x00) => "ethernet",
        (0x02, _) => "network",
        (0x03, _) => "display",
        (0x04, _) => "multimedia",
        (0x05, _) => "memory controller",
        (0x06, 0x00) => "host bridge",
        (0x06, 0x01) => "ISA bridge",
        (0x06, 0x04) => "PCI-PCI bridge",
        (0x06, _) => "bridge",
        (0x0C, 0x03) => "USB controller",
        (0x0C, 0x05) => "SMBus",
        (0x0C, _) => "serial bus",
        (0xFF, _) => "unassigned",
        _ => "other",
    }
}
