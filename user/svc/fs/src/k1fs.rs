//! K1FS: the native crash-safe file system of K1OS (slice 1: on-disk format,
//! mkfs, A/B superblock mount).
//!
//! The design (docs/K1FS.ru.md) promises: A/B superblocks with generation and
//! CRC, copy-on-write metadata, and a mount that picks the newest *valid*
//! superblock, so a power failure leaves either the old or the new state.
//! This module fixes the concrete v1 format that the design document leaves
//! open and implements the first safe slice: creating a volume and mounting
//! it. Allocation, inodes, extents and directory pages arrive in later
//! slices; until then a fresh volume mounts with `root_block == ROOT_NONE`.
//!
//! # On-disk format v1 (fixed here, 2026-11-11)
//!
//! - The unit is a **4096-byte block** = 8 sectors of the 512-byte block
//!   protocol. Every K1FS structure is one block or a run of whole blocks;
//!   block numbers multiply by 8 to become LBAs.
//! - **Superblock slots**: block 0 (slot A) and block 1 (slot B), one 4 KiB
//!   block each. A commit writes the *other* slot with `generation + 1` and
//!   only that publish switches the volume root.
//! - **Superblock header** is the first 1024 bytes of the block, little-endian,
//!   the rest of the block is zero:
//!
//!   ```text
//!   0..4    magic "K1FS"
//!   4..8    header_crc32 — CRC32/IEEE of bytes 8..1024
//!   8..12   format_version u32 (= 1)
//!   12..16  header_size u32 (= 1024)
//!   16..24  generation u64 (0 = never written; mkfs writes 1)
//!   24..32  root_block u64 (ROOT_NONE while the volume has no root yet)
//!   32..40  total_blocks u64 (volume size in blocks)
//!   40..44  log_block_size u32 (= 12, blocks are 4096 bytes)
//!   44..48  flags u32 (0; bit definitions land with the features that use them)
//!   48..1024 reserved zero
//!   ```
//!
//! - **CRC32/IEEE 802.3** (reflected, polynomial 0xEDB88320): cheap in ring 3,
//!   table-driven, and good enough for torn-write detection on metadata.
//!
//! # Mount and commit contract
//!
//! Mount reads both slots and picks the highest `generation` among the slots
//! that pass magic, version, header size, CRC and fit the device; on a tie
//! slot A wins, so mkfs (which writes both slots at generation 1) mounts
//! deterministically. Both slots invalid is `NoSuperblock` — there is nothing
//! to guess from.
//!
//! The commit order this format implies (implemented slice by slice): write
//! new metadata to free blocks, verify their CRC, then write the other
//! superblock slot with `generation + 1`, and only after the device has
//! flushed treat the new root as live. `blkproto` has no FLUSH opcode yet —
//! the host regression's `Disk` flushes after every write, and the NVMe flush
//! that makes this contract real on hardware arrives with the commit slice.
//!
//! Not compiled into the fs service yet: the host regression in
//! `tools/k1fs-test.rs` builds this file the same way `tools/fat16-test.rs`
//! builds `fat.rs`; the ring-3 K1FS service wires it into the manifest later.

use crate::fat::BlockDevice;

/// Logical block size; all K1FS structures are whole blocks.
pub const BLOCK: usize = 4096;
/// 512-byte sectors per block, how block numbers map onto the block protocol.
pub const SECTORS_PER_BLOCK: u64 = 8;
/// Superblock magic at bytes 0..4 of a slot.
pub const MAGIC: [u8; 4] = *b"K1FS";
/// Bumped for every incompatible change of anything below.
pub const FORMAT_VERSION: u32 = 1;
/// Bytes of a slot block that carry the header; CRC covers 8..HEADER_SIZE.
pub const HEADER_SIZE: usize = 1024;
/// Slot A lives in block 0, slot B in block 1.
pub const SLOT_A_BLOCK: u64 = 0;
pub const SLOT_B_BLOCK: u64 = 1;
/// Smallest device mkfs accepts: the two superblock slots plus room to work.
pub const MIN_BLOCKS: u64 = 8;
/// `root_block` of a volume that has no root directory yet.
pub const ROOT_NONE: u64 = u64::MAX;

/// Why a superblock was rejected, or the volume cannot be used. Distinct
/// reasons matter: mount falls back to the other slot on any of these, and
/// the regression checks them one by one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum K1fsErr {
    /// The block device failed underneath us.
    Io,
    /// Bytes 0..4 are not "K1FS".
    BadMagic,
    /// A different format_version than this build reads.
    BadVersion,
    /// header_size is not HEADER_SIZE.
    BadHeader,
    /// The CRC does not match the header bytes.
    BadCrc,
    /// Neither slot is usable.
    NoSuperblock,
    /// Geometry that cannot be: smaller than MIN_BLOCKS or past the device.
    Inval,
}

/// The decoded superblock header: everything mount needs to choose a root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Superblock {
    pub generation: u64,
    pub root_block: u64,
    pub total_blocks: u64,
    pub log_block_size: u32,
}

/// A mounted K1FS volume: the winning superblock and which slot it came from
/// (`b'A'` or `b'B'` — the regression asserts the fallback picked the right
/// one).
pub struct K1fs {
    pub sb: Superblock,
    pub slot: u8,
}

fn crc32(data: &[u8]) -> u32 {
    const fn table() -> [u32; 256] {
        let mut t = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut c = i as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
                k += 1;
            }
            t[i] = c;
            i += 1;
        }
        t
    }
    const CRC_TABLE: [u32; 256] = table();
    let mut crc = !0u32;
    for &b in data {
        crc = CRC_TABLE[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

fn u32le(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

fn u64le(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

fn put_u32le(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u64le(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

/// Read one 4096-byte block through the 512-byte sector trait.
fn read_block(dev: &mut impl BlockDevice, block: u64, out: &mut [u8]) -> Result<(), K1fsErr> {
    if out.len() < BLOCK {
        return Err(K1fsErr::Inval);
    }
    dev.read(
        block.checked_mul(SECTORS_PER_BLOCK).ok_or(K1fsErr::Inval)?,
        SECTORS_PER_BLOCK as usize,
        out,
    )
    .map_err(|_| K1fsErr::Io)
}

/// Write one 4096-byte block. Slice 5 adds the flush that makes a superblock
/// publish durable; until then the host regression flushes per write.
fn write_block(dev: &mut impl BlockDevice, block: u64, data: &[u8]) -> Result<(), K1fsErr> {
    if data.len() < BLOCK {
        return Err(K1fsErr::Inval);
    }
    dev.write(
        block.checked_mul(SECTORS_PER_BLOCK).ok_or(K1fsErr::Inval)?,
        SECTORS_PER_BLOCK as usize,
        data,
    )
    .map_err(|_| K1fsErr::Io)
}

impl Superblock {
    /// Serialize into a zeroed slot block.
    pub fn encode(&self, block: &mut [u8; BLOCK]) {
        *block = [0u8; BLOCK];
        block[..4].copy_from_slice(&MAGIC);
        put_u32le(block, 8, FORMAT_VERSION);
        put_u32le(block, 12, HEADER_SIZE as u32);
        put_u64le(block, 16, self.generation);
        put_u64le(block, 24, self.root_block);
        put_u64le(block, 32, self.total_blocks);
        put_u32le(block, 40, self.log_block_size);
        put_u32le(block, 44, 0);
        let crc = crc32(&block[8..HEADER_SIZE]);
        put_u32le(block, 4, crc);
    }

    /// Parse and validate a slot block; `device_blocks` bounds total_blocks.
    pub fn decode(block: &[u8], device_blocks: u64) -> Result<Superblock, K1fsErr> {
        if block.len() < HEADER_SIZE {
            return Err(K1fsErr::Io);
        }
        if block[..4] != MAGIC {
            return Err(K1fsErr::BadMagic);
        }
        if crc32(&block[8..HEADER_SIZE]) != u32le(block, 4) {
            return Err(K1fsErr::BadCrc);
        }
        if u32le(block, 8) != FORMAT_VERSION {
            return Err(K1fsErr::BadVersion);
        }
        if u32le(block, 12) != HEADER_SIZE as u32 {
            return Err(K1fsErr::BadHeader);
        }
        let sb = Superblock {
            generation: u64le(block, 16),
            root_block: u64le(block, 24),
            total_blocks: u64le(block, 32),
            log_block_size: u32le(block, 40),
        };
        if sb.generation == 0
            || sb.total_blocks < MIN_BLOCKS
            || sb.total_blocks > device_blocks
            || sb.log_block_size != BLOCK.trailing_zeros()
        {
            return Err(K1fsErr::Inval);
        }
        Ok(sb)
    }
}

impl K1fs {
    /// Format a volume: both slots written at generation 1 with an empty root.
    /// Idempotent by construction — every byte of both slots is rewritten.
    pub fn mkfs(dev: &mut impl BlockDevice, total_blocks: u64) -> Result<(), K1fsErr> {
        if total_blocks < MIN_BLOCKS {
            return Err(K1fsErr::Inval);
        }
        let sb = Superblock {
            generation: 1,
            root_block: ROOT_NONE,
            total_blocks,
            log_block_size: BLOCK.trailing_zeros(),
        };
        let mut block = [0u8; BLOCK];
        sb.encode(&mut block);
        write_block(dev, SLOT_A_BLOCK, &block)?;
        write_block(dev, SLOT_B_BLOCK, &block)
    }

    /// Mount: the newest valid superblock wins, ties go to slot A. A slot
    /// whose magic, version and CRC check out but whose geometry cannot fit
    /// the device is an `Inval` volume, not an absent one.
    pub fn mount(dev: &mut impl BlockDevice, device_blocks: u64) -> Result<K1fs, K1fsErr> {
        let mut best: Option<(Superblock, u8)> = None;
        let mut bad_geometry = false;
        for (slot, block_no) in [(b'A', SLOT_A_BLOCK), (b'B', SLOT_B_BLOCK)] {
            let mut raw = [0u8; BLOCK];
            read_block(dev, block_no, &mut raw)?;
            let sb = match Superblock::decode(&raw, device_blocks) {
                Ok(sb) => sb,
                Err(K1fsErr::Inval) => {
                    bad_geometry = true;
                    continue;
                }
                Err(_) => continue,
            };
            let better = best.is_none_or(|(cur, cur_slot)| {
                sb.generation > cur.generation
                    || (sb.generation == cur.generation && slot == b'A' && cur_slot == b'B')
            });
            if better {
                best = Some((sb, slot));
            }
        }
        match best {
            Some((sb, slot)) => Ok(K1fs { sb, slot }),
            None if bad_geometry => Err(K1fsErr::Inval),
            None => Err(K1fsErr::NoSuperblock),
        }
    }
}
