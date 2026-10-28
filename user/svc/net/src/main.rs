//! net — the network driver, running in ring 3. The kernel hands it a virtio
//! network device (slot 0). For now it speaks just enough Ethernet, ARP, IPv4
//! and ICMP to find the gateway and ping it; sockets come on top of this.
#![no_std]
#![no_main]

mod dhcp;
mod dns;
mod stack;
mod virtio;

use k1k_rt::{Cap, dev_info, exit, log, sleep_ms, uptime_ns};
use stack::{Event, Stack};
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

    // From here on the service just answers ARP and ping.
    log!("up; answering ARP and ping");
    loop {
        net.poll();
        sleep_ms(1);
    }
}

k1k_rt::main!(main);
