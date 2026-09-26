//! The file service protocol: how a ring-3 service asks `fs` for files.
//!
//! `fs` is an ordinary capability service: it owns one endpoint, granted at
//! start, and every message a client sends arrives there. A client connects in
//! two messages, each carrying one capability — the shared buffer that carries
//! request arguments and results, and the endpoint its replies must go to.
//! After that a request is one message and the reply is the next one, in order,
//! so a client keeps exactly one request in flight.
//!
//! Nothing but the buffer crosses the boundary: paths, directory listings and
//! file contents are bytes in shared memory, so no service has to trust a
//! pointer and the kernel stays out of the file business entirely. A request
//! message is
//!
//! ```text
//! w0 = opcode      w1 = bytes written in the request block      w2 = result bytes wanted
//! ```
//!
//! and the reply carries `[status, arg0, arg1]` (word 3 stays empty). The
//! request block and the result block live at fixed offsets in the buffer, so
//! neither side has to describe its own memory in the message.

/// Fixed layout of the shared buffer, in bytes from its base.
pub const REQ_OFF: usize = 0;
/// A request never needs more than this; the rest of the first page is slack.
pub const REQ_MAX: usize = 64;
pub const DATA_OFF: usize = 128;

/// Client → server, capability attached: the shared request/result buffer.
/// The high 32 bits of w0 carry its size in pages.
pub const REGISTER_BUF: u64 = 1;
/// Client → server, capability attached: where replies to this client go.
pub const CONNECT: u64 = 2;
/// Request block `[path_off, path_len]`.
pub const OPEN: u64 = 3;
/// Request block `[handle, file_offset, dest_off, count]`.
pub const READ: u64 = 4;
/// Request block `[path_off, path_len]`.
pub const STAT: u64 = 5;
/// Request block `[path_off, path_len, max_records, rec_off]`.
pub const LIST: u64 = 6;
/// Request block `[handle]`.
pub const CLOSE: u64 = 7;
/// The client is going away; the server drops its state.
pub const BYE: u64 = 8;

/// Reply status codes, in `w0`. Anything but [`OK`] leaves `w1`/`w2` unset.
pub const OK: u64 = 0;
pub const NOENT: u64 = 1;
pub const ISDIR: u64 = 2;
pub const NOTDIR: u64 = 3;
pub const BADF: u64 = 4;
pub const IO: u64 = 5;
pub const INVAL: u64 = 6;
pub const NOSPC: u64 = 7;
pub const TOOBIG: u64 = 8;
pub const NOCONN: u64 = 9;

/// Bytes one directory listing record occupies: the 8.3 name, the attribute
/// byte and the size.
pub const DIR_REC: usize = 16;
/// Longest path a client may ask for.
pub const MAX_PATH: usize = 255;
/// Clients a server tracks at once.
pub const MAX_CLIENTS: usize = 16;
/// Largest single `read`. The server copies out of its own cluster buffer, so
/// the real limit is the smaller of this and the registered buffer.
pub const MAX_READ: usize = 32 * 1024;

/// A directory entry as the server reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirRec {
    pub name: [u8; 11],
    pub attr: u8,
    pub size: u32,
}

impl DirRec {
    pub fn is_dir(&self) -> bool {
        self.attr & 0x10 != 0
    }

    /// "NAME    EXT" → "NAME.EXT", the way the directory stores it.
    pub fn display<'a>(&self, out: &'a mut [u8; 13]) -> &'a str {
        let mut n = 0;
        for &b in &self.name[..8] {
            if b == b' ' {
                break;
            }
            out[n] = b;
            n += 1;
        }
        if self.name[8] != b' ' {
            out[n] = b'.';
            n += 1;
            for &b in &self.name[8..] {
                if b == b' ' {
                    break;
                }
                out[n] = b;
                n += 1;
            }
        }
        core::str::from_utf8(&out[..n]).unwrap_or("?")
    }
}

/// Metadata about a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub size: u64,
    pub is_dir: bool,
}

pub fn status_name(code: u64) -> &'static str {
    match code {
        OK => "ok",
        NOENT => "no such file",
        ISDIR => "is a directory",
        NOTDIR => "not a directory",
        BADF => "bad file handle",
        IO => "i/o error",
        INVAL => "invalid request",
        NOSPC => "buffer too small",
        TOOBIG => "request too large",
        NOCONN => "not connected",
        _ => "unknown status",
    }
}

/// Read up to [`REQ_MAX`] bytes of a client's request block.
pub fn req_u64(buf: &[u8], index: usize) -> u64 {
    let off = REQ_OFF + index * 8;
    if off + 8 > buf.len() {
        return 0;
    }
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(b)
}

/// The bytes a client wrote into the data area, bounds-checked.
pub fn req_bytes(buf: &[u8], off: u64, len: u64) -> Option<&[u8]> {
    let off = off as usize;
    let len = len as usize;
    let end = off.checked_add(len)?;
    if end > buf.len() {
        return None;
    }
    Some(&buf[off..end])
}

/// The mutable part of a client's data area, bounds-checked.
pub fn req_mut(buf: &mut [u8], off: u64, len: u64) -> Option<&mut [u8]> {
    let off = off as usize;
    let len = len as usize;
    let end = off.checked_add(len)?;
    if end > buf.len() {
        return None;
    }
    Some(&mut buf[off..end])
}

/// Write one listing record into a client's data area.
pub fn put_dir_rec(buf: &mut [u8], off: u64, rec: &DirRec) -> bool {
    let Some(dst) = req_mut(buf, off, DIR_REC as u64) else {
        return false;
    };
    dst[..11].copy_from_slice(&rec.name);
    dst[11] = rec.attr;
    dst[12..16].copy_from_slice(&rec.size.to_le_bytes());
    true
}
