//! blk — the storage driver, running in ring 3. The kernel hands it the NVMe
//! controller as a device capability (slot 0); it maps BAR0, brings the
//! controller up over DMA pages and reads the first block of the disk.
#![no_std]
#![no_main]

mod nvme;

use k1k_rt::{Cap, dev_info, dev_map, exit, log, sleep_ms};

const DEVICE: Cap = Cap(0);

fn trim(bytes: &[u8]) -> &str {
    let end = bytes
        .iter()
        .rposition(|&b| b != b' ' && b != 0)
        .map_or(0, |i| i + 1);
    core::str::from_utf8(&bytes[..end]).unwrap_or("?")
}

fn main() -> ! {
    let info = match dev_info(DEVICE) {
        Ok(i) => i,
        Err(e) => {
            log!("no storage device capability ({:?}); nothing to drive", e);
            exit(0);
        }
    };
    log!(
        "pci {:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}.{:02x}, bar0 {:#x} ({} KiB)",
        info.bus,
        info.slot,
        info.func,
        info.vendor,
        info.device,
        info.class,
        info.subclass,
        info.bars[0].base,
        info.bars[0].size / 1024
    );
    let regs = match dev_map(DEVICE, 0) {
        Ok(p) => p,
        Err(e) => {
            log!("cannot map BAR0: {:?}", e);
            exit(2);
        }
    };
    log!("BAR0 mapped at {:p}", regs);

    let mut dev = match nvme::Nvme::init(regs) {
        Ok(d) => d,
        Err(e) => {
            log!("nvme init failed: {:?}", e);
            exit(2);
        }
    };
    log!(
        "nvme ready: model \"{}\" serial \"{}\", {} blocks x {} B = {} MiB",
        trim(&dev.model),
        trim(&dev.serial),
        dev.blocks,
        dev.block_size,
        dev.blocks * dev.block_size as u64 / (1024 * 1024)
    );

    let buf = match nvme::dma_page() {
        Ok(b) => b,
        Err(e) => {
            log!("dma page: {:?}", e);
            exit(2);
        }
    };
    match dev.read(0, 1, &buf) {
        Ok(()) => {
            let sector = unsafe { core::slice::from_raw_parts(buf.virt, dev.block_size as usize) };
            let end = sector.iter().position(|&b| b == 0).unwrap_or(64).min(64);
            log!(
                "sector 0: \"{}\"",
                core::str::from_utf8(&sector[..end]).unwrap_or("<binary>")
            );
        }
        Err(e) => {
            log!("read of sector 0 failed: {:?}", e);
            exit(2);
        }
    }
    let _ = dev.doorbell_sanity();

    loop {
        sleep_ms(5000);
    }
}

k1k_rt::main!(main);
