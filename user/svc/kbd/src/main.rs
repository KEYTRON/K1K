//! kbd — the PS/2 keyboard driver, running in ring 3.
//!
//! The kernel knows nothing about keyboards: it hands this service an
//! interrupt object for ISA IRQ 1 (slot 0) and the 8042 port range (slot 1).
//! Each interrupt signals a notification we create, we take every signal there
//! is and drain the controller's output buffer through the port capability,
//! decode scancode set 1 (US layout) and echo completed lines. If we crash, the
//! supervisor restarts us and keys keep working.
//!
//! A notification rather than an endpoint on purpose: the controller can buffer
//! scancodes faster than we drain them, and with a message queue a full queue
//! loses keystrokes without telling anybody. Here a keystroke that arrives while
//! we are busy is simply still counted when we come back for it.
#![no_std]
#![no_main]

use k1k_rt::{Cap, WaitMode, exit, irq_ack, irq_bind, log, notify_create, notify_wait, port_in};

const IRQ: Cap = Cap(0);
const PORTS: Cap = Cap(1);
const DATA: u16 = 0; // 0x60
const STATUS: u16 = 4; // 0x64

const UNSHIFTED: [u8; 58] =
    *b"\0\x1b1234567890-=\x08\tqwertyuiop[]\n\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ";
const SHIFTED: [u8; 58] =
    *b"\0\x1b!@#$%^&*()_+\x08\tQWERTYUIOP{}\n\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0*\0 ";

const LSHIFT: u8 = 0x2A;
const RSHIFT: u8 = 0x36;
const CAPS: u8 = 0x3A;

struct Decoder {
    shift: bool,
    caps: bool,
    line: [u8; 120],
    len: usize,
}

impl Decoder {
    fn feed(&mut self, sc: u8) {
        let released = sc & 0x80 != 0;
        let code = sc & 0x7F;
        match code {
            LSHIFT | RSHIFT => self.shift = !released,
            CAPS if !released => self.caps = !self.caps,
            _ if released => {}
            _ => {
                let table = if self.shift { &SHIFTED } else { &UNSHIFTED };
                let Some(&ch) = table.get(code as usize) else {
                    return;
                };
                match ch {
                    0 => {}
                    b'\n' => {
                        let s =
                            core::str::from_utf8(&self.line[..self.len]).unwrap_or("<bad utf8>");
                        log!("typed: {}", s);
                        self.len = 0;
                    }
                    0x08 => self.len = self.len.saturating_sub(1),
                    _ => {
                        let ch = if self.caps && ch.is_ascii_alphabetic() {
                            ch ^ 0x20
                        } else {
                            ch
                        };
                        if self.len < self.line.len() {
                            self.line[self.len] = ch;
                            self.len += 1;
                        }
                    }
                }
            }
        }
    }
}

fn main() -> ! {
    let keys = notify_create().unwrap_or_else(|e| die("notify_create", e));
    irq_bind(IRQ, keys).unwrap_or_else(|e| die("irq_bind", e));
    log!("keyboard driver online (ring 3, IRQ 1 via notification), type and press Enter");

    let mut dec = Decoder {
        shift: false,
        caps: false,
        line: [0; 120],
        len: 0,
    };
    // Drain anything the controller buffered before we were listening. The
    // line is masked until the first ack, so this is really about the state we
    // inherited, not about signals.
    drain(&mut dec);
    let _ = irq_ack(IRQ);
    loop {
        // Take every signal at once: one wake-up covers whatever number of
        // scancodes the controller is holding.
        match notify_wait(keys, WaitMode::All) {
            Ok(_) => {}
            Err(e) => die("notify_wait", e),
        }
        drain(&mut dec);
        let _ = irq_ack(IRQ);
    }
}

/// Read scancodes while the 8042 output buffer is full.
fn drain(dec: &mut Decoder) {
    for _ in 0..32 {
        let status = port_in(PORTS, STATUS, 1).unwrap_or(0);
        if status & 1 == 0 {
            break;
        }
        if let Ok(sc) = port_in(PORTS, DATA, 1) {
            dec.feed(sc as u8);
        }
    }
}

fn die(what: &str, e: k1k_rt::Error) -> ! {
    log!("{} failed: {:?}", what, e);
    exit(2)
}

k1k_rt::main!(main);
