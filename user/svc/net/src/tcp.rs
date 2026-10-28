//! A TCP client: one connection, in-order receive, stop-and-wait send with
//! retransmission. Enough to fetch a file over HTTP; windows, congestion
//! control and many connections come with the real stack.

use k1k_rt::{sleep_ms, uptime_ns};

use crate::stack::{Event, Ip, Stack, fold, sum};

pub const MSS: usize = 1360;
const RX: usize = 16384;

const FIN: u8 = 1;
const SYN: u8 = 2;
const RST: u8 = 4;
const PSH: u8 = 8;
const ACK: u8 = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Closed,
    SynSent,
    Established,
    Reset,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Fail {
    Timeout,
    Refused,
    Closed,
}

pub struct Tcp {
    pub state: State,
    lport: u16,
    rip: Ip,
    rport: u16,
    snd_una: u32,
    snd_nxt: u32,
    rcv_nxt: u32,
    rx: [u8; RX],
    head: usize,
    len: usize,
    fin: bool,
    next_port: u16,
}

/// `a` is at or after `b` in sequence space.
fn geq(a: u32, b: u32) -> bool {
    a.wrapping_sub(b) < 0x8000_0000
}

impl Tcp {
    pub const fn new() -> Tcp {
        Tcp {
            state: State::Closed,
            lport: 0,
            rip: [0; 4],
            rport: 0,
            snd_una: 0,
            snd_nxt: 0,
            rcv_nxt: 0,
            rx: [0; RX],
            head: 0,
            len: 0,
            fin: false,
            next_port: 49152,
        }
    }

    fn segment(&self, s: &mut Stack, flags: u8, seq: u32, data: &[u8]) {
        let mut p = [0u8; 1500];
        let syn = flags & SYN != 0;
        let hdr = if syn { 24 } else { 20 };
        let total = hdr + data.len();
        p[0..2].copy_from_slice(&self.lport.to_be_bytes());
        p[2..4].copy_from_slice(&self.rport.to_be_bytes());
        p[4..8].copy_from_slice(&seq.to_be_bytes());
        p[8..12].copy_from_slice(&self.rcv_nxt.to_be_bytes());
        p[12] = (hdr as u8 / 4) << 4;
        p[13] = flags;
        let win = (RX - self.len).min(0xFFFF) as u16;
        p[14..16].copy_from_slice(&win.to_be_bytes());
        if syn {
            p[20..24].copy_from_slice(&[2, 4, (MSS >> 8) as u8, MSS as u8]);
        }
        p[hdr..total].copy_from_slice(data);
        let acc = sum(&s.ip, 0);
        let acc = sum(&self.rip, acc) + 6 + total as u32;
        let c = fold(sum(&p[..total], acc));
        p[16..18].copy_from_slice(&c.to_be_bytes());
        s.send_ip(6, self.rip, &p[..total]);
    }

    fn ack(&self, s: &mut Stack) {
        self.segment(s, ACK, self.snd_nxt, &[]);
    }

    /// Take the effect of one incoming segment; true if it wants an ACK.
    fn on_segment(&mut self, flags: u8, seq: u32, ack: u32, data: &[u8]) -> bool {
        if flags & RST != 0 {
            self.state = State::Reset;
            return false;
        }
        match self.state {
            State::SynSent => {
                if flags & (SYN | ACK) == SYN | ACK && ack == self.snd_nxt {
                    self.rcv_nxt = seq.wrapping_add(1);
                    self.snd_una = ack;
                    self.state = State::Established;
                    return true;
                }
                false
            }
            State::Established => {
                if flags & ACK != 0 && geq(ack, self.snd_una) && geq(self.snd_nxt, ack) {
                    self.snd_una = ack;
                }
                let mut need = false;
                if !data.is_empty() {
                    need = true;
                    if seq == self.rcv_nxt {
                        let n = data.len().min(RX - self.len);
                        for &b in &data[..n] {
                            self.rx[(self.head + self.len) % RX] = b;
                            self.len += 1;
                        }
                        self.rcv_nxt = self.rcv_nxt.wrapping_add(n as u32);
                    }
                }
                if flags & FIN != 0
                    && seq.wrapping_add(data.len() as u32) == self.rcv_nxt
                    && !self.fin
                {
                    self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
                    self.fin = true;
                    need = true;
                }
                need
            }
            _ => false,
        }
    }

    /// Process whatever the stack has received, answering what needs an ACK.
    pub fn pump(&mut self, s: &mut Stack) {
        if let Event::Tcp {
            src,
            sport,
            dport,
            flags,
            seq,
            ack,
            off,
            len,
        } = s.poll()
        {
            if self.state != State::Closed
                && src == self.rip
                && sport == self.rport
                && dport == self.lport
            {
                let need = self.on_segment(flags, seq, ack, &s.frame[off..off + len]);
                if need {
                    self.ack(s);
                }
            }
        }
    }

    fn wait(&mut self, s: &mut Stack, ms: u64, mut done: impl FnMut(&Tcp) -> bool) -> bool {
        let start = uptime_ns();
        while uptime_ns() - start < ms * 1_000_000 {
            self.pump(s);
            if done(self) {
                return true;
            }
            sleep_ms(1);
        }
        done(self)
    }

    pub fn connect(&mut self, s: &mut Stack, ip: Ip, port: u16) -> Result<(), Fail> {
        self.lport = self.next_port;
        self.next_port = if self.next_port == 65000 {
            49152
        } else {
            self.next_port + 1
        };
        (self.rip, self.rport) = (ip, port);
        let iss = (uptime_ns() / 1000) as u32;
        (self.snd_una, self.snd_nxt) = (iss, iss.wrapping_add(1));
        (self.head, self.len, self.fin, self.rcv_nxt) = (0, 0, false, 0);
        self.state = State::SynSent;
        for _ in 0..5 {
            self.segment(s, SYN, iss, &[]);
            if self.wait(s, 1000, |t| t.state != State::SynSent) {
                break;
            }
        }
        match self.state {
            State::Established => Ok(()),
            State::Reset => {
                self.state = State::Closed;
                Err(Fail::Refused)
            }
            _ => {
                self.state = State::Closed;
                Err(Fail::Timeout)
            }
        }
    }

    /// Send all of `data`, one acknowledged segment at a time.
    pub fn send(&mut self, s: &mut Stack, data: &[u8]) -> Result<usize, Fail> {
        for chunk in data.chunks(MSS) {
            if self.state != State::Established {
                return Err(Fail::Closed);
            }
            let seq = self.snd_nxt;
            let end = seq.wrapping_add(chunk.len() as u32);
            self.snd_nxt = end;
            let mut acked = false;
            for _ in 0..5 {
                self.segment(s, PSH | ACK, seq, chunk);
                if self.wait(s, 1000, |t| {
                    t.state != State::Established || geq(t.snd_una, end)
                }) {
                    acked = geq(self.snd_una, end);
                    break;
                }
            }
            if !acked {
                return Err(if self.state == State::Reset {
                    Fail::Closed
                } else {
                    Fail::Timeout
                });
            }
        }
        Ok(data.len())
    }

    /// Wait for data and copy up to `out.len()` bytes; `Ok(0)` is end of stream.
    pub fn recv(&mut self, s: &mut Stack, out: &mut [u8]) -> Result<usize, Fail> {
        if !self.wait(s, 10_000, |t| {
            t.len > 0 || t.fin || t.state != State::Established
        }) {
            return Err(Fail::Timeout);
        }
        if self.len == 0 {
            return if self.fin { Ok(0) } else { Err(Fail::Closed) };
        }
        let n = out.len().min(self.len);
        for b in out.iter_mut().take(n) {
            *b = self.rx[self.head];
            self.head = (self.head + 1) % RX;
            self.len -= 1;
        }
        // The window re-opened; tell the peer.
        self.ack(s);
        Ok(n)
    }

    pub fn close(&mut self, s: &mut Stack) {
        if self.state == State::Established {
            let seq = self.snd_nxt;
            self.snd_nxt = seq.wrapping_add(1);
            self.segment(s, FIN | ACK, seq, &[]);
            let end = self.snd_nxt;
            self.wait(s, 1000, |t| {
                geq(t.snd_una, end) || t.state != State::Established
            });
            if !self.fin {
                self.wait(s, 500, |t| t.fin || t.state != State::Established);
            }
        }
        self.state = State::Closed;
    }
}
