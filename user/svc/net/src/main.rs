//! net — the network driver, running in ring 3. The kernel hands it a virtio
//! network device (slot 0). For now it speaks just enough Ethernet, ARP, IPv4
//! and ICMP to find the gateway and ping it; sockets come on top of this.
#![no_std]
#![no_main]

mod virtio;

use k1k_rt::{Cap, dev_info, exit, log, sleep_ms, uptime_ns};
use virtio::Nic;

const DEVICE: Cap = Cap(0);

/// QEMU's user-mode network: us, the gateway, and a mask of /24.
const OUR_IP: [u8; 4] = [10, 0, 2, 15];
const GATEWAY: [u8; 4] = [10, 0, 2, 2];

fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    for pair in data.chunks(2) {
        sum += u16::from_be_bytes([pair[0], *pair.get(1).unwrap_or(&0)]) as u32;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

fn eth(buf: &mut [u8], dst: [u8; 6], src: [u8; 6], ethertype: u16) {
    buf[0..6].copy_from_slice(&dst);
    buf[6..12].copy_from_slice(&src);
    buf[12..14].copy_from_slice(&ethertype.to_be_bytes());
}

fn arp_request(nic: &mut Nic, target: [u8; 4]) {
    let mut f = [0u8; 42];
    eth(&mut f, [0xFF; 6], nic.mac, 0x0806);
    f[14..22].copy_from_slice(&[0, 1, 8, 0, 6, 4, 0, 1]);
    f[22..28].copy_from_slice(&nic.mac);
    f[28..32].copy_from_slice(&OUR_IP);
    f[38..42].copy_from_slice(&target);
    nic.send(&f);
}

fn echo_request(nic: &mut Nic, dst_mac: [u8; 6], dst: [u8; 4], seq: u16) {
    let mut f = [0u8; 14 + 20 + 8 + 32];
    eth(&mut f, dst_mac, nic.mac, 0x0800);
    let ip = &mut f[14..34];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&((20 + 8 + 32) as u16).to_be_bytes());
    ip[8] = 64;
    ip[9] = 1;
    ip[12..16].copy_from_slice(&OUR_IP);
    ip[16..20].copy_from_slice(&dst);
    let c = checksum(ip);
    ip[10..12].copy_from_slice(&c.to_be_bytes());
    let icmp = &mut f[34..];
    icmp[0] = 8;
    icmp[4..6].copy_from_slice(&0x4B31u16.to_be_bytes());
    icmp[6..8].copy_from_slice(&seq.to_be_bytes());
    for (i, b) in icmp[8..].iter_mut().enumerate() {
        *b = i as u8;
    }
    let c = checksum(icmp);
    icmp[2..4].copy_from_slice(&c.to_be_bytes());
    nic.send(&f);
}

fn main() -> ! {
    let info = match dev_info(DEVICE) {
        Ok(i) => i,
        Err(e) => {
            log!("no network device capability ({:?}); nothing to drive", e);
            exit(0);
        }
    };
    log!(
        "pci {:02x}:{:02x}.{} {:04x}:{:04x}",
        info.bus,
        info.slot,
        info.func,
        info.vendor,
        info.device
    );
    let mut nic = match Nic::init(DEVICE) {
        Ok(n) => n,
        Err(e) => {
            log!("virtio-net init failed: {:?}", e);
            exit(2);
        }
    };
    let m = nic.mac;
    log!(
        "virtio-net ready, mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, address {}.{}.{}.{}",
        m[0],
        m[1],
        m[2],
        m[3],
        m[4],
        m[5],
        OUR_IP[0],
        OUR_IP[1],
        OUR_IP[2],
        OUR_IP[3]
    );

    // Resolve the gateway, then ping it.
    let mut frame = [0u8; 1536];
    let mut gw_mac = None;
    let start = uptime_ns();
    let mut last_arp = 0u64;
    while gw_mac.is_none() && uptime_ns() - start < 3_000_000_000 {
        if uptime_ns() - last_arp > 500_000_000 || last_arp == 0 {
            arp_request(&mut nic, GATEWAY);
            last_arp = uptime_ns();
        }
        while let Some(n) = nic.recv(&mut frame) {
            let f = &frame[..n];
            if n >= 42
                && f[12] == 0x08
                && f[13] == 0x06
                && f[20] == 0
                && f[21] == 2
                && f[28..32] == GATEWAY
            {
                let mut mac = [0u8; 6];
                mac.copy_from_slice(&f[22..28]);
                gw_mac = Some(mac);
            }
        }
        sleep_ms(1);
    }
    let Some(gw_mac) = gw_mac else {
        log!(
            "gateway {}.{}.{}.{} did not answer ARP",
            GATEWAY[0],
            GATEWAY[1],
            GATEWAY[2],
            GATEWAY[3]
        );
        exit(1);
    };
    log!(
        "gateway is at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        gw_mac[0],
        gw_mac[1],
        gw_mac[2],
        gw_mac[3],
        gw_mac[4],
        gw_mac[5]
    );

    echo_request(&mut nic, gw_mac, GATEWAY, 1);
    let sent = uptime_ns();
    while uptime_ns() - sent < 3_000_000_000 {
        while let Some(n) = nic.recv(&mut frame) {
            let f = &frame[..n];
            if n >= 42
                && f[12] == 0x08
                && f[13] == 0x00
                && f[23] == 1
                && f[34] == 0
                && f[38..40] == 0x4B31u16.to_be_bytes()
            {
                log!(
                    "ping {}.{}.{}.{}: reply, {} bytes",
                    GATEWAY[0],
                    GATEWAY[1],
                    GATEWAY[2],
                    GATEWAY[3],
                    n - 34
                );
                exit(0);
            }
        }
        sleep_ms(1);
    }
    log!("ping: no reply");
    exit(1);
}

k1k_rt::main!(main);
