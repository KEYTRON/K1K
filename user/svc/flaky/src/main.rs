//! flaky — a deliberately buggy service. It works for a few iterations, then
//! dereferences a null pointer. The kernel kills it and the supervisor
//! restarts it: the system keeps running without a reboot.
#![no_std]
#![no_main]

use k1k_rt::{log, sleep_ms};

fn main() -> ! {
    log!("flaky service started");
    let mut iteration = 0u32;
    loop {
        iteration += 1;
        log!("working, iteration {}", iteration);
        sleep_ms(400);
        if iteration == 3 {
            log!("about to dereference NULL...");
            let p: *mut u64 = core::ptr::null_mut();
            unsafe { core::ptr::write_volatile(p, 0xDEAD) };
        }
    }
}

k1k_rt::main!(main);
