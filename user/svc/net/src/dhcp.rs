//! A DHCP client: DISCOVER → OFFER → REQUEST → ACK over raw UDP frames.

use k1k_rt::{sleep_ms, uptime_ns};

use crate::stack::{UNSET, checksum, eth};
use crate::virtio::Nic;

pub struct Lease {
    pub ip: [u8; 4],
    pub router: [u8; 4],
    pub dns: [u8; 4],
}

const XID: [u8; 4] = [0x4B, 0x31, 0x4B, 0x31];

/// Broadcast one DHCP message (`kind` 1 = discover, 3 = request).
fn send(nic: &mut Nic, kind: u8, want: Option<([u8; 4], [u8; 4])>) {
    let mut f = [0u8; 14 + 20 + 8 + 300];
    eth(&mut f, [0xFF; 6], nic.mac, 0x0800);
    let d = &mut f[42..];
    d[0] = 1; // BOOTREQUEST
    d[1] = 1; // Ethernet
    d[2] = 6;
    d[4..8].copy_from_slice(&XID);
    d[10] = 0x80; // ask for a broadcast reply
    d[28..34].copy_from_slice(&nic.mac);
    d[236..240].copy_from_slice(&[99, 130, 83, 99]); // magic cookie
    let mut o = 240;
    d[o..o + 3].copy_from_slice(&[53, 1, kind]);
    o += 3;
    if let Some((ip, server)) = want {
        d[o..o + 6].copy_from_slice(&[50, 4, ip[0], ip[1], ip[2], ip[3]]);
        o += 6;
        d[o..o + 6].copy_from_slice(&[54, 4, server[0], server[1], server[2], server[3]]);
        o += 6;
    }
    d[o..o + 5].copy_from_slice(&[55, 3, 1, 3, 6]); // parameters: mask, router, dns
    o += 5;
    d[o] = 255;
    o += 1;

    let udp_len = 8 + o;
    let ip = &mut f[14..34];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&((20 + udp_len) as u16).to_be_bytes());
    ip[8] = 64;
    ip[9] = 17;
    ip[16..20].copy_from_slice(&[255; 4]);
    let c = checksum(ip);
    ip[10..12].copy_from_slice(&c.to_be_bytes());
    let u = &mut f[34..];
    u[0..2].copy_from_slice(&68u16.to_be_bytes());
    u[2..4].copy_from_slice(&67u16.to_be_bytes());
    u[4..6].copy_from_slice(&(udp_len as u16).to_be_bytes());
    // The UDP checksum is optional over IPv4 and left zero.
    let total = 14 + 20 + udp_len;
    nic.send(&f[..total]);
}

/// Wait for a reply of `kind` with our transaction id; returns the offered
/// address, the server id and the options we asked for.
fn wait(nic: &mut Nic, kind: u8) -> Option<Lease> {
    let mut frame = [0u8; 1536];
    let start = uptime_ns();
    while uptime_ns() - start < 2_000_000_000 {
        while let Some(n) = nic.recv(&mut frame) {
            let f = &frame[..n];
            if n < 14 + 20 + 8 + 240 || f[12] != 0x08 || f[13] != 0x00 || f[23] != 17 {
                continue;
            }
            let d = &f[42..];
            if d[0] != 2 || d[4..8] != XID || d[236..240] != [99, 130, 83, 99] {
                continue;
            }
            let mut l = Lease {
                ip: [0; 4],
                router: UNSET,
                dns: UNSET,
            };
            l.ip.copy_from_slice(&d[16..20]);
            let mut got = 0;
            let mut server = UNSET;
            let mut i = 240;
            while i + 1 < d.len() && d[i] != 255 {
                if d[i] == 0 {
                    i += 1;
                    continue;
                }
                let (code, len) = (d[i], d[i + 1] as usize);
                let v = d.get(i + 2..i + 2 + len)?;
                match code {
                    53 => got = v[0],
                    54 if len >= 4 => server.copy_from_slice(&v[..4]),
                    3 if len >= 4 => l.router.copy_from_slice(&v[..4]),
                    6 if len >= 4 => l.dns.copy_from_slice(&v[..4]),
                    _ => {}
                }
                i += 2 + len;
            }
            if got == kind {
                // Carry the server id in `dns` slot only for the OFFER → REQUEST hop.
                SERVER.with(server);
                return Some(l);
            }
        }
        sleep_ms(1);
    }
    None
}

struct Server(core::cell::Cell<[u8; 4]>);
unsafe impl Sync for Server {}
static SERVER: Server = Server(core::cell::Cell::new([0; 4]));
impl Server {
    fn with(&self, s: [u8; 4]) {
        self.0.set(s);
    }
}

pub fn acquire(nic: &mut Nic) -> Option<Lease> {
    for _ in 0..3 {
        send(nic, 1, None);
        let Some(offer) = wait(nic, 2) else { continue };
        let server = SERVER.0.get();
        send(nic, 3, Some((offer.ip, server)));
        if let Some(ack) = wait(nic, 5) {
            return Some(ack);
        }
    }
    None
}
