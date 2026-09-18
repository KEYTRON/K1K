//! blk — the storage driver, running in ring 3. The kernel hands it the NVMe
//! controller as a device capability (slot 0) and a request endpoint (slot 1).
//! Clients attach a DMA buffer and a reply endpoint, then ask for block reads.
#![no_std]
#![no_main]

mod nvme;

use k1k_rt::{Cap, blkproto as proto, dev_info, dev_map, exit, log, mem_map, mem_phys, recv, send};

const DEVICE: Cap = Cap(0);
const REQUESTS: Cap = Cap(1);

fn trim(bytes: &[u8]) -> &str {
    let end = bytes
        .iter()
        .rposition(|&b| b != b' ' && b != 0)
        .map_or(0, |i| i + 1);
    core::str::from_utf8(&bytes[..end]).unwrap_or("?")
}

struct Client {
    buf_phys: u64,
    buf_pages: u64,
    reply: Option<Cap>,
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

    let mut client = Client {
        buf_phys: 0,
        buf_pages: 0,
        reply: None,
    };
    log!("serving block requests");
    loop {
        let m = match recv(REQUESTS) {
            Ok(m) => m,
            Err(e) => {
                log!("recv failed: {:?}", e);
                exit(2);
            }
        };
        // Opcode in the low half of word 0; send_cap carries only one word,
        // so REGISTER_BUF packs its page count into the high half.
        let op = m.words[0] & 0xFFFF_FFFF;
        let arg = m.words[0] >> 32;
        match op {
            proto::REGISTER_BUF => {
                let Some(cap) = m.cap else {
                    log!("REGISTER_BUF without a capability from task {}", m.sender);
                    continue;
                };
                match (mem_map(cap, true), mem_phys(cap)) {
                    (Ok(_), Ok(phys)) => {
                        client.buf_phys = phys;
                        client.buf_pages = arg;
                        log!(
                            "task {} attached a {} KiB DMA buffer at {:#x}",
                            m.sender,
                            arg * 4,
                            phys
                        );
                    }
                    (a, b) => log!("cannot use buffer from task {}: {:?} {:?}", m.sender, a, b),
                }
            }
            proto::SET_REPLY => {
                client.reply = m.cap;
            }
            proto::READ => {
                let lba = m.words[1];
                let count = m.words[2];
                let Some(reply) = client.reply else { continue };
                let max = (client.buf_pages * 4096 / dev.block_size as u64).min(proto::MAX_BLOCKS);
                if client.buf_phys == 0 || count == 0 || count > max {
                    let _ = send(reply, proto::STATUS_ERR, 0, dev.block_size as u64);
                    continue;
                }
                let status = match dev.read_phys(lba, count as u16, client.buf_phys) {
                    Ok(()) => proto::STATUS_OK,
                    Err(e) => {
                        log!("read lba {} x{} failed: {:?}", lba, count, e);
                        proto::STATUS_ERR
                    }
                };
                let _ = send(reply, status, count, dev.block_size as u64);
            }
            other => log!("unknown request {} from task {}", other, m.sender),
        }
    }
}

k1k_rt::main!(main);
