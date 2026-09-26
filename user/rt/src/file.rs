//! Client side of the [`crate::fsproto`] protocol: a service's handle on the
//! file server.

use alloc::vec::Vec;

use crate::fsproto::*;
use crate::{
    Cap, Error, Result, ep_create, log, mem_create, mem_map, recv, rights, send, send_cap,
};

/// Pages of shared memory a client registers by default: room for a path, a
/// full [`MAX_READ`] and a directory listing.
pub const DEFAULT_PAGES: usize = 16;

/// Log which half of the handshake failed and pass the error on: the codes are
/// kernel-level and mean little without the step that produced them.
fn step(what: &str, e: Error) -> Error {
    log!("file client: cannot {}: {:?}", what, e);
    e
}

/// An open file on the server. Handles are small integers owned by the server,
/// valid until [`FileClient::close`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct File(pub u32);

/// A connected file-service client: one shared buffer plus one reply endpoint.
/// Rebuilt from raw parts each time: a `&mut [u8]` borrowed from `&self` would
/// be a lie the borrow checker rightly refuses.
///
/// # Safety
/// `base` must point at `len` writable bytes that outlive the returned slice,
/// and nothing else may write them while the slice is alive.
unsafe fn view<'a>(base: *mut u8, len: usize) -> &'a mut [u8] {
    unsafe { core::slice::from_raw_parts_mut(base, len) }
}

pub struct FileClient {
    listen: Cap,
    reply: Cap,
    buf: Cap,
    base: *mut u8,
    len: usize,
}

impl FileClient {
    /// Connect to the file server listening on `listen`.
    ///
    /// `listen` is the server's endpoint (the service finds it with
    /// [`crate::granted`]); this registers a `pages`-page buffer and a reply
    /// endpoint under the calling task.
    pub fn new(listen: Cap, pages: usize) -> Result<Self> {
        let pages = pages.clamp(2, 64);
        // Name every step: "Perm" on its own tells a service author nothing.
        let buf = mem_create(pages).map_err(|e| step("create the shared buffer", e))?;
        let base = mem_map(buf, true).map_err(|e| step("map the shared buffer", e))?;
        let reply = ep_create().map_err(|e| step("create a reply endpoint", e))?;
        send_cap(listen, reply, rights::SEND, CONNECT).map_err(|e| step("connect", e))?;
        send_cap(
            listen,
            buf,
            rights::MAP_READ | rights::MAP_WRITE,
            REGISTER_BUF | (pages as u64) << 32,
        )
        .map_err(|e| step("register the shared buffer", e))?;
        Ok(FileClient {
            listen,
            reply,
            buf,
            base,
            len: pages * 4096,
        })
    }

    /// Send a request block and wait for the reply. `args` are the request
    /// block words; the caller has already put any bytes it needs into the
    /// data area.
    fn request(&self, op: u64, args: &[u64; 4], want: usize) -> Result<[u64; 4]> {
        let buf = unsafe { view(self.base, self.len) };
        if REQ_OFF + 32 > buf.len() {
            return Err(Error::NoMem);
        }
        for (i, a) in args.iter().enumerate() {
            let off = REQ_OFF + i * 8;
            buf[off..off + 8].copy_from_slice(&a.to_le_bytes());
        }
        send(self.listen, op, 32, want as u64)?;
        let m = recv(self.reply)?;
        if m.words[0] != OK {
            return Ok(m.words);
        }
        Ok(m.words)
    }

    fn put_path(&self, path: &str) -> Result<u64> {
        if path.is_empty() || path.len() > MAX_PATH {
            return Err(Error::Inval);
        }
        let buf = unsafe { view(self.base, self.len) };
        let dst = buf
            .get_mut(DATA_OFF..DATA_OFF + path.len())
            .ok_or(Error::NoMem)?;
        dst.copy_from_slice(path.as_bytes());
        Ok(DATA_OFF as u64)
    }

    /// Open `path` for reading.
    pub fn open(&self, path: &str) -> Result<File> {
        let off = self.put_path(path)?;
        let r = self.request(OPEN, &[off, path.len() as u64, 0, 0], 0)?;
        if r[0] != OK {
            return Err(Error::Inval);
        }
        if r[1] == 0 {
            return Err(Error::Inval);
        }
        Ok(File(r[1] as u32))
    }

    pub fn close(&self, f: File) -> Result<()> {
        self.request(CLOSE, &[f.0 as u64, 0, 0, 0], 0)?;
        Ok(())
    }

    /// Read up to `buf.len()` bytes at `offset`; returns how many arrived. A
    /// short read means end of file.
    pub fn read(&self, f: File, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let dest = DATA_OFF;
        if dest + buf.len().min(MAX_READ) > self.len {
            return Err(Error::NoMem);
        }
        let count = buf.len().min(MAX_READ);
        let r = self.request(
            READ,
            &[f.0 as u64, offset, dest as u64, count as u64],
            count,
        )?;
        if r[0] != OK {
            return Err(Error::Inval);
        }
        let n = (r[1] as usize).min(count);
        // The server filled the data area of our shared buffer; hand the bytes
        // to the caller from there.
        let shared = unsafe { view(self.base, self.len) };
        buf[..n].copy_from_slice(&shared[dest..dest + n]);
        Ok(n)
    }

    /// Read a whole file, growing `out` as needed. Returns the byte count.
    pub fn read_all(&self, f: File, out: &mut Vec<u8>) -> Result<usize> {
        let mut off = 0u64;
        let mut chunk = [0u8; 1024];
        loop {
            let n = self.read(f, off, &mut chunk)?;
            if n == 0 {
                return Ok(off as usize);
            }
            out.extend_from_slice(&chunk[..n]);
            off += n as u64;
        }
    }

    pub fn stat(&self, path: &str) -> Result<Stat> {
        let off = self.put_path(path)?;
        let r = self.request(STAT, &[off, path.len() as u64, 0, 0], 0)?;
        if r[0] != OK {
            return Err(Error::Inval);
        }
        Ok(Stat {
            size: r[1],
            is_dir: r[2] != 0,
        })
    }

    /// List a directory into `out`, returning how many entries were written.
    pub fn list(&self, path: &str, out: &mut [DirRec]) -> Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let path_off = self.put_path(path)?;
        let rec_off = DATA_OFF + MAX_PATH + 8;
        let want = out.len() * DIR_REC;
        if rec_off + want > self.len {
            return Err(Error::NoMem);
        }
        let r = self.request(
            LIST,
            &[
                path_off,
                path.len() as u64,
                out.len() as u64,
                rec_off as u64,
            ],
            want,
        )?;
        if r[0] != OK {
            return Err(Error::Inval);
        }
        let n = (r[1] as usize).min(out.len());
        let buf = unsafe { view(self.base, self.len) };
        for (i, slot) in out.iter_mut().enumerate().take(n) {
            let off = rec_off + i * DIR_REC;
            let src = &buf[off..off + DIR_REC];
            let mut name = [0u8; 11];
            name.copy_from_slice(&src[..11]);
            let mut size = [0u8; 4];
            size.copy_from_slice(&src[12..16]);
            *slot = DirRec {
                name,
                attr: src[11],
                size: u32::from_le_bytes(size),
            };
        }
        Ok(n)
    }

    /// Tell the server this client is going away.
    pub fn disconnect(&mut self) {
        let _ = send(self.listen, BYE, 0, 0);
    }

    pub fn buffer_cap(&self) -> Cap {
        self.buf
    }
}
