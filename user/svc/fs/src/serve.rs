//! The request side of the file service: one loop, one buffer per client.
//!
//! Every request names a client task, and the server only ever writes into the
//! buffer that client registered and replies on the endpoint that client
//! handed over — so a service cannot reach another service's memory or make
//! the server answer on somebody else's endpoint.

use k1k_rt::fsproto::*;
use k1k_rt::server::Server;
use k1k_rt::{info, log};

use crate::fat::{DirEntry, Fat};
use crate::{Blk, SERVE};

/// Open files a client may hold at once, across all clients.
const MAX_OPEN: usize = 32;
/// Longest path in a request, in bytes.
const MAX_PATH: usize = 255;

type Reply = (u64, u64, u64);

/// Serve until the task is killed. There is nothing to lose by stopping: the
/// supervisor restarts `fs`, which re-reads the manifest.
pub fn serve(fs: &mut Fat, dev: &mut Blk, scratch: &mut [u8]) -> ! {
    let mut srv = Server::new(SERVE);
    let mut open: [Option<DirEntry>; MAX_OPEN] = [None; MAX_OPEN];
    log!(
        "serving files on task {} (up to {} open files, {} clients)",
        info().task_id,
        MAX_OPEN,
        k1k_rt::fsproto::MAX_CLIENTS
    );
    loop {
        let Some(req) = srv.recv() else {
            log!("lost the client endpoint; exiting for the supervisor to restart");
            k1k_rt::exit(3)
        };
        let r = dispatch(&mut srv, fs, dev, scratch, &mut open, &req);
        if srv.reply(&req, r.0, r.1, r.2).is_err() {
            log!("cannot reply to task {}", req.client);
        }
    }
}

fn dispatch(
    srv: &mut Server,
    fs: &mut Fat,
    dev: &mut Blk,
    scratch: &mut [u8],
    open: &mut [Option<DirEntry>; MAX_OPEN],
    req: &k1k_rt::server::Request,
) -> Reply {
    // The client says how much of the request block it filled; refuse anything
    // past the block rather than reading whatever happens to be there.
    if req.req_len == 0 || req.req_len > REQ_MAX as u64 {
        return (INVAL, 0, 0);
    }
    let Some((ptr, len)) = srv.client_buffer(req.client) else {
        return (NOCONN, 0, 0);
    };
    if len < DATA_OFF {
        return (NOSPC, 0, 0);
    }
    let buf: &mut [u8] = unsafe { core::slice::from_raw_parts_mut(ptr, len) };

    let result = match req.op {
        OPEN => op_open(fs, dev, scratch, open, buf),
        READ => op_read(fs, dev, scratch, open, buf),
        STAT => op_stat(fs, dev, scratch, buf),
        LIST => op_list(fs, dev, scratch, buf),
        CLOSE => op_close(open, buf),
        _ => (INVAL, 0, 0),
    };
    if result.0 != OK {
        log!(
            "request {} from task {} failed: {} (req_len {} want {} buf {} w[0..2] {} {})",
            req.op,
            req.client,
            status_name(result.0),
            req.req_len,
            req.want,
            len,
            req_u64(buf, 0),
            req_u64(buf, 1),
        );
    }
    result
}

/// Read a path out of the client's data area, bounds- and length-checked.
fn take_path(buf: &[u8]) -> Result<&str, u64> {
    let off = req_u64(buf, 0);
    let len = req_u64(buf, 1);
    if len == 0 || len as usize > MAX_PATH {
        return Err(INVAL);
    }
    let bytes = req_bytes(buf, off, len).ok_or(INVAL)?;
    core::str::from_utf8(bytes).map_err(|_| INVAL)
}

fn op_open(
    fs: &mut Fat,
    dev: &mut Blk,
    scratch: &mut [u8],
    open: &mut [Option<DirEntry>; MAX_OPEN],
    buf: &[u8],
) -> Reply {
    let path = match take_path(buf) {
        Ok(p) => p,
        Err(e) => return (e, 0, 0),
    };
    let (dir, _leaf) = match crate::paths::split_parent(fs, dev, path, scratch) {
        Ok(v) => v,
        Err(()) => return (NOENT, 0, 0),
    };
    let entry = match fs.lookup(dev, dir, crate::paths::leaf_name(path), scratch) {
        Ok(Some(e)) => e,
        Ok(None) => return (NOENT, 0, 0),
        Err(()) => return (IO, 0, 0),
    };
    if entry.is_dir() {
        return (ISDIR, 0, 0);
    }
    // A handle is the slot number plus one, so zero always means "none".
    for (i, slot) in open.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(entry);
            return (OK, i as u64 + 1, 0);
        }
    }
    (NOSPC, 0, 0)
}

fn op_read(
    fs: &mut Fat,
    dev: &mut Blk,
    scratch: &mut [u8],
    open: &[Option<DirEntry>; MAX_OPEN],
    buf: &mut [u8],
) -> Reply {
    let handle = req_u64(buf, 0);
    let offset = req_u64(buf, 1);
    let dest = req_u64(buf, 2);
    let count = req_u64(buf, 3);
    if count == 0 {
        return (OK, 0, 0);
    }
    if count as usize > MAX_READ {
        return (TOOBIG, 0, 0);
    }
    let Some(entry) = handle_of(open, handle) else {
        return (BADF, 0, 0);
    };
    let Some(dst) = req_mut(buf, dest, count) else {
        return (NOSPC, 0, 0);
    };
    match fs.read_at(dev, &entry, offset, scratch, dst) {
        Ok(n) => (OK, n as u64, 0),
        Err(()) => (IO, 0, 0),
    }
}

fn op_stat(fs: &mut Fat, dev: &mut Blk, scratch: &mut [u8], buf: &[u8]) -> Reply {
    let path = match take_path(buf) {
        Ok(p) => p,
        Err(e) => return (e, 0, 0),
    };
    if path == "/" {
        return (OK, 0, 1);
    }
    let (dir, _leaf) = match crate::paths::split_parent(fs, dev, path, scratch) {
        Ok(v) => v,
        Err(()) => return (NOENT, 0, 0),
    };
    match fs.lookup(dev, dir, crate::paths::leaf_name(path), scratch) {
        Ok(Some(e)) => (OK, e.size as u64, if e.is_dir() { 1 } else { 0 }),
        Ok(None) => (NOENT, 0, 0),
        Err(()) => (IO, 0, 0),
    }
}

fn op_list(fs: &mut Fat, dev: &mut Blk, scratch: &mut [u8], buf: &mut [u8]) -> Reply {
    let path = match take_path(buf) {
        Ok(p) => p,
        Err(e) => return (e, 0, 0),
    };
    let max = req_u64(buf, 2);
    let rec_off = req_u64(buf, 3);
    if max == 0 || max as usize > MAX_READ / DIR_REC {
        return (INVAL, 0, 0);
    }
    // Make sure the whole listing fits before touching anything.
    let want = max as usize * DIR_REC;
    if req_bytes(buf, rec_off, want as u64).is_none() {
        return (NOSPC, 0, 0);
    }
    let cluster = if path == "/" {
        0
    } else {
        let (dir, _leaf) = match crate::paths::split_parent(fs, dev, path, scratch) {
            Ok(v) => v,
            Err(()) => return (NOENT, 0, 0),
        };
        match fs.lookup(dev, dir, crate::paths::leaf_name(path), scratch) {
            Ok(Some(e)) if e.is_dir() => e.cluster,
            Ok(Some(_)) => return (NOTDIR, 0, 0),
            Ok(None) => return (NOENT, 0, 0),
            Err(()) => return (IO, 0, 0),
        }
    };
    let mut count = 0u64;
    let mut full = false;
    let result = fs.read_dir(dev, cluster, scratch, |e| {
        if count < max {
            let rec = DirRec {
                name: e.name,
                attr: e.attr,
                size: e.size,
            };
            if !put_dir_rec(buf, rec_off + count as u64 * DIR_REC as u64, &rec) {
                full = true;
            }
            count += 1;
        }
    });
    if result.is_err() {
        return (IO, 0, 0);
    }
    if full {
        return (NOSPC, 0, 0);
    }
    (OK, count, 0)
}

fn op_close(open: &mut [Option<DirEntry>; MAX_OPEN], buf: &[u8]) -> Reply {
    let handle = req_u64(buf, 0);
    match open.get_mut(handle.wrapping_sub(1) as usize) {
        Some(slot) if slot.is_some() => {
            *slot = None;
            (OK, 0, 0)
        }
        _ => (BADF, 0, 0),
    }
}

fn handle_of(open: &[Option<DirEntry>; MAX_OPEN], handle: u64) -> Option<DirEntry> {
    if handle == 0 {
        return None;
    }
    open.get(handle as usize - 1).and_then(|s| *s)
}
