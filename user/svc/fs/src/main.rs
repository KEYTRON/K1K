//! fs — file system server and init. Talks to `blk` over IPC with a shared
//! DMA buffer, mounts the FAT volume on the disk, and spawns every ELF it
//! finds under /SVC as a supervised service. This is how user space gets on
//! the machine without being baked into the kernel image.
#![no_std]
#![no_main]

mod fat;

use fat::{BlockDevice, Fat};
use k1k_rt::{
    Cap, blkproto as proto, ep_create, exit, log, mem_create, mem_create_dma, mem_map, recv,
    rights, send, send_cap, spawn,
};

const BLK: Cap = Cap(0);
const CONTROL: Cap = Cap(1);

const BUF_PAGES: usize = 16;

struct Blk {
    reply: Cap,
    buf: *mut u8,
}

impl BlockDevice for Blk {
    fn read(&mut self, lba: u64, count: usize, out: &mut [u8]) -> Result<(), ()> {
        let mut done = 0usize;
        while done < count {
            let n = (count - done).min(proto::MAX_BLOCKS as usize);
            send(BLK, proto::READ, lba + done as u64, n as u64).map_err(|_| ())?;
            let m = recv(self.reply).map_err(|_| ())?;
            if m.words[0] != proto::STATUS_OK {
                return Err(());
            }
            let bytes = n * 512;
            unsafe {
                core::ptr::copy_nonoverlapping(self.buf, out[done * 512..].as_mut_ptr(), bytes);
            }
            done += n;
        }
        Ok(())
    }
}

fn fail(msg: &str, code: i64) -> ! {
    log!("{}", msg);
    exit(code)
}

fn main() -> ! {
    // Shared DMA buffer for block transfers + our reply endpoint, both handed to blk.
    let buf_cap = mem_create_dma(BUF_PAGES).unwrap_or_else(|e| fail_e("dma buffer", e));
    let buf = mem_map(buf_cap, true).unwrap_or_else(|e| fail_e("map buffer", e));
    let reply = ep_create().unwrap_or_else(|e| fail_e("reply endpoint", e));
    send_cap(
        BLK,
        buf_cap,
        rights::MAP_READ | rights::MAP_WRITE | rights::DMA,
        proto::REGISTER_BUF | (BUF_PAGES as u64) << 32,
    )
    .unwrap_or_else(|e| fail_e("register buffer", e));
    send_cap(BLK, reply, rights::SEND, proto::SET_REPLY).unwrap_or_else(|e| fail_e("set reply", e));

    let mut dev = Blk { reply, buf };
    let fs = match Fat::mount(&mut dev) {
        Ok(f) => f,
        Err(e) => fail(e, 2),
    };
    let mut fs = fs;
    let mut label = [0u8; 11];
    label.copy_from_slice(&fs.label);
    log!(
        "mounted {:?} volume \"{}\": {} clusters x {} B",
        fs.kind,
        core::str::from_utf8(&label).unwrap_or("?").trim_end(),
        fs.total_clusters,
        fs.cluster_bytes()
    );

    // Scratch memory for directory and cluster reads (a plain shared page set).
    let scratch_cap = mem_create(16).unwrap_or_else(|e| fail_e("scratch", e));
    let scratch_ptr = mem_map(scratch_cap, true).unwrap_or_else(|e| fail_e("map scratch", e));
    let scratch = unsafe { core::slice::from_raw_parts_mut(scratch_ptr, 16 * 4096) };

    // README.TXT in the root, if present.
    if let Ok(Some(readme)) = fs.lookup(&mut dev, 0, "README.TXT", scratch) {
        let mut text = [0u8; 128];
        let n = readme.size.min(127) as usize;
        let mut file = [0u8; 128];
        let (a, b) = scratch.split_at_mut(8 * 4096);
        if fs
            .read_file(&mut dev, &readme, a, &mut file[..n.max(1)])
            .is_ok()
        {
            text[..n].copy_from_slice(&file[..n]);
            log!(
                "/README.TXT: \"{}\"",
                core::str::from_utf8(&text[..n]).unwrap_or("?").trim_end()
            );
        }
        let _ = b;
    }

    // /SVC: spawn every ELF as a service.
    let svc_dir = match fs.lookup(&mut dev, 0, "SVC", scratch) {
        Ok(Some(d)) if d.is_dir() => d,
        _ => fail("no /SVC directory on the disk", 2),
    };
    let mut entries = [None; 16];
    let mut count = 0;
    fs.read_dir(&mut dev, svc_dir.cluster, scratch, |e| {
        if !e.is_dir() && count < entries.len() && e.size > 0 {
            entries[count] = Some(*e);
            count += 1;
        }
    })
    .unwrap_or_else(|_| fail("cannot read /SVC", 2));

    let mut spawned = 0;
    for entry in entries.iter().flatten() {
        let mut namebuf = [0u8; 13];
        let display = entry.display(&mut namebuf);
        let base_len = display.find('.').unwrap_or(display.len());
        let mut lower = [0u8; 13];
        for (i, b) in display.as_bytes()[..base_len].iter().enumerate() {
            lower[i] = b.to_ascii_lowercase();
        }
        let name = core::str::from_utf8(&lower[..base_len]).unwrap_or("svc");

        let pages = (entry.size as usize).div_ceil(4096);
        let img_cap = match mem_create(pages) {
            Ok(c) => c,
            Err(e) => {
                log!("{}: cannot allocate {} pages: {:?}", display, pages, e);
                continue;
            }
        };
        let img = match mem_map(img_cap, true) {
            Ok(p) => p,
            Err(e) => {
                log!("{}: cannot map image: {:?}", display, e);
                continue;
            }
        };
        let out = unsafe { core::slice::from_raw_parts_mut(img, entry.size as usize) };
        let read = match fs.read_file(&mut dev, entry, scratch, out) {
            Ok(n) => n,
            Err(()) => {
                log!("{}: read failed", display);
                continue;
            }
        };
        match spawn(CONTROL, img_cap, read, name) {
            Ok(task) => {
                log!("spawned {} ({} bytes) as task {}", display, read, task);
                spawned += 1;
            }
            Err(e) => log!("{}: spawn failed: {:?}", display, e),
        }
    }
    log!("{} service(s) started from /SVC", spawned);

    loop {
        k1k_rt::sleep_ms(10_000);
    }
}

fn fail_e(what: &str, e: k1k_rt::Error) -> ! {
    log!("{} failed: {:?}", what, e);
    exit(2)
}

k1k_rt::main!(main);
