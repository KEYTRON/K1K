//! pong — IPC server. slot 0: request endpoint (RECV), slot 1: reply endpoint (SEND).
#![no_std]
#![no_main]

use k1k_rt::{Cap, exit, log, recv, send};

const REQ: Cap = Cap(0);
const REP: Cap = Cap(1);
const PONG: u64 = 0x504F_4E47;

fn main() -> ! {
    log!("pong server listening");
    loop {
        let m = match recv(REQ) {
            Ok(m) => m,
            Err(e) => {
                log!("recv failed: {:?}", e);
                exit(2);
            }
        };
        log!("request #{} from task {}", m.words[0], m.sender);
        if let Err(e) = send(REP, m.words[0] + 1, m.sender as u64, PONG) {
            log!("reply failed: {:?}", e);
        }
    }
}

k1k_rt::main!(main);
