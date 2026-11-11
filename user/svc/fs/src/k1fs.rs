//! K1FS: the native crash-safe file system of K1OS.
//!
//! Slices 1-3 are here: the on-disk format, mkfs, A/B superblock mount, the
//! pure copy-on-write allocator, and the read-only path — mount scans and
//! validates the whole reachable tree, paths resolve through sorted directory
//! pages, files read back through inline data or extents (with the EXT chain
//! and holes). What is deliberately not here yet: the mutation operations
//! (create/unlink/rename as commit sequences), the block-level FLUSH that
//! makes a superblock publish durable on hardware, the ring-3 service and the
//! fs protocol extensions, and everything crypto (K1FS-CRYPT).
//!
//! The design (docs/K1FS.ru.md) promises: A/B superblocks with generation and
//! CRC, copy-on-write metadata, and a mount that picks the newest *valid*
//! superblock, so a power failure leaves either the old or the new state.
//! The metadata formats (inode, directory pages, extent blocks) were drafted
//! by a subagent and reviewed into docs/K1FS.ru.md.
//!
//! # On-disk format v1 (fixed here, 2026-11-11)
//!
//! - The unit is a **4096-byte block** = 8 sectors of the 512-byte block
//!   protocol. Every K1FS structure is one block or a run of whole blocks;
//!   block numbers multiply by 8 to become LBAs.
//! - **Superblock slots**: block 0 (slot A) and block 1 (slot B), one 4 KiB
//!   block each. A commit writes the *other* slot with `generation + 1` and
//!   only that publish switches the volume root. Blocks from 2 up are
//!   copy-on-write payload, handed out by the in-memory allocator.
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

use alloc::vec::Vec;

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
/// Blocks 0 and 1 are the superblock slots; everything from here on is
/// ordinary CoW payload, allocated from the in-memory map.
pub const FIRST_DATA_BLOCK: u64 = 2;
/// Smallest device mkfs accepts: the two superblock slots plus room to work.
pub const MIN_BLOCKS: u64 = 8;
/// `root_block` of a volume that has no root directory yet.
pub const ROOT_NONE: u64 = u64::MAX;

/// Metadata block magic at bytes 0..4 of every block the tree references.
pub const META_MAGIC: [u8; 4] = *b"K1MB";
/// Metadata block types.
pub const T_INODE: u32 = 1;
pub const T_DIRPAGE: u32 = 2;
pub const T_EXT: u32 = 3;
/// Metadata block header bytes; the body follows at this offset.
pub const META_HDR: usize = 64;
/// Longest file or directory name, bytes of UTF-8.
pub const NAME_MAX: usize = 255;
/// Inode body layout: payload starts here...
pub const INODE_BODY: usize = 112;
/// ...and a file this small lives inline in its inode, no data blocks.
pub const INLINE_MAX: usize = BLOCK - INODE_BODY;
/// Extents stored in the inode itself; longer files chain EXT blocks.
pub const INODE_EXTENTS: usize = INLINE_MAX / 16;
/// Extents per EXT block: `(4096 - 80) / 16`.
pub const EXT_EXTENTS: usize = 251;
/// Directory page body header ends here; entries pack from this offset.
pub const DIR_BODY: usize = 96;
/// Directory page capacity for entries, bytes.
pub const DIR_PAYLOAD: usize = BLOCK - DIR_BODY;
/// POSIX-shaped type bits in `Inode::mode` (permissions are stored, not
/// enforced, in v1).
pub const MODE_DIR: u16 = 0o040000;
pub const MODE_REG: u16 = 0o100000;

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
    /// The volume has no free block left.
    NoSpc,
    /// A metadata block at this number failed magic/type validation.
    BadMeta(u64),
    /// A metadata block at this number failed its CRC.
    MetaCrc(u64),
    /// A metadata block claims to live at a different block number.
    MetaSelf(u64),
    /// A metadata block is from a newer generation than the mounted superblock.
    MetaFuture(u64),
    /// Two references claim this block: corruption, and the cycle detector.
    Alias(u64),
    /// A metadata block is structurally broken (bounds, counts, name, order).
    Corrupt(u64),
    /// A path component does not exist.
    NoEnt,
    /// A path component that must be a directory is not one.
    NotDir,
}

/// The decoded superblock header: everything mount needs to choose a root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Superblock {
    pub generation: u64,
    pub root_block: u64,
    pub total_blocks: u64,
    pub log_block_size: u32,
}

/// A mounted K1FS volume: the winning superblock, which slot it came from
/// (`b'A'` or `b'B'` — the regression asserts the fallback picked the right
/// one) and the in-memory allocation map.
///
/// # The allocator is pure copy-on-write
///
/// There is no allocation bitmap on disk. Mount derives the used set from
/// the committed state — today the superblock slots and `root_block`, the
/// tree walk of slice 3+ — and every other block is free. `alloc` hands out
/// free blocks from that map, `free` returns them before a commit. Two
/// consequences, both wanted:
///
/// - After a commit, the old version's exclusive blocks are unreachable
///   from the new root and become free space again — garbage collection is
///   not a facility, it is the CoW invariant.
/// - A crash mid-commit writes blocks nothing reachable points at; the old
///   superblock still mounts and sees them as free. The worst failure is a
///   few dead blocks, never double ownership.
///
/// The map is RAM-only (`total_blocks` bits), which the "bounded RAM" goal
/// accepts: the K1K volumes this serves stay in the thousands of blocks.
pub struct K1fs {
    pub sb: Superblock,
    pub slot: u8,
    used: Vec<u64>,
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
        let (sb, slot) = match best {
            Some(best) => best,
            None if bad_geometry => return Err(K1fsErr::Inval),
            None => return Err(K1fsErr::NoSuperblock),
        };
        let mut fs = K1fs {
            sb,
            slot,
            used: vec![0u64; ((sb.total_blocks + 63) / 64) as usize],
        };
        fs.set_used(SLOT_A_BLOCK);
        fs.set_used(SLOT_B_BLOCK);
        if sb.root_block != ROOT_NONE {
            if sb.root_block < FIRST_DATA_BLOCK || sb.root_block >= sb.total_blocks {
                return Err(K1fsErr::Inval);
            }
            fs.scan(dev)?;
        }
        Ok(fs)
    }

    /// Claim `block` for the mounted state; a block claimed twice is
    /// [`K1fsErr::Alias`] — the visited set is also what makes the tree walk
    /// terminate on cycles.
    fn claim(&mut self, block: u64) -> Result<(), K1fsErr> {
        if block >= self.sb.total_blocks {
            return Err(K1fsErr::Corrupt(block));
        }
        if self.is_used_bit(block) {
            return Err(K1fsErr::Alias(block));
        }
        self.set_used(block);
        Ok(())
    }

    /// Walk the committed tree from `root_block`, validating every reachable
    /// metadata block and marking every reachable block (metadata and data)
    /// used. Data blocks are bounds-checked and claimed, never read — scan
    /// cost is proportional to the metadata, not the payload. Any reachable
    /// block that fails validation aborts the mount: with no journal and no
    /// fsck, a silently mounted damaged directory is worse than an honest
    /// refusal that names the block.
    fn scan(&mut self, dev: &mut impl BlockDevice) -> Result<(), K1fsErr> {
        let mut queue: Vec<u64> = alloc::vec![self.sb.root_block];
        while let Some(block) = queue.pop() {
            if self.is_used_bit(block) {
                return Err(K1fsErr::Alias(block));
            }
            let mut raw = [0u8; BLOCK];
            read_block(dev, block, &mut raw)?;
            let mtype = meta_check(&raw, block, self.sb.generation)?;
            self.set_used(block);
            match mtype {
                T_INODE => {
                    let inode = inode_decode(&raw, block)?;
                    for extent in inode_data_extents(&inode) {
                        if !extent.hole {
                            self.claim_extent(*extent, block)?;
                        }
                    }
                    if inode.ext_overflow != ROOT_NONE {
                        queue.push(inode.ext_overflow);
                    }
                    if inode.is_dir() && inode.dir_first != ROOT_NONE {
                        queue.push(inode.dir_first);
                    }
                }
                T_DIRPAGE => {
                    let page = dirpage_decode(&raw, block, self.sb.total_blocks)?;
                    if page.next != ROOT_NONE {
                        queue.push(page.next);
                    }
                    for entry in &page.entries {
                        queue.push(entry.target);
                    }
                }
                T_EXT => {
                    let (next, extents) = ext_decode(&raw, block, self.sb.total_blocks)?;
                    for extent in &extents {
                        if !extent.hole {
                            self.claim_extent(*extent, block)?;
                        }
                    }
                    if next != ROOT_NONE {
                        queue.push(next);
                    }
                }
                _ => unreachable!("meta_check accepts only known types"),
            }
        }
        Ok(())
    }

    /// Claim the blocks one extent covers. Adjacent extents of one inode may
    /// not overlap by construction of the writers; anything already claimed
    /// (another inode's extent, a metadata block) is [`K1fsErr::Alias`].
    fn claim_extent(&mut self, extent: Extent, owner: u64) -> Result<(), K1fsErr> {
        let end = extent.lba + extent.nblocks as u64;
        if extent.lba < FIRST_DATA_BLOCK || end > self.sb.total_blocks {
            return Err(K1fsErr::Corrupt(owner));
        }
        for block in extent.lba..end {
            self.claim(block)?;
        }
        Ok(())
    }

    fn is_used_bit(&self, block: u64) -> bool {
        self.used[(block / 64) as usize] & (1u64 << (block % 64)) != 0
    }

    fn set_used(&mut self, block: u64) {
        self.used[(block / 64) as usize] |= 1u64 << (block % 64);
    }

    /// Whether `block` is handed out. Slot blocks always are.
    pub fn is_used(&self, block: u64) -> bool {
        block < self.sb.total_blocks && self.is_used_bit(block)
    }

    /// Hand out one free data block, the lowest-numbered free block there is
    /// — mkfs-sized volumes stay deterministic in the regression.
    pub fn alloc(&mut self) -> Result<u64, K1fsErr> {
        for block in FIRST_DATA_BLOCK..self.sb.total_blocks {
            if !self.is_used_bit(block) {
                self.set_used(block);
                return Ok(block);
            }
        }
        Err(K1fsErr::NoSpc)
    }

    /// Return a data block to the pool. Only meaningful before the commit
    /// that drops the last reference; freeing a slot block is refused.
    pub fn free(&mut self, block: u64) {
        if block >= FIRST_DATA_BLOCK && block < self.sb.total_blocks {
            self.used[(block / 64) as usize] &= !(1u64 << (block % 64));
        }
    }

    /// Publish `root_block` (or [`ROOT_NONE`] for an emptied volume) as the
    /// new root: the *other* superblock slot gets `generation + 1`. A crash
    /// before the slot write lands leaves the previous slot authoritative —
    /// that is the whole transaction.
    pub fn commit(&mut self, dev: &mut impl BlockDevice, root_block: u64) -> Result<(), K1fsErr> {
        if root_block != ROOT_NONE
            && (root_block < FIRST_DATA_BLOCK || root_block >= self.sb.total_blocks)
        {
            return Err(K1fsErr::Inval);
        }
        let (slot, slot_block) = match self.slot {
            b'A' => (b'B', SLOT_B_BLOCK),
            _ => (b'A', SLOT_A_BLOCK),
        };
        let sb = Superblock {
            generation: self.sb.generation + 1,
            root_block,
            total_blocks: self.sb.total_blocks,
            log_block_size: self.sb.log_block_size,
        };
        let mut raw = [0u8; BLOCK];
        sb.encode(&mut raw);
        write_block(dev, slot_block, &raw)?;
        self.sb = sb;
        self.slot = slot;
        Ok(())
    }
}

/// One run of contiguous blocks. Extents sit in the file's logical order,
/// with no logical offset of their own: the reader accumulates `nblocks`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub lba: u64,
    pub nblocks: u32,
    pub hole: bool,
}

/// A decoded inode. Files carry inline data or extents (the EXT chain is
/// loaded on demand by the read path); directories carry the head of their
/// page chain. Offsets 80..96 differ by kind on disk — one struct here, the
/// codec picks by `mode`.
#[derive(Debug, Clone)]
pub struct Inode {
    pub mode: u16,
    pub size: u64,
    pub inline: Vec<u8>,
    pub extents: Vec<Extent>,
    pub ext_overflow: u64,
    pub dir_first: u64,
    pub dir_pages: u64,
}

/// One live directory entry. `DEAD` entries are skipped by the reader and
/// never written by v1 writers (removal is compaction); the flag exists so a
/// future in-place mode stays format-compatible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: Vec<u8>,
    pub kind: u8,
    pub target: u64,
}

/// A decoded directory page.
#[derive(Debug, Clone)]
pub struct DirPage {
    pub next: u64,
    pub entries: Vec<DirEntry>,
}

/// The extents that address payload: the inode's own plus, transparently to
/// the caller, none of the EXT chain (that is read separately).
fn inode_data_extents(inode: &Inode) -> &[Extent] {
    &inode.extents
}

impl Inode {
    pub fn is_dir(&self) -> bool {
        self.mode & 0o170000 == MODE_DIR
    }

    /// An empty directory inode: no page chain yet.
    pub fn empty_dir() -> Inode {
        Inode {
            mode: MODE_DIR,
            size: 0,
            inline: Vec::new(),
            extents: Vec::new(),
            ext_overflow: ROOT_NONE,
            dir_first: ROOT_NONE,
            dir_pages: 0,
        }
    }

    /// A regular file from inline content (the only form small files take).
    pub fn file(inline: &[u8]) -> Result<Inode, K1fsErr> {
        if inline.len() > INLINE_MAX {
            return Err(K1fsErr::Inval);
        }
        Ok(Inode {
            mode: MODE_REG,
            size: inline.len() as u64,
            inline: inline.to_vec(),
            extents: Vec::new(),
            ext_overflow: ROOT_NONE,
            dir_first: ROOT_NONE,
            dir_pages: 0,
        })
    }

    /// Serialize into a metadata block claiming `self_block`, stamped with
    /// `generation` (the commit that will publish it: sb.generation + 1).
    pub fn encode(&self, self_block: u64, generation: u64) -> [u8; BLOCK] {
        let mut raw = [0u8; BLOCK];
        put_u16le(&mut raw, INODE_BODY, self.mode);
        put_u16le(&mut raw, INODE_BODY + 2, 1); // nlink: hardlinks do not exist in v1
        put_u32le(
            &mut raw,
            INODE_BODY + 4,
            if self.inline.is_empty() { 0 } else { 1 },
        );
        put_u64le(&mut raw, INODE_BODY + 8, self.size);
        if self.is_dir() {
            put_u64le(&mut raw, INODE_BODY + 16, self.dir_pages);
            put_u64le(&mut raw, INODE_BODY + 24, self.dir_first);
        } else {
            put_u64le(&mut raw, INODE_BODY + 16, self.extents.len() as u64);
            put_u32le(&mut raw, INODE_BODY + 24, self.inline.len() as u32);
            put_u64le(&mut raw, INODE_BODY + 32, self.ext_overflow);
        }
        let body = &mut raw[INODE_BODY + 40..];
        if !self.inline.is_empty() {
            body[..self.inline.len()].copy_from_slice(&self.inline);
        } else {
            for (i, extent) in self.extents.iter().enumerate() {
                write_extent(&mut body[i * 16..], extent);
            }
        }
        meta_write_header(&mut raw, T_INODE, generation, self_block);
        raw
    }
}

impl DirPage {
    pub fn new(entries: Vec<DirEntry>) -> DirPage {
        DirPage {
            next: ROOT_NONE,
            entries,
        }
    }

    /// Serialize, sorting entries by name (unsigned byte order — for UTF-8
    /// that is codepoint order) and packing them from DIR_BODY.
    pub fn encode(&self, self_block: u64, generation: u64) -> Result<[u8; BLOCK], K1fsErr> {
        let mut raw = [0u8; BLOCK];
        let mut entries = self.entries.clone();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let mut off = DIR_BODY;
        for entry in &entries {
            let rec_len = entry_len(entry);
            if off + rec_len > BLOCK {
                return Err(K1fsErr::NoSpc);
            }
            raw[off] = (entry.name.len() & 0xff) as u8;
            raw[off + 1] = (entry.name.len() >> 8) as u8;
            put_u16le(&mut raw, off + 2, rec_len as u16);
            raw[off + 4] = entry.kind;
            raw[off + 8..off + 16].copy_from_slice(&entry.target.to_le_bytes());
            raw[off + 16..off + 16 + entry.name.len()].copy_from_slice(&entry.name);
            off += rec_len;
        }
        put_u64le(&mut raw, META_HDR, self.next);
        put_u32le(&mut raw, META_HDR + 8, entries.len() as u32);
        put_u32le(&mut raw, META_HDR + 12, 0); // dead_bytes: v1 writers emit none
        put_u32le(&mut raw, META_HDR + 16, off as u32);
        meta_write_header(&mut raw, T_DIRPAGE, generation, self_block);
        Ok(raw)
    }
}

fn entry_len(entry: &DirEntry) -> usize {
    (16 + entry.name.len() + 7) / 8 * 8
}

fn put_u16le(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

fn u16le(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(buf[off..off + 2].try_into().unwrap())
}

fn write_extent(slot: &mut [u8], extent: &Extent) {
    put_u64le(slot, 0, extent.lba);
    put_u32le(slot, 8, extent.nblocks);
    put_u32le(slot, 12, if extent.hole { 1 } else { 0 });
}

fn read_extent(chunk: &[u8]) -> Extent {
    Extent {
        lba: u64le(chunk, 0),
        nblocks: u32le(chunk, 8),
        hole: u32le(chunk, 12) & 1 != 0,
    }
}

/// Fill the 64-byte metadata header and stamp the CRC over bytes 8..4096 —
/// everything except the magic and the CRC itself.
fn meta_write_header(raw: &mut [u8; BLOCK], mtype: u32, generation: u64, self_block: u64) {
    raw[..4].copy_from_slice(&META_MAGIC);
    put_u32le(raw, 8, mtype);
    put_u32le(raw, 12, 0);
    put_u64le(raw, 16, generation);
    put_u64le(raw, 24, self_block);
    raw[32..META_HDR].fill(0);
    let crc = crc32(&raw[8..BLOCK]);
    put_u32le(raw, 4, crc);
}

/// Validate a metadata block's header; returns its type. Order: magic, known
/// type, CRC, self block, generation — the cheap filters run before the CRC.
fn meta_check(raw: &[u8; BLOCK], block: u64, sb_generation: u64) -> Result<u32, K1fsErr> {
    if raw[..4] != META_MAGIC {
        return Err(K1fsErr::BadMeta(block));
    }
    let mtype = u32le(raw, 8);
    if !(T_INODE..=T_EXT).contains(&mtype) {
        return Err(K1fsErr::BadMeta(block));
    }
    if crc32(&raw[8..BLOCK]) != u32le(raw, 4) {
        return Err(K1fsErr::MetaCrc(block));
    }
    if u64le(raw, 24) != block {
        return Err(K1fsErr::MetaSelf(block));
    }
    if u64le(raw, 16) > sb_generation {
        return Err(K1fsErr::MetaFuture(block));
    }
    Ok(mtype)
}

fn inode_decode(raw: &[u8; BLOCK], block: u64) -> Result<Inode, K1fsErr> {
    let mode = u16le(raw, INODE_BODY);
    if mode & 0o170000 != MODE_REG && mode & 0o170000 != MODE_DIR {
        return Err(K1fsErr::Corrupt(block));
    }
    let size = u64le(raw, INODE_BODY + 8);
    let is_dir = mode & 0o170000 == MODE_DIR;
    let mut inode = Inode {
        mode,
        size,
        inline: Vec::new(),
        extents: Vec::new(),
        ext_overflow: ROOT_NONE,
        dir_first: ROOT_NONE,
        dir_pages: 0,
    };
    if is_dir {
        inode.dir_pages = u64le(raw, INODE_BODY + 16);
        inode.dir_first = u64le(raw, INODE_BODY + 24);
        if inode.dir_first == ROOT_NONE && inode.dir_pages != 0 {
            return Err(K1fsErr::Corrupt(block));
        }
        return Ok(inode);
    }
    let n_extents = u64le(raw, INODE_BODY + 16);
    let inline_len = u32le(raw, INODE_BODY + 24) as usize;
    inode.ext_overflow = u64le(raw, INODE_BODY + 32);
    if inline_len > 0 {
        if inline_len > INLINE_MAX || u32le(raw, INODE_BODY + 4) & 1 == 0 {
            return Err(K1fsErr::Corrupt(block));
        }
        inode.size = inline_len as u64;
        inode.inline = raw[INODE_BODY + 40..INODE_BODY + 40 + inline_len].to_vec();
        return Ok(inode);
    }
    if n_extents > INODE_EXTENTS as u64 {
        return Err(K1fsErr::Corrupt(block));
    }
    for i in 0..n_extents as usize {
        inode
            .extents
            .push(read_extent(&raw[INODE_BODY + 40 + i * 16..]));
    }
    if size > inode.extents.iter().map(|e| e.nblocks as u64).sum::<u64>() * BLOCK as u64 {
        return Err(K1fsErr::Corrupt(block));
    }
    Ok(inode)
}

fn ext_decode(
    raw: &[u8; BLOCK],
    block: u64,
    total_blocks: u64,
) -> Result<(u64, Vec<Extent>), K1fsErr> {
    let next = u64le(raw, META_HDR);
    let count = u32le(raw, META_HDR + 8) as usize;
    if count > EXT_EXTENTS {
        return Err(K1fsErr::Corrupt(block));
    }
    let mut extents = Vec::with_capacity(count);
    for i in 0..count {
        let extent = read_extent(&raw[META_HDR + 16 + i * 16..]);
        if extent.nblocks == 0 || (!extent.hole && extent.lba < FIRST_DATA_BLOCK) {
            return Err(K1fsErr::Corrupt(block));
        }
        if !extent.hole && extent.lba + extent.nblocks as u64 > total_blocks {
            return Err(K1fsErr::Corrupt(block));
        }
        extents.push(extent);
    }
    if next != ROOT_NONE && (next < FIRST_DATA_BLOCK || next >= total_blocks) {
        return Err(K1fsErr::Corrupt(block));
    }
    Ok((next, extents))
}

fn dirpage_decode(raw: &[u8; BLOCK], block: u64, total_blocks: u64) -> Result<DirPage, K1fsErr> {
    let next = u64le(raw, META_HDR);
    let used_end = u32le(raw, META_HDR + 16) as usize;
    if used_end < DIR_BODY || used_end > BLOCK {
        return Err(K1fsErr::Corrupt(block));
    }
    if next != ROOT_NONE && (next < FIRST_DATA_BLOCK || next >= total_blocks) {
        return Err(K1fsErr::Corrupt(block));
    }
    let mut page = DirPage {
        next,
        entries: Vec::new(),
    };
    let mut off = DIR_BODY;
    let mut prev: Option<Vec<u8>> = None;
    while off < used_end {
        if off + 16 > used_end {
            return Err(K1fsErr::Corrupt(block));
        }
        let name_len = u16le(raw, off) as usize;
        let rec_len = u16le(raw, off + 2) as usize;
        let kind = raw[off + 4];
        let dead = raw[off + 5] & 1 != 0;
        let target = u64le(raw, off + 8);
        if rec_len < 16 || rec_len % 8 != 0 || off + rec_len > used_end {
            return Err(K1fsErr::Corrupt(block));
        }
        if name_len == 0 || name_len > NAME_MAX || 16 + name_len > rec_len {
            return Err(K1fsErr::Corrupt(block));
        }
        if kind != 1 && kind != 2 {
            return Err(K1fsErr::Corrupt(block));
        }
        if !dead {
            if target < FIRST_DATA_BLOCK || target >= total_blocks {
                return Err(K1fsErr::Corrupt(block));
            }
            let name = raw[off + 16..off + 16 + name_len].to_vec();
            if &name == b"." || &name == b".." || name.contains(&b'/') {
                return Err(K1fsErr::Corrupt(block));
            }
            if let Some(prev) = &prev {
                if prev.as_slice() > name.as_slice() {
                    return Err(K1fsErr::Corrupt(block)); // entries must be sorted
                }
            }
            prev = Some(name.clone());
            page.entries.push(DirEntry { name, kind, target });
        }
        off += rec_len;
    }
    Ok(page)
}

impl K1fs {
    /// Read and validate an inode block.
    pub fn read_inode(&self, dev: &mut impl BlockDevice, block: u64) -> Result<Inode, K1fsErr> {
        let mut raw = [0u8; BLOCK];
        read_block(dev, block, &mut raw)?;
        if meta_check(&raw, block, self.sb.generation)? != T_INODE {
            return Err(K1fsErr::BadMeta(block));
        }
        inode_decode(&raw, block)
    }

    /// Resolve a path to its inode. Absolute paths only, `/`-separated,
    /// `.` and `..` are not stored and rejected here (the service synthesizes
    /// them for listings).
    pub fn lookup(&self, dev: &mut impl BlockDevice, path: &str) -> Result<Inode, K1fsErr> {
        let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        if components.is_empty() {
            if self.sb.root_block == ROOT_NONE {
                return Err(K1fsErr::NoEnt);
            }
            return self.read_inode(dev, self.sb.root_block);
        }
        let mut current = self.sb.root_block;
        for component in &components {
            if current == ROOT_NONE {
                return Err(K1fsErr::NoEnt);
            }
            let inode = self.read_inode(dev, current)?;
            if !inode.is_dir() {
                return Err(K1fsErr::NotDir);
            }
            if inode.dir_first == ROOT_NONE {
                return Err(K1fsErr::NoEnt);
            }
            let page = self.read_dirpage(dev, inode.dir_first)?;
            let found = page
                .entries
                .iter()
                .find(|entry| entry.name.as_slice() == component.as_bytes());
            let Some(entry) = found else {
                return Err(K1fsErr::NoEnt);
            };
            // Only the final component may resolve to a file; deeper
            // components must be directories, and the walk continues at the
            // entry's inode block.
            current = entry.target;
        }
        let inode = self.read_inode(dev, current)?;
        if components.len() > 1 && !inode.is_dir() {
            return Err(K1fsErr::NotDir);
        }
        Ok(inode)
    }

    /// Read a directory's page chain into entries.
    pub fn list_dir(
        &self,
        dev: &mut impl BlockDevice,
        inode: &Inode,
    ) -> Result<Vec<DirEntry>, K1fsErr> {
        if !inode.is_dir() {
            return Err(K1fsErr::NotDir);
        }
        let mut entries = Vec::new();
        let mut next = inode.dir_first;
        let mut pages = 0u64;
        while next != ROOT_NONE {
            let page = self.read_dirpage(dev, next)?;
            entries.extend(page.entries);
            next = page.next;
            pages += 1;
            if pages > inode.dir_pages.max(1) {
                return Err(K1fsErr::Corrupt(inode.dir_first));
            }
        }
        Ok(entries)
    }

    fn read_dirpage(&self, dev: &mut impl BlockDevice, block: u64) -> Result<DirPage, K1fsErr> {
        let mut raw = [0u8; BLOCK];
        read_block(dev, block, &mut raw)?;
        if meta_check(&raw, block, self.sb.generation)? != T_DIRPAGE {
            return Err(K1fsErr::BadMeta(block));
        }
        dirpage_decode(&raw, block, self.sb.total_blocks)
    }

    /// Read up to `out.len()` bytes of a regular file at `offset`. Inline
    /// files copy from the inode; extent files walk the extent list, loading
    /// the EXT chain when the inode's own array is not the whole story.
    pub fn read_file(
        &self,
        dev: &mut impl BlockDevice,
        inode: &Inode,
        offset: u64,
        out: &mut [u8],
    ) -> Result<usize, K1fsErr> {
        if inode.is_dir() {
            return Err(K1fsErr::NotDir);
        }
        if offset >= inode.size || out.is_empty() {
            return Ok(0);
        }
        let wanted = ((inode.size - offset) as usize).min(out.len());
        if !inode.inline.is_empty() {
            out[..wanted].copy_from_slice(&inode.inline[offset as usize..offset as usize + wanted]);
            return Ok(wanted);
        }
        let extents = self.all_extents(dev, inode)?;
        let mut done = 0usize;
        let mut block_index = offset / BLOCK as u64;
        let mut skip = (offset % BLOCK as u64) as usize;
        for extent in &extents {
            let extent_blocks = extent.nblocks as u64;
            if block_index >= extent_blocks {
                block_index -= extent_blocks;
                continue;
            }
            for b in block_index..extent_blocks {
                if done >= wanted {
                    return Ok(done);
                }
                let take = (BLOCK - skip).min(wanted - done);
                if extent.hole {
                    out[done..done + take].fill(0);
                } else {
                    let mut raw = [0u8; BLOCK];
                    read_block(dev, extent.lba + b, &mut raw)?;
                    out[done..done + take].copy_from_slice(&raw[skip..skip + take]);
                }
                done += take;
                skip = 0;
            }
            block_index = 0;
        }
        Ok(done)
    }

    /// The full extent list: the inode's own array plus the EXT chain.
    fn all_extents(
        &self,
        dev: &mut impl BlockDevice,
        inode: &Inode,
    ) -> Result<Vec<Extent>, K1fsErr> {
        let mut extents = inode.extents.clone();
        let mut next = inode.ext_overflow;
        let mut chain = 0usize;
        while next != ROOT_NONE {
            let mut raw = [0u8; BLOCK];
            read_block(dev, next, &mut raw)?;
            if meta_check(&raw, next, self.sb.generation)? != T_EXT {
                return Err(K1fsErr::BadMeta(next));
            }
            let (following, more) = ext_decode(&raw, next, self.sb.total_blocks)?;
            extents.extend(more);
            next = following;
            chain += 1;
            if chain > 64 {
                return Err(K1fsErr::Corrupt(inode.ext_overflow));
            }
        }
        Ok(extents)
    }
}
