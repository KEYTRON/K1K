//! net — the network driver, running in ring 3. The kernel hands it a virtio
//! network device (slot 0). For now it speaks just enough Ethernet, ARP, IPv4
//! and ICMP to find the gateway and ping it; sockets come on top of this.
#![no_std]
#![no_main]

mod dhcp;
mod dns;
mod stack;
mod tcp;
mod virtio;

use k1k_rt::{Cap, dev_info, exit, log, mem_map, netproto, send, sleep_ms, try_recv, uptime_ns};
use stack::{Event, Stack};
use tcp::{Fail, Tcp};
use virtio::Nic;

const DEVICE: Cap = Cap(0);

fn main() -> ! {
    let info = match dev_info(DEVICE) {
        Ok(i) => i,
        Err(e) => {
            log!("no network device capability ({:?}); nothing to drive", e);
            exit(0);
        }
    };
    log!(
        "pci {:02x}:{:02x}.{} {:04x}:{:04x}",
        info.bus,
        info.slot,
        info.func,
        info.vendor,
        info.device
    );
    let mut nic = match Nic::init(DEVICE) {
        Ok(n) => n,
        Err(e) => {
            log!("virtio-net init failed: {:?}", e);
            exit(2);
        }
    };
    let m = nic.mac;
    log!(
        "virtio-net ready, mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[0],
        m[1],
        m[2],
        m[3],
        m[4],
        m[5]
    );
    let Some(lease) = dhcp::acquire(&mut nic) else {
        log!("dhcp: no lease");
        exit(1);
    };
    let (our_ip, gateway) = (lease.ip, lease.router);
    log!(
        "dhcp: leased {}.{}.{}.{}, router {}.{}.{}.{}, dns {}.{}.{}.{}",
        our_ip[0],
        our_ip[1],
        our_ip[2],
        our_ip[3],
        gateway[0],
        gateway[1],
        gateway[2],
        gateway[3],
        lease.dns[0],
        lease.dns[1],
        lease.dns[2],
        lease.dns[3]
    );

    let mut net = Stack::new(nic, our_ip, gateway);
    if !net.resolve_gateway() {
        log!(
            "gateway {}.{}.{}.{} did not answer ARP",
            gateway[0],
            gateway[1],
            gateway[2],
            gateway[3]
        );
        exit(1);
    }
    let g = net.gw_mac;
    log!(
        "gateway is at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        g[0],
        g[1],
        g[2],
        g[3],
        g[4],
        g[5]
    );

    net.send_echo(gateway, 1);
    let sent = uptime_ns();
    let mut pinged = false;
    while !pinged && uptime_ns() - sent < 3_000_000_000 {
        if let Event::EchoReply { len, .. } = net.poll() {
            log!(
                "ping {}.{}.{}.{}: reply, {} bytes",
                gateway[0],
                gateway[1],
                gateway[2],
                gateway[3],
                len
            );
            pinged = true;
        }
        sleep_ms(1);
    }
    if !pinged {
        log!("ping: no reply");
        exit(1);
    }

    // Names need a resolver upstream, which a CI box may not have: not fatal.
    match dns::resolve(&mut net, lease.dns, "keytron-prime.org") {
        Some(a) => log!(
            "dns: keytron-prime.org is {}.{}.{}.{}",
            a[0],
            a[1],
            a[2],
            a[3]
        ),
        None => log!("dns: no answer for keytron-prime.org"),
    }

    serve(&mut net, lease.dns)
}

/// Slot 0 is the device, slot 1 the request endpoint.
const REQUESTS: Cap = Cap(1);

/// Reply `(status, value)` to the client, if it has told us where.
fn reply(to: Option<Cap>, status: u64, value: u64) {
    if let Some(ep) = to {
        let _ = send(ep, status, value, 0);
    }
}

/// Answer ARP and ping, and serve the one client's TCP requests. Requests are
/// handled start to finish (connect, send and recv wait on the network inside
/// the handler), so there is a single connection and a single client for now.
fn serve(net: &mut Stack, dns_server: stack::Ip) -> ! {
    use netproto as p;
    let mut tcp = Tcp::new();
    let mut buf: Option<(*mut u8, usize)> = None;
    let mut reply_to: Option<Cap> = None;
    log!("up; answering ARP and ping, serving TCP requests");
    loop {
        tcp.pump(net);
        let Ok(m) = try_recv(REQUESTS) else {
            sleep_ms(1);
            continue;
        };
        let (op, arg) = (m.words[0] & 0xFFFF_FFFF, m.words[0] >> 32);
        match op {
            p::REGISTER_BUF => match m.cap.map(|c| mem_map(c, true)) {
                Some(Ok(ptr)) => buf = Some((ptr, arg as usize * 4096)),
                other => log!("cannot map the client buffer: {:?}", other),
            },
            p::SET_REPLY => reply_to = m.cap,
            p::CONNECT => {
                let ip = (m.words[1] as u32).to_be_bytes();
                let status = match tcp.connect(net, ip, m.words[2] as u16) {
                    Ok(()) => p::STATUS_OK,
                    Err(Fail::Refused) => p::STATUS_REFUSED,
                    Err(Fail::Timeout) => p::STATUS_TIMEOUT,
                    Err(Fail::Closed) => p::STATUS_ERR,
                };
                reply(reply_to, status, 0);
            }
            p::SEND | p::RECV => {
                let (off, len) = (m.words[1] as usize, m.words[2] as usize);
                let Some((ptr, size)) = buf.filter(|(_, size)| off.saturating_add(len) <= *size)
                else {
                    reply(reply_to, p::STATUS_ERR, 0);
                    continue;
                };
                let _ = size;
                let area = unsafe { core::slice::from_raw_parts_mut(ptr.add(off), len) };
                let result = if op == p::SEND {
                    tcp.send(net, area)
                } else {
                    tcp.recv(net, area)
                };
                match result {
                    Ok(n) => reply(reply_to, p::STATUS_OK, n as u64),
                    Err(Fail::Timeout) => reply(reply_to, p::STATUS_TIMEOUT, 0),
                    Err(_) => reply(reply_to, p::STATUS_ERR, 0),
                }
            }
            p::RESOLVE => {
                let (off, len) = (m.words[1] as usize, m.words[2] as usize);
                let name = buf
                    .filter(|(_, size)| off.saturating_add(len) <= *size && len <= 253)
                    .and_then(|(ptr, _)| {
                        let bytes = unsafe { core::slice::from_raw_parts(ptr.add(off), len) };
                        core::str::from_utf8(bytes).ok()
                    });
                match name.and_then(|n| dns::resolve(net, dns_server, n)) {
                    Some(ip) => reply(reply_to, p::STATUS_OK, u32::from_be_bytes(ip) as u64),
                    None => reply(reply_to, p::STATUS_ERR, 0),
                }
            }
            p::CLOSE => {
                tcp.close(net);
                reply(reply_to, p::STATUS_OK, 0);
            }
            other => log!("unknown request {} from task {}", other, m.sender),
        }
    }
}

k1k_rt::main!(main);
