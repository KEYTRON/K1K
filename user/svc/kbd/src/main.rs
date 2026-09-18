//! kbd — the PS/2 keyboard driver, running in ring 3.
//!
//! The kernel only forwards raw scancodes from the IRQ into an endpoint; this
//! service holds the RECV capability (slot 0), decodes scancode set 1 (US
//! layout) and echoes completed lines. If it crashes, the supervisor restarts
//! it and keys keep working — the kernel never contained the decoder.
#![no_std]
#![no_main]

use k1k_rt::{Cap, exit, log, recv};

const KEYS: Cap = Cap(0);

const UNSHIFTED: [u8; 58] =
    *b"\0\x1b1234567890-=\x08\tqwertyuiop[]\n\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ";
const SHIFTED: [u8; 58] =
    *b"\0\x1b!@#$%^&*()_+\x08\tQWERTYUIOP{}\n\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0*\0 ";

const LSHIFT: u8 = 0x2A;
const RSHIFT: u8 = 0x36;
const CAPS: u8 = 0x3A;

fn main() -> ! {
    log!("keyboard driver online (ring 3), type and press Enter");
    let mut shift = false;
    let mut caps = false;
    let mut line = [0u8; 120];
    let mut len = 0usize;

    loop {
        let m = match recv(KEYS) {
            Ok(m) => m,
            Err(e) => {
                log!("recv failed: {:?}", e);
                exit(2);
            }
        };
        let sc = m.words[0] as u8;
        let released = sc & 0x80 != 0;
        let code = sc & 0x7F;

        match code {
            LSHIFT | RSHIFT => shift = !released,
            CAPS if !released => caps = !caps,
            _ if released => {}
            _ => {
                let Some(&ch) = (if shift { &SHIFTED } else { &UNSHIFTED }).get(code as usize)
                else {
                    continue;
                };
                match ch {
                    0 => {}
                    b'\n' => {
                        let s = core::str::from_utf8(&line[..len]).unwrap_or("<bad utf8>");
                        log!("typed: {}", s);
                        len = 0;
                    }
                    0x08 => len = len.saturating_sub(1),
                    _ => {
                        let ch = if caps && ch.is_ascii_alphabetic() {
                            ch ^ 0x20
                        } else {
                            ch
                        };
                        if len < line.len() {
                            line[len] = ch;
                            len += 1;
                        }
                    }
                }
            }
        }
    }
}

k1k_rt::main!(main);
