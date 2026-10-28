//! A stub resolver: one A-record question over UDP to the DHCP-provided server.

use k1k_rt::{sleep_ms, uptime_ns};

use crate::stack::{Event, Ip, Stack};

const ID: u16 = 0x4B44;
const PORT: u16 = 40053;

pub fn resolve(s: &mut Stack, server: Ip, name: &str) -> Option<Ip> {
    let mut q = [0u8; 300];
    q[0..2].copy_from_slice(&ID.to_be_bytes());
    q[2..4].copy_from_slice(&0x0100u16.to_be_bytes()); // recursion desired
    q[5] = 1; // one question
    let mut o = 12;
    for label in name.split('.') {
        if label.is_empty() || label.len() > 63 || o + label.len() + 6 > q.len() {
            return None;
        }
        q[o] = label.len() as u8;
        q[o + 1..o + 1 + label.len()].copy_from_slice(label.as_bytes());
        o += 1 + label.len();
    }
    o += 1; // root label
    q[o..o + 4].copy_from_slice(&[0, 1, 0, 1]); // type A, class IN
    o += 4;

    for _ in 0..3 {
        s.send_udp(server, PORT, 53, &q[..o]);
        let start = uptime_ns();
        while uptime_ns() - start < 1_500_000_000 {
            if let Event::Udp {
                sport: 53,
                dport: PORT,
                off,
                len,
                ..
            } = s.poll()
            {
                if let Some(ip) = answer(&s.frame[off..off + len]) {
                    return Some(ip);
                }
            }
            sleep_ms(1);
        }
    }
    None
}

/// The first A record in a response to our question.
fn answer(r: &[u8]) -> Option<Ip> {
    if r.len() < 12 || r[0..2] != ID.to_be_bytes() || r[2] & 0x80 == 0 || r[3] & 15 != 0 {
        return None;
    }
    let (qd, an) = (
        u16::from_be_bytes([r[4], r[5]]),
        u16::from_be_bytes([r[6], r[7]]),
    );
    let mut i = 12;
    for _ in 0..qd {
        i = skip_name(r, i)? + 4;
    }
    for _ in 0..an {
        i = skip_name(r, i)?;
        let hdr = r.get(i..i + 10)?;
        let rdlen = u16::from_be_bytes([hdr[8], hdr[9]]) as usize;
        if hdr[0..2] == [0, 1] && hdr[2..4] == [0, 1] && rdlen == 4 {
            return r.get(i + 10..i + 14)?.try_into().ok();
        }
        i += 10 + rdlen;
    }
    None
}

fn skip_name(r: &[u8], mut i: usize) -> Option<usize> {
    loop {
        let l = *r.get(i)? as usize;
        if l == 0 {
            return Some(i + 1);
        }
        if l & 0xC0 == 0xC0 {
            return Some(i + 2);
        }
        i += 1 + l;
    }
}
