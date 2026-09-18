//! pong — IPC server. slot 0: request endpoint (RECV), slot 1: reply endpoint (SEND).
//!
//! On start it allocates a shared page, writes a greeting into it and hands
//! the memory capability to ping over the reply endpoint. Every request is
//! answered with a message; the first one is also checked for ping's
//! acknowledgement written back through the shared page.
#![no_std]
#![no_main]

use k1k_rt::{Cap, exit, log, mem_create, mem_map, recv, rights, send, send_cap};

const REQ: Cap = Cap(0);
const REP: Cap = Cap(1);
const PONG: u64 = 0x504F_4E47;
const SHM_TAG: u64 = 0x5348_4D21;
const ACK_OFFSET: usize = 256;

fn main() -> ! {
    let shm = match mem_create(1) {
        Ok(c) => c,
        Err(e) => {
            log!("mem_create failed: {:?}", e);
            exit(2);
        }
    };
    let base = match mem_map(shm, true) {
        Ok(p) => p,
        Err(e) => {
            log!("mem_map failed: {:?}", e);
            exit(2);
        }
    };
    let greeting = b"hello via shared memory from pong\0";
    unsafe { core::ptr::copy_nonoverlapping(greeting.as_ptr(), base, greeting.len()) };
    if let Err(e) = send_cap(REP, shm, rights::MAP_READ | rights::MAP_WRITE, SHM_TAG) {
        log!("send_cap failed: {:?}", e);
        exit(2);
    }
    log!("pong server listening (shared page granted to ping)");

    let mut first = true;
    loop {
        let m = match recv(REQ) {
            Ok(m) => m,
            Err(e) => {
                log!("recv failed: {:?}", e);
                exit(2);
            }
        };
        if first {
            first = false;
            let ack = unsafe { core::slice::from_raw_parts(base.add(ACK_OFFSET), 64) };
            let end = ack.iter().position(|&b| b == 0).unwrap_or(ack.len());
            let ack = core::str::from_utf8(&ack[..end]).unwrap_or("<bad utf8>");
            log!("ping wrote back through shared memory: \"{}\"", ack);
        }
        log!("request #{} from task {}", m.words[0], m.sender);
        if let Err(e) = send(REP, m.words[0] + 1, m.sender as u64, PONG) {
            log!("reply failed: {:?}", e);
        }
    }
}

k1k_rt::main!(main);
