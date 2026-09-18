//! ping — IPC client. slot 0: request endpoint (SEND only), slot 1: reply endpoint (RECV).
//!
//! First it proves the rights model (recv on a SEND-only slot must fail),
//! then it waits for pong to grant a shared page over the reply endpoint,
//! reads pong's greeting from it, writes an acknowledgement back, and finally
//! runs the ping/pong request loop.
#![no_std]
#![no_main]

use k1k_rt::{Cap, Error, exit, log, mem_map, recv, send, sleep_ms};

const REQ: Cap = Cap(0);
const REP: Cap = Cap(1);
const PING: u64 = 0x5049_4E47;
const SHM_TAG: u64 = 0x5348_4D21;
const ACK_OFFSET: usize = 256;

fn main() -> ! {
    match recv(REQ) {
        Err(Error::Perm) => log!("recv on send-only cap denied (EPERM) - rights enforced"),
        other => {
            log!("BUG: recv on send-only cap returned {:?}", other);
            exit(3);
        }
    }

    let grant = match recv(REP) {
        Ok(m) if m.words[0] == SHM_TAG && m.cap.is_some() => m,
        other => {
            log!("expected shared-memory grant, got {:?}", other);
            exit(3);
        }
    };
    let shm = grant.cap.unwrap();
    let base = match mem_map(shm, true) {
        Ok(p) => p,
        Err(e) => {
            log!("mem_map failed: {:?}", e);
            exit(3);
        }
    };
    let text = unsafe { core::slice::from_raw_parts(base, 64) };
    let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
    log!(
        "received memory cap {:?} from task {}, mapped at {:p}: \"{}\"",
        shm,
        grant.sender,
        base,
        core::str::from_utf8(&text[..end]).unwrap_or("<bad utf8>")
    );
    let ack = b"ack from ping, same page\0";
    unsafe { core::ptr::copy_nonoverlapping(ack.as_ptr(), base.add(ACK_OFFSET), ack.len()) };

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
