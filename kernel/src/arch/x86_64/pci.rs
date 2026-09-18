//! PCI configuration-space scan over the legacy `0xCF8/0xCFC` mechanism.
//! Enumeration only: drivers will live in ring 3 and get their devices
//! handed over as capabilities.

use alloc::vec::Vec;
use spin::Once;
use x86_64::instructions::port::Port;

use crate::klog;

#[derive(Debug, Clone, Copy)]
pub struct PciDevice {
    pub bus: u8,
    pub slot: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
}

static DEVICES: Once<Vec<PciDevice>> = Once::new();

fn read32(bus: u8, slot: u8, func: u8, off: u8) -> u32 {
    let addr = 0x8000_0000
        | (bus as u32) << 16
        | (slot as u32) << 11
        | (func as u32) << 8
        | (off as u32 & 0xFC);
    unsafe {
        Port::<u32>::new(0xCF8).write(addr);
        Port::<u32>::new(0xCFC).read()
    }
}

fn probe(bus: u8, slot: u8, func: u8, out: &mut Vec<PciDevice>) -> bool {
    let id = read32(bus, slot, func, 0);
    let vendor = (id & 0xFFFF) as u16;
    if vendor == 0xFFFF {
        return false;
    }
    let class = read32(bus, slot, func, 8);
    out.push(PciDevice {
        bus,
        slot,
        func,
        vendor,
        device: (id >> 16) as u16,
        class: (class >> 24) as u8,
        subclass: (class >> 16) as u8,
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
        // Buses beyond the last populated one are empty on every machine we care about.
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
    }
    klog!("pci", "{} function(s) found", list.len());
    DEVICES.call_once(|| list);
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
