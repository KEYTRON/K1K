//! fs — file system server and init.
//!
//! Talks to `blk` over IPC with a shared DMA buffer, mounts the FAT volume on
//! the disk, starts the services listed in the manifest, and then serves
//! `open`/`read`/`stat`/`list` to any service that was given its endpoint.
//! This is how user space gets on the machine without being baked into the
//! kernel image, and how a service that was itself started from disk reads
//! files.
#![no_std]
#![no_main]

extern crate alloc;

mod fat;
mod manifest;
mod paths;
mod serve;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use fat::{BlockDevice, Fat};
use k1k_rt::{
    Cap, Grant, blkproto as proto, ep_create, exit, log, mem_create, mem_create_dma, mem_map, recv,
    rights, send, send_cap, spawn_with,
};

/// Capability slots `fs` starts with: the block driver's request endpoint, the
/// authority to start services, the half of the file-service channel `fs`
/// receives on, and the half it hands to clients so they can send requests.
pub const BLK: Cap = Cap(0);
pub const CONTROL: Cap = Cap(1);
/// Requests arrive here.
pub const SERVE: Cap = Cap(2);
/// Handed out to services the manifest grants `fs` to; they send on it.
pub const CALL: Cap = Cap(3);

/// Manifest to read, relative to the volume root.
const MANIFEST: &str = "/SVC/MANIFEST.TXT";
/// Services in the manifest may be named at most this long.
const MAX_SERVICES: usize = 24;

const BUF_PAGES: usize = 16;
/// Scratch memory for directory and cluster reads, in pages.
const SCRATCH_PAGES: usize = 16;

pub struct Blk {
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

fn fail_e(what: &str, e: k1k_rt::Error) -> ! {
    log!("{} failed: {:?}", what, e);
    exit(2)
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
    let mut fs = match Fat::mount(&mut dev) {
        Ok(f) => f,
        Err(e) => fail(e, 2),
    };
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
    let scratch_cap = mem_create(SCRATCH_PAGES).unwrap_or_else(|e| fail_e("scratch", e));
    let scratch_ptr = mem_map(scratch_cap, true).unwrap_or_else(|e| fail_e("map scratch", e));
    let mut scratch = unsafe { core::slice::from_raw_parts_mut(scratch_ptr, SCRATCH_PAGES * 4096) };

    show_readme(&mut fs, &mut dev, &mut scratch);
    let started = start_manifest(&mut fs, &mut dev, &mut scratch);
    log!(
        "{} of {} service(s) started from {}",
        started,
        started.max(0),
        MANIFEST
    );

    serve::serve(&mut fs, &mut dev, &mut scratch);
}

/// Read and show the volume's README, if it has one.
fn show_readme(fs: &mut Fat, dev: &mut Blk, scratch: &mut [u8]) {
    let Ok(Some(entry)) = fs.lookup(dev, 0, "README.TXT", scratch) else {
        return;
    };
    if entry.is_dir() || entry.size == 0 || entry.size > 128 {
        return;
    }
    let mut text = [0u8; 128];
    let n = entry.size as usize;
    let (head, _) = scratch.split_at_mut(8 * 4096);
    if fs.read_file(dev, &entry, head, &mut text[..n]).is_err() {
        return;
    }
    let text = core::str::from_utf8(&text[..n]).unwrap_or("<binary>");
    log!("/README.TXT: \"{}\"", text.trim_end());
}

/// Read the manifest and start everything it lists. A service that cannot be
/// started is reported and skipped: init stays up and keeps serving files, so
/// a broken image cannot take the system with it.
fn start_manifest(fs: &mut Fat, dev: &mut Blk, scratch: &mut [u8]) -> usize {
    let text = match read_manifest(fs, dev, scratch) {
        Ok(t) => t,
        Err(why) => {
            log!("no manifest: {} — nothing started", why);
            return 0;
        }
    };
    let parsed = manifest::parse(&text);
    for (line, why) in &parsed.errors {
        log!("manifest line {}: {}", line, why);
    }
    if parsed.entries.is_empty() {
        log!("manifest lists no services");
        return 0;
    }
    log!(
        "manifest: {} service(s), {} rejected line(s)",
        parsed.entries.len(),
        parsed.errors.len()
    );

    let mut started = 0usize;
    for entry in parsed.entries.iter().take(MAX_SERVICES) {
        match start_one(fs, dev, scratch, entry) {
            Ok(()) => started += 1,
            Err(why) => log!("manifest line {}: {}", entry.line, why),
        }
    }
    if parsed.entries.len() > MAX_SERVICES {
        log!(
            "manifest lists more than {} services; the rest were ignored",
            MAX_SERVICES
        );
    }
    started
}

/// Load the manifest into a `String`.
fn read_manifest(fs: &mut Fat, dev: &mut Blk, scratch: &mut [u8]) -> Result<String, &'static str> {
    let (dir, _leaf) = match paths::split_parent(fs, dev, MANIFEST, scratch) {
        Ok(v) => v,
        Err(_) => return Err("no /SVC directory on the disk"),
    };
    let name = paths::leaf_name(MANIFEST);
    let entry = match fs.lookup(dev, dir, name, scratch) {
        Ok(Some(e)) if !e.is_dir() => e,
        Ok(_) => return Err("no MANIFEST.TXT in /SVC"),
        Err(_) => return Err("cannot read the manifest"),
    };
    if entry.size == 0 || entry.size > 16 * 1024 {
        return Err("manifest is empty or too large");
    }
    let mut text = String::new();
    let n = entry.size as usize;
    let (head, _) = scratch.split_at_mut(8 * 4096);
    let mut bytes = alloc::vec![0u8; n];
    let got = fs
        .read_file(dev, &entry, head, &mut bytes[..n])
        .map_err(|_| "cannot read the manifest")?;
    if let Ok(s) = core::str::from_utf8(&bytes[..got]) {
        text.push_str(s);
    }
    Ok(text)
}

/// Start one service from the manifest: read its image, resolve its
/// capabilities and hand both to the supervisor.
fn start_one(
    fs: &mut Fat,
    dev: &mut Blk,
    scratch: &mut [u8],
    entry: &manifest::Entry,
) -> Result<(), String> {
    // A relative image name lives under /SVC.
    let image = if entry.image.starts_with('/') {
        entry.image.clone()
    } else {
        format!("/SVC/{}", entry.image)
    };
    let (dir, _leaf) = paths::split_parent(fs, dev, &image, scratch)
        .map_err(|_| format!("{}: no such directory", image))?;
    let meta = fs
        .lookup(dev, dir, paths::leaf_name(&image), scratch)
        .map_err(|_| format!("{}: cannot read directory", image))?
        .ok_or_else(|| format!("{}: no such file", image))?;
    if meta.is_dir() || meta.size == 0 {
        return Err(format!("{}: not an ELF image", image));
    }

    let mut grants: Vec<Grant> = Vec::new();
    let mut names: Vec<(String, u32)> = Vec::new();
    for cap in &entry.caps {
        let grant = named_grant(cap).ok_or_else(|| format!("unknown capability '{}'", cap))?;
        names.push((cap.clone(), grants.len() as u32));
        grants.push(grant);
    }
    // The service needs to know where its capabilities ended up.
    let args = format!("name={}\n{}", entry.name, manifest::caps_arg(&names));

    let pages = (meta.size as usize).div_ceil(4096);
    let img_cap =
        mem_create(pages).map_err(|e| format!("cannot allocate {} pages: {:?}", pages, e))?;
    let img_ptr = mem_map(img_cap, true).map_err(|e| format!("cannot map image: {:?}", e))?;
    let out = unsafe { core::slice::from_raw_parts_mut(img_ptr, meta.size as usize) };
    let (head, _) = scratch.split_at_mut(8 * 4096);
    let read = fs
        .read_file(dev, &meta, head, out)
        .map_err(|_| "read failed".to_string())?;

    let task = spawn_with(CONTROL, img_cap, read, &entry.name, &grants, &args)
        .map_err(|e| format!("spawn failed: {:?}", e))?;
    log!(
        "started '{}' from {} as task {} ({} bytes, {} capability slot(s))",
        entry.name,
        image,
        task,
        read,
        grants.len()
    );
    Ok(())
}

/// Capability names a manifest may use, and what they mean. `fs` only hands out
/// authority it holds itself.
fn named_grant(name: &str) -> Option<Grant> {
    match name {
        // Talk to the file service: send requests on the calling half.
        "fs" => Some(Grant::with(CALL, rights::SEND)),
        // Start further services.
        "control" => Some(Grant::full(CONTROL)),
        _ => None,
    }
}

k1k_rt::main!(main);
