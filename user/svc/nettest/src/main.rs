//! nettest — the first client of `net`: fetches a file over HTTP from the host
//! (QEMU's gateway address, port 18791) through the TCP requests of `netproto`.
#![no_std]
#![no_main]

use k1k_rt::{
    Cap, ep_create, exit, log, mem_create, mem_map, netproto as p, recv, rights, send, send_cap,
};

const NET: Cap = Cap(0);
const HOST: u32 = u32::from_be_bytes([10, 0, 2, 2]);
const PORT: u64 = 18791;
const REQUEST: &[u8] = b"GET /k1k-net.txt HTTP/1.0\r\nHost: 10.0.2.2\r\nConnection: close\r\n\r\n";

fn fail(what: &str) -> ! {
    log!("{} failed", what);
    exit(1);
}

fn main() -> ! {
    let mem = mem_create(2).unwrap_or_else(|_| fail("buffer"));
    let buf = mem_map(mem, true).unwrap_or_else(|_| fail("map"));
    let reply = ep_create().unwrap_or_else(|_| fail("reply endpoint"));
    send_cap(
        NET,
        mem,
        rights::MAP_READ | rights::MAP_WRITE,
        p::REGISTER_BUF | 2 << 32,
    )
    .unwrap_or_else(|_| fail("register buffer"));
    send_cap(NET, reply, rights::SEND, p::SET_REPLY).unwrap_or_else(|_| fail("set reply"));

    let ask = |a: u64, b: u64, c: u64| -> (u64, u64) {
        if send(NET, a, b, c).is_err() {
            fail("send request");
        }
        match recv(reply) {
            Ok(m) => (m.words[0], m.words[1]),
            Err(_) => fail("reply"),
        }
    };

    let (status, _) = ask(p::CONNECT, HOST as u64, PORT);
    if status != p::STATUS_OK {
        log!("connect: status {}", status);
        exit(1);
    }
    unsafe { core::ptr::copy_nonoverlapping(REQUEST.as_ptr(), buf, REQUEST.len()) };
    let (status, sent) = ask(p::SEND, 0, REQUEST.len() as u64);
    if status != p::STATUS_OK || sent as usize != REQUEST.len() {
        log!("send: status {}, {} bytes", status, sent);
        exit(1);
    }

    // Read the response until the server closes the connection.
    let mut total = 0usize;
    loop {
        let (status, n) = ask(p::RECV, total as u64, (8192 - total) as u64);
        if status != p::STATUS_OK {
            log!("recv: status {}", status);
            break;
        }
        if n == 0 {
            break;
        }
        total += n as usize;
        if total >= 8192 {
            break;
        }
    }
    ask(p::CLOSE, 0, 0);

    let resp = unsafe { core::slice::from_raw_parts(buf, total) };
    let text = core::str::from_utf8(resp).unwrap_or("");
    let status_line = text.lines().next().unwrap_or("");
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
    log!(
        "http: {} bytes, \"{}\", body \"{}\"",
        total,
        status_line,
        body.trim()
    );
    if status_line.contains("200") && body.contains("K1K-NET-OK") {
        log!("http ok");
        exit(0);
    }
    exit(1);
}

k1k_rt::main!(main);
