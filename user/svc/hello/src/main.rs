//! hello — the simplest ring-3 service: greets periodically and reports uptime.
#![no_std]
#![no_main]

use k1k_rt::{info, log, sleep_ms};

fn main() -> ! {
    let me = info();
    log!("hello service up (ring 3, task {}, Rust ELF)", me.task_id);
    let mut beat = 0u64;
    loop {
        beat += 1;
        log!("alive #{}, uptime ms = {}", beat, info().uptime_ms);
        sleep_ms(900);
    }
}

k1k_rt::main!(main);
