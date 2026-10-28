//! Ethernet, ARP, IPv4, ICMP echo and UDP. One interface, one address, every
//! off-link and on-link packet goes through the gateway (good enough behind
//! QEMU's user-mode network; a routing table comes with the real stack).

use k1k_rt::{sleep_ms, uptime_ns};

use crate::virtio::Nic;

pub type Ip = [u8; 4];
pub type Mac = [u8; 6];

pub const UNSET: Ip = [0; 4];
const ETH: usize = 14;
const IPH: usize = 20;
const ID_ECHO: u16 = 0x4B31;

pub fn checksum(data: &[u8]) -> u16 {
    fold(sum(data, 0))
}

fn sum(data: &[u8], mut acc: u32) -> u32 {
    for pair in data.chunks(2) {
        acc += u16::from_be_bytes([pair[0], *pair.get(1).unwrap_or(&0)]) as u32;
    }
    acc
}

fn fold(mut acc: u32) -> u16 {
    while acc >> 16 != 0 {
        acc = (acc & 0xFFFF) + (acc >> 16);
    }
    !(acc as u16)
}

pub fn eth(buf: &mut [u8], dst: Mac, src: Mac, ethertype: u16) {
    buf[0..6].copy_from_slice(&dst);
    buf[6..12].copy_from_slice(&src);
    buf[12..14].copy_from_slice(&ethertype.to_be_bytes());
}

/// What `poll` found in a received frame that the caller may care about.
#[allow(dead_code)] // fields are the interface the socket layer will use
pub enum Event {
    /// Nothing for the caller (idle, or handled here: ARP, echo requests).
    None,
    Udp {
        src: Ip,
        sport: u16,
        dport: u16,
        off: usize,
        len: usize,
    },
    EchoReply {
        seq: u16,
        len: usize,
    },
}

pub struct Stack {
    pub nic: Nic,
    pub ip: Ip,
    pub gateway: Ip,
    pub gw_mac: Mac,
    pub frame: [u8; 1536],
}

impl Stack {
    pub fn new(nic: Nic, ip: Ip, gateway: Ip) -> Stack {
        Stack {
            nic,
            ip,
            gateway,
            gw_mac: [0xFF; 6],
            frame: [0; 1536],
        }
    }

    fn arp(&mut self, op: u16, dst_mac: Mac, target_mac: Mac, target_ip: Ip) {
        let mut f = [0u8; 42];
        eth(&mut f, dst_mac, self.nic.mac, 0x0806);
        f[14..20].copy_from_slice(&[0, 1, 8, 0, 6, 4]);
        f[20..22].copy_from_slice(&op.to_be_bytes());
        f[22..28].copy_from_slice(&self.nic.mac);
        f[28..32].copy_from_slice(&self.ip);
        f[32..38].copy_from_slice(&target_mac);
        f[38..42].copy_from_slice(&target_ip);
        self.nic.send(&f);
    }

    /// Learn the gateway's hardware address; false if it never answers.
    pub fn resolve_gateway(&mut self) -> bool {
        let start = uptime_ns();
        let mut last = 0;
        while uptime_ns() - start < 3_000_000_000 {
            if last == 0 || uptime_ns() - last > 500_000_000 {
                self.arp(1, [0xFF; 6], [0; 6], self.gateway);
                last = uptime_ns();
            }
            if let Some(n) = self.nic.recv(&mut self.frame) {
                let f = &self.frame[..n];
                if n >= 42
                    && f[12..14] == [8, 6]
                    && f[20..22] == [0, 2]
                    && f[28..32] == self.gateway
                {
                    self.gw_mac.copy_from_slice(&f[22..28]);
                    return true;
                }
            }
            sleep_ms(1);
        }
        false
    }

    /// Send an IPv4 packet; `payload` is the transport header and data.
    pub fn send_ip(&mut self, proto: u8, dst: Ip, payload: &[u8]) -> bool {
        let mut f = [0u8; 1536];
        let total = ETH + IPH + payload.len();
        if total > f.len() {
            return false;
        }
        let dst_mac = if dst == [255; 4] {
            [0xFF; 6]
        } else {
            self.gw_mac
        };
        eth(&mut f, dst_mac, self.nic.mac, 0x0800);
        let ip = &mut f[ETH..ETH + IPH];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&((IPH + payload.len()) as u16).to_be_bytes());
        ip[8] = 64;
        ip[9] = proto;
        ip[12..16].copy_from_slice(&self.ip);
        ip[16..20].copy_from_slice(&dst);
        let c = checksum(ip);
        ip[10..12].copy_from_slice(&c.to_be_bytes());
        f[ETH + IPH..total].copy_from_slice(payload);
        self.nic.send(&f[..total])
    }

    pub fn send_udp(&mut self, dst: Ip, sport: u16, dport: u16, data: &[u8]) -> bool {
        let mut p = [0u8; 1500];
        let len = 8 + data.len();
        if len > p.len() {
            return false;
        }
        p[0..2].copy_from_slice(&sport.to_be_bytes());
        p[2..4].copy_from_slice(&dport.to_be_bytes());
        p[4..6].copy_from_slice(&(len as u16).to_be_bytes());
        p[8..len].copy_from_slice(data);
        // Pseudo-header: addresses, protocol, UDP length.
        let mut acc = sum(&self.ip, 0);
        acc = sum(&dst, acc) + 17 + len as u32;
        let mut c = fold(sum(&p[..len], acc));
        if c == 0 {
            c = 0xFFFF;
        }
        p[6..8].copy_from_slice(&c.to_be_bytes());
        self.send_ip(17, dst, &p[..len])
    }

    pub fn send_echo(&mut self, dst: Ip, seq: u16) -> bool {
        let mut p = [0u8; 8 + 32];
        p[0] = 8;
        p[4..6].copy_from_slice(&ID_ECHO.to_be_bytes());
        p[6..8].copy_from_slice(&seq.to_be_bytes());
        for (i, b) in p[8..].iter_mut().enumerate() {
            *b = i as u8;
        }
        let c = checksum(&p);
        p[2..4].copy_from_slice(&c.to_be_bytes());
        self.send_ip(1, dst, &p)
    }

    /// Take one received frame, answer what the stack answers by itself and
    /// report what is left for the caller.
    pub fn poll(&mut self) -> Event {
        let Some(n) = self.nic.recv(&mut self.frame) else {
            return Event::None;
        };
        let f = &self.frame[..n];
        if n >= 42 && f[12..14] == [8, 6] && f[20..22] == [0, 1] && f[38..42] == self.ip {
            // Someone asks for our address.
            let (mut mac, mut ip) = ([0u8; 6], [0u8; 4]);
            mac.copy_from_slice(&f[22..28]);
            ip.copy_from_slice(&f[28..32]);
            self.arp(2, mac, mac, ip);
            return Event::None;
        }
        if n < ETH + IPH || f[12..14] != [8, 0] || f[ETH] >> 4 != 4 {
            return Event::None;
        }
        let ihl = (f[ETH] & 15) as usize * 4;
        let total = (u16::from_be_bytes([f[ETH + 2], f[ETH + 3]]) as usize).min(n - ETH);
        if ihl < IPH || total < ihl {
            return Event::None;
        }
        let (proto, mut src) = (f[ETH + 9], [0u8; 4]);
        src.copy_from_slice(&f[ETH + 12..ETH + 16]);
        let dst_ok = f[ETH + 16..ETH + 20] == self.ip || f[ETH + 16..ETH + 20] == [255; 4];
        let t = ETH + ihl;
        let tlen = total - ihl;
        match proto {
            1 if tlen >= 8 && f[ETH + 16..ETH + 20] == self.ip => {
                let seq = u16::from_be_bytes([f[t + 6], f[t + 7]]);
                match f[t] {
                    0 if f[t + 4..t + 6] == ID_ECHO.to_be_bytes() => {
                        Event::EchoReply { seq, len: tlen }
                    }
                    8 => {
                        let mut reply = [0u8; 1500];
                        let l = tlen.min(reply.len());
                        reply[..l].copy_from_slice(&f[t..t + l]);
                        reply[0] = 0;
                        reply[2..4].copy_from_slice(&[0, 0]);
                        let c = checksum(&reply[..l]);
                        reply[2..4].copy_from_slice(&c.to_be_bytes());
                        self.send_ip(1, src, &reply[..l]);
                        Event::None
                    }
                    _ => Event::None,
                }
            }
            17 if dst_ok && tlen >= 8 => Event::Udp {
                src,
                sport: u16::from_be_bytes([f[t], f[t + 1]]),
                dport: u16::from_be_bytes([f[t + 2], f[t + 3]]),
                off: t + 8,
                len: tlen - 8,
            },
            _ => Event::None,
        }
    }
}
