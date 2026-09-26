//! hello — the smallest service that does real work.
//!
//! It shows the three things a service needs to live on K1K: the capability
//! table it was started with, the file service protocol, and a heap. The
//! manifest gives it `fs`, so it finds the file server with `granted("fs")`
//! instead of assuming a slot number.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use k1k_rt::file::FileClient;
use k1k_rt::fsproto::DirRec;
use k1k_rt::{granted, heap_stats, info, log, sleep_ms};

/// Listing entries shown at start-up.
const MAX_ENTRIES: usize = 16;

fn main() -> ! {
    let me = info();
    log!("hello service up (ring 3, task {}, Rust ELF)", me.task_id);
    read_files();
    let mut beat = 0u64;
    loop {
        beat += 1;
        log!(
            "alive #{}, uptime {} ms, heap {} B in {} block(s)",
            beat,
            info().uptime_ms,
            heap_stats().in_use,
            heap_stats().live
        );
        sleep_ms(900);
    }
}

/// List the service directory and read the volume README over the file
/// protocol. Every failure is reported and survived: a service that cannot
/// reach the file server still runs.
fn read_files() {
    let Some(ep) = granted("fs") else {
        log!("no 'fs' capability in my manifest entry; skipping files");
        return;
    };
    let mut fs = match FileClient::new(ep, 16) {
        Ok(c) => c,
        Err(e) => {
            log!("cannot connect to the file service: {:?}", e);
            return;
        }
    };

    let mut entries = [DirRec {
        name: [0; 11],
        attr: 0,
        size: 0,
    }; MAX_ENTRIES];
    match fs.list("/SVC", &mut entries) {
        Ok(n) => {
            let mut names = String::new();
            for e in entries.iter().take(n) {
                let mut buf = [0u8; 13];
                let name = e.display(&mut buf);
                if !names.is_empty() {
                    names.push_str(" ");
                }
                names.push_str(name);
            }
            log!(
                "/SVC holds {} entr{}: {}",
                n,
                if n == 1 { "y" } else { "ies" },
                names
            );
        }
        Err(e) => log!("cannot list /SVC: {:?}", e),
    }

    match fs.stat("/README.TXT") {
        Ok(st) => log!("/README.TXT is {} byte(s)", st.size),
        Err(e) => log!("cannot stat /README.TXT: {:?}", e),
    }

    let file = match fs.open("/README.TXT") {
        Ok(f) => f,
        Err(e) => {
            log!("cannot open /README.TXT: {:?}", e);
            return;
        }
    };
    let mut text = Vec::new();
    match fs.read_all(file, &mut text) {
        Ok(n) => log!(
            "/README.TXT: \"{}\" ({} B read, first line: {:?})",
            String::from_utf8_lossy(&text[..n.min(64)]).trim_end(),
            n,
            first_line(&text),
        ),
        Err(e) => log!("read failed: {:?}", e),
    }
    let _ = fs.close(file);
    let _ = fs.stat("/SVC/NOSUCH.ELF");
    fs.disconnect();
    log!("{} used on the heap so far", heap_stats().in_use);
}

fn first_line(text: &[u8]) -> String {
    let end = text.iter().position(|b| *b == b'\n').unwrap_or(text.len());
    format!("{}", core::str::from_utf8(&text[..end]).unwrap_or("?"))
}

k1k_rt::main!(main);
