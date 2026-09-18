//! ping — IPC client. slot 0: request endpoint (SEND only), slot 1: reply endpoint (RECV).
#![no_std]
#![no_main]

use k1k_rt::{Cap, Error, exit, log, recv, send, sleep_ms};

const REQ: Cap = Cap(0);
const REP: Cap = Cap(1);
const PING: u64 = 0x5049_4E47;

fn main() -> ! {
    // Capability check: slot 0 is SEND-only, so receiving on it must be refused.
    match recv(REQ) {
        Err(Error::Perm) => log!("recv on send-only cap denied (EPERM) - rights enforced"),
        other => {
            log!("BUG: recv on send-only cap returned {:?}", other);
            exit(3);
        }
    }

    let mut n = 0u64;
    loop {
        n += 1;
        if let Err(e) = send(REQ, n, 0, PING) {
            log!("send failed: {:?}", e);
            exit(4);
        }
        match recv(REP) {
            Ok(m) => log!("got reply {} (tag {:#x})", m.words[0], m.words[2]),
            Err(e) => {
                log!("recv failed: {:?}", e);
                exit(4);
            }
        }
        sleep_ms(600);
    }
}

k1k_rt::main!(main);
