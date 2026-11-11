//! FAT12/16/32 reader and the FAT16 mutation primitives used by K1K's disk.
//!
//! # Mutation crash semantics (FAT16 only)
//!
//! FAT has no transactions, so every operation orders its writes so that a
//! power failure leaks space at worst and never publishes a half-done name:
//!
//! - **create**: the cluster chain is allocated and linked, the payload is
//!   written, and only then is the directory entry published. A crash before
//!   the publish leaks unreachable clusters; it cannot show a file whose data
//!   was never written.
//! - **replace**: the new payload lands in fresh clusters first; the existing
//!   directory entry is then rewritten in one sector write to point at them,
//!   and the old chain is released afterwards. A crash leaves either the old
//!   or the new content visible, never a mix; the orphaned chain leaks.
//! - **mkdir**: the fresh cluster gets its `.`/`..` entries before the parent
//!   entry names it. A crash leaks a directory cluster nobody points at.
//! - **unlink**: the directory entry is marked deleted (0xE5) *before* the
//!   cluster chain is freed. A crash in between leaks the chain; it can never
//!   leave a live entry pointing at clusters the allocator has handed out
//!   again. Freeing is best-effort: the name is already gone when a free
//!   fails.
//! - **rename**: the 11 name bytes are rewritten inside one directory sector.
//!   A crash leaves the old or the new name; the file's clusters are
//!   untouched. Only same-directory renames exist; moving between directories
//!   is a create plus an unlink.
//!
//! Two standing caveats: an open file handle keeps its directory-entry
//! snapshot, so replacing or unlinking under a reader is not synchronized,
//! and a directory has no rmdir yet.

pub trait BlockDevice {
    /// Read `count` 512-byte sectors starting at `lba` into `out`.
    fn read(&mut self, lba: u64, count: usize, out: &mut [u8]) -> Result<(), ()>;
    /// Write count 512-byte sectors from data starting at lba.
    ///
    /// FAT16 create uses this for its mirrored table, payload and directory
    /// entry writes. Other FAT variants remain read-only for now.
    fn write(&mut self, lba: u64, count: usize, data: &[u8]) -> Result<(), ()>;
    /// Make everything written so far durable: the barrier K1FS commits
    /// publish through. Devices without a volatile write cache implement it
    /// as a no-op.
    fn flush(&mut self) -> Result<(), ()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatKind {
    Fat12,
    Fat16,
    Fat32,
}

#[derive(Debug, Clone, Copy)]
pub struct DirEntry {
    pub name: [u8; 11],
    pub attr: u8,
    pub cluster: u32,
    pub size: u32,
}

impl DirEntry {
    pub fn is_dir(&self) -> bool {
        self.attr & 0x10 != 0
    }
}

/// Why a mutation failed, in the terms the file protocol reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatErr {
    Io,
    Exists,
    NoEnt,
    IsDir,
    Inval,
    NoSpc,
}

/// Where one 32-byte directory entry lives on disk.
#[derive(Debug, Clone, Copy)]
struct DirSlot {
    lba: u64,
    off: usize,
}

/// What must be published right after an entry that takes the directory's
/// end-of-directory (0x00) slot.
#[derive(Debug, Clone, Copy)]
enum Term {
    /// A later 0x00 entry already terminates the directory (the slot was a
    /// deleted 0xE5 entry, or a fresh cluster provides the marker).
    None,
    /// Zero the 32 bytes immediately after the entry: it consumed the old
    /// end-of-directory slot.
    ZeroNext,
    /// The entry takes the last slot of the chain's last cluster; append a
    /// fresh cluster whose zeroed body becomes the new terminator.
    NewCluster(u32),
}

/// A free directory slot found by [`Fat::find_slot`].
#[derive(Debug, Clone, Copy)]
enum FreeSlot {
    Slot {
        lba: u64,
        off: usize,
        term: Term,
    },
    /// The directory has no free slot; append a cluster after this one and put
    /// the entry in its first slot.
    AppendCluster {
        last_cluster: u32,
    },
}

/// Parse one 32-byte directory entry.
fn entry_at(chunk: &[u8]) -> DirEntry {
    let mut name = [0u8; 11];
    name.copy_from_slice(&chunk[..11]);
    DirEntry {
        name,
        attr: chunk[11],
        cluster: (u16_at(chunk, 20) as u32) << 16 | u16_at(chunk, 26) as u32,
        size: u32_at(chunk, 28),
    }
}

pub struct Fat {
    pub kind: FatKind,
    bytes_per_sector: u32,
    sectors_per_cluster: u32,
    fat_start: u32,
    fat_count: u32,
    fat_sectors: u32,
    root_start: u32,
    root_entries: u32,
    data_start: u32,
    root_cluster: u32,
    pub total_clusters: u32,
    pub label: [u8; 11],
    sector: [u8; 512],
    cached: u64,
}

fn u16_at(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

impl Fat {
    pub fn mount(dev: &mut dyn BlockDevice) -> Result<Self, &'static str> {
        let mut bs = [0u8; 512];
        dev.read(0, 1, &mut bs)
            .map_err(|_| "cannot read boot sector")?;
        if bs[510] != 0x55 || bs[511] != 0xAA {
            return Err("no boot signature");
        }
        let bytes_per_sector = u16_at(&bs, 11) as u32;
        let sectors_per_cluster = bs[13] as u32;
        let reserved = u16_at(&bs, 14) as u32;
        let fats = bs[16] as u32;
        let root_entries = u16_at(&bs, 17) as u32;
        let total16 = u16_at(&bs, 19) as u32;
        let fat_size16 = u16_at(&bs, 22) as u32;
        let total32 = u32_at(&bs, 32);
        let fat_size32 = u32_at(&bs, 36);
        if bytes_per_sector != 512 || sectors_per_cluster == 0 || fats == 0 {
            return Err("unsupported geometry");
        }
        let total = if total16 != 0 { total16 } else { total32 };
        let fat_size = if fat_size16 != 0 {
            fat_size16
        } else {
            fat_size32
        };
        let root_sectors = (root_entries * 32).div_ceil(bytes_per_sector);
        let fat_start = reserved;
        let root_start = fat_start + fats * fat_size;
        let data_start = root_start + root_sectors;
        let total_clusters = (total - data_start) / sectors_per_cluster;
        let kind = if fat_size16 == 0 {
            FatKind::Fat32
        } else if total_clusters < 4085 {
            FatKind::Fat12
        } else {
            FatKind::Fat16
        };
        let mut label = [b' '; 11];
        let label_off = if kind == FatKind::Fat32 { 71 } else { 43 };
        label.copy_from_slice(&bs[label_off..label_off + 11]);
        Ok(Self {
            kind,
            bytes_per_sector,
            sectors_per_cluster,
            fat_start,
            fat_count: fats,
            fat_sectors: fat_size,
            root_start,
            root_entries,
            data_start,
            root_cluster: if kind == FatKind::Fat32 {
                u32_at(&bs, 44)
            } else {
                0
            },
            total_clusters,
            label,
            sector: [0; 512],
            cached: u64::MAX,
        })
    }

    pub fn cluster_bytes(&self) -> usize {
        (self.sectors_per_cluster * self.bytes_per_sector) as usize
    }

    fn cluster_lba(&self, cluster: u32) -> u64 {
        (self.data_start + (cluster - 2) * self.sectors_per_cluster) as u64
    }

    fn fat_sector(&mut self, dev: &mut dyn BlockDevice, lba: u64) -> Result<(), ()> {
        if self.cached != lba {
            let mut tmp = [0u8; 512];
            dev.read(lba, 1, &mut tmp)?;
            self.sector = tmp;
            self.cached = lba;
        }
        Ok(())
    }

    /// Replace one sector and keep the single-sector FAT cache coherent.
    fn write_sector(
        &mut self,
        dev: &mut dyn BlockDevice,
        lba: u64,
        data: &[u8; 512],
    ) -> Result<(), ()> {
        dev.write(lba, 1, data)?;
        if self.cached == lba {
            self.sector.copy_from_slice(data);
        }
        Ok(())
    }

    fn fat16_value(&mut self, dev: &mut dyn BlockDevice, cluster: u32) -> Result<u16, ()> {
        if self.kind != FatKind::Fat16 || cluster > self.total_clusters + 1 {
            return Err(());
        }
        let off = cluster as usize * 2;
        let lba = self.fat_start as u64 + (off / 512) as u64;
        self.fat_sector(dev, lba)?;
        Ok(u16_at(&self.sector, off % 512))
    }

    /// Store a FAT16 link in every on-disk mirror. FAT is updated before a
    /// directory entry points at the cluster, so an interrupted create leaks
    /// at worst; it must never turn a live file into a dangling entry.
    fn set_fat16_value(
        &mut self,
        dev: &mut dyn BlockDevice,
        cluster: u32,
        value: u16,
    ) -> Result<(), ()> {
        if self.kind != FatKind::Fat16 || cluster > self.total_clusters + 1 {
            return Err(());
        }
        let off = cluster as usize * 2;
        let sector_off = off % 512;
        for copy in 0..self.fat_count {
            let lba =
                self.fat_start as u64 + copy as u64 * self.fat_sectors as u64 + (off / 512) as u64;
            let mut sector = [0u8; 512];
            dev.read(lba, 1, &mut sector)?;
            sector[sector_off..sector_off + 2].copy_from_slice(&value.to_le_bytes());
            self.write_sector(dev, lba, &sector)?;
        }
        Ok(())
    }

    /// Allocate one zeroed cluster and mark it end-of-chain. This is FAT16
    /// only for now: K1K's mformat image is FAT16, while FAT12/32 mutation is
    /// deliberately rejected instead of risking a differently encoded table.
    pub fn alloc_cluster(&mut self, dev: &mut dyn BlockDevice) -> Result<u32, ()> {
        if self.kind != FatKind::Fat16 {
            return Err(());
        }
        let cluster = (2..=self.total_clusters + 1)
            .find(|&c| self.fat16_value(dev, c).ok() == Some(0))
            .ok_or(())?;
        self.set_fat16_value(dev, cluster, 0xFFFF)?;
        let zero = [0u8; 512];
        let lba = self.cluster_lba(cluster);
        for sector in 0..self.sectors_per_cluster {
            self.write_sector(dev, lba + sector as u64, &zero)?;
        }
        Ok(cluster)
    }

    /// Return a just-allocated FAT16 cluster to the free list. Higher layers
    /// call this only after a failed create has no directory entry referring
    /// to it; removal of a live file needs a full chain operation.
    #[allow(dead_code)] // exercised by the host regression; future unlink uses it too
    pub fn free_cluster(&mut self, dev: &mut dyn BlockDevice, cluster: u32) -> Result<(), ()> {
        if self.fat16_value(dev, cluster)? == 0 {
            return Err(());
        }
        self.set_fat16_value(dev, cluster, 0)
    }

    fn write_cluster(
        &mut self,
        dev: &mut dyn BlockDevice,
        cluster: u32,
        data: &[u8],
    ) -> Result<(), ()> {
        let cb = self.cluster_bytes();
        if data.len() > cb {
            return Err(());
        }
        let mut sector = [0u8; 512];
        let lba = self.cluster_lba(cluster);
        for i in 0..self.sectors_per_cluster as usize {
            let start = i * 512;
            let end = (start + 512).min(data.len());
            sector.fill(0);
            if start < end {
                sector[..end - start].copy_from_slice(&data[start..end]);
            }
            self.write_sector(dev, lba + i as u64, &sector)?;
        }
        Ok(())
    }

    /// Create one regular file in the fixed FAT16 root directory.
    ///
    /// Kept for the bootstrap path; the general operation is [`Fat::create_file`].
    pub fn create_root_file(
        &mut self,
        dev: &mut dyn BlockDevice,
        name: &str,
        data: &[u8],
    ) -> Result<DirEntry, ()> {
        self.create_file(dev, 0, name, data, false).map_err(|_| ())
    }

    /// Find `name83` in a directory, with the slot it lives in. Unlike
    /// [`Fat::lookup`] this also reports where the entry is, which rename,
    /// replace and unlink need. The walk stops at the first 0x00 marker:
    /// stale bytes after it are not entries.
    fn find_entry(
        &mut self,
        dev: &mut dyn BlockDevice,
        dir_cluster: u32,
        name83: &[u8; 11],
    ) -> Result<Option<(DirEntry, DirSlot)>, FatErr> {
        let mut sector = [0u8; 512];
        if dir_cluster == 0 {
            if self.kind == FatKind::Fat32 {
                return Err(FatErr::Inval);
            }
            let sectors = (self.root_entries * 32).div_ceil(512);
            for n in 0..sectors {
                let lba = self.root_start as u64 + n as u64;
                dev.read(lba, 1, &mut sector).map_err(|_| FatErr::Io)?;
                for off in (0..512).step_by(32) {
                    if sector[off] == 0 {
                        return Ok(None);
                    }
                    if sector[off] == 0xE5 || sector[off + 11] & 0x08 != 0 {
                        continue;
                    }
                    if &sector[off..off + 11] == name83 {
                        return Ok(Some((
                            entry_at(&sector[off..off + 32]),
                            DirSlot { lba, off },
                        )));
                    }
                }
            }
            return Ok(None);
        }
        let mut cluster = dir_cluster;
        let mut guard = 0;
        loop {
            let base = self.cluster_lba(cluster);
            for s in 0..self.sectors_per_cluster {
                let lba = base + s as u64;
                dev.read(lba, 1, &mut sector).map_err(|_| FatErr::Io)?;
                for off in (0..512).step_by(32) {
                    if sector[off] == 0 {
                        return Ok(None);
                    }
                    if sector[off] == 0xE5 || sector[off + 11] & 0x08 != 0 {
                        continue;
                    }
                    if &sector[off..off + 11] == name83 {
                        return Ok(Some((
                            entry_at(&sector[off..off + 32]),
                            DirSlot { lba, off },
                        )));
                    }
                }
            }
            match self.next_cluster(dev, cluster).map_err(|_| FatErr::Io)? {
                Some(c) => {
                    guard += 1;
                    if guard > self.total_clusters {
                        return Err(FatErr::Io); // a cyclic FAT is not our doing
                    }
                    cluster = c;
                }
                None => return Ok(None),
            }
        }
    }

    /// Find the first slot a new entry may take. A deleted (0xE5) slot wins
    /// over the end-of-directory (0x00) slot; consuming the latter means a
    /// new terminator must be published right after the entry.
    fn find_slot(
        &mut self,
        dev: &mut dyn BlockDevice,
        dir_cluster: u32,
    ) -> Result<FreeSlot, FatErr> {
        let mut sector = [0u8; 512];
        if dir_cluster == 0 {
            if self.kind == FatKind::Fat32 {
                return Err(FatErr::Inval);
            }
            let sectors = (self.root_entries * 32).div_ceil(512);
            for n in 0..sectors {
                let lba = self.root_start as u64 + n as u64;
                dev.read(lba, 1, &mut sector).map_err(|_| FatErr::Io)?;
                for off in (0..512).step_by(32) {
                    if sector[off] == 0xE5 {
                        return Ok(FreeSlot::Slot {
                            lba,
                            off,
                            term: Term::None,
                        });
                    }
                    if sector[off] == 0 {
                        // Everything after the marker is unused, so the marker
                        // itself is the last place an entry can go: only take
                        // it when a slot remains for the new terminator.
                        if off + 32 < 512 || n + 1 < sectors {
                            return Ok(FreeSlot::Slot {
                                lba,
                                off,
                                term: Term::ZeroNext,
                            });
                        }
                        return Err(FatErr::NoSpc);
                    }
                }
            }
            return Err(FatErr::NoSpc);
        }
        let mut cluster = dir_cluster;
        let mut guard = 0;
        loop {
            let base = self.cluster_lba(cluster);
            for s in 0..self.sectors_per_cluster {
                let lba = base + s as u64;
                dev.read(lba, 1, &mut sector).map_err(|_| FatErr::Io)?;
                for off in (0..512).step_by(32) {
                    if sector[off] == 0xE5 {
                        return Ok(FreeSlot::Slot {
                            lba,
                            off,
                            term: Term::None,
                        });
                    }
                    if sector[off] == 0 {
                        if off + 32 < 512 || s + 1 < self.sectors_per_cluster {
                            return Ok(FreeSlot::Slot {
                                lba,
                                off,
                                term: Term::ZeroNext,
                            });
                        }
                        // The marker is the last slot of this cluster: the
                        // next cluster holds the terminator, whether it
                        // already exists or has to be appended.
                        return match self.next_cluster(dev, cluster).map_err(|_| FatErr::Io)? {
                            Some(_) => Ok(FreeSlot::Slot {
                                lba,
                                off,
                                term: Term::ZeroNext,
                            }),
                            None => Ok(FreeSlot::Slot {
                                lba,
                                off,
                                term: Term::NewCluster(cluster),
                            }),
                        };
                    }
                }
            }
            match self.next_cluster(dev, cluster).map_err(|_| FatErr::Io)? {
                Some(c) => {
                    guard += 1;
                    if guard > self.total_clusters {
                        return Err(FatErr::Io);
                    }
                    cluster = c;
                }
                None => {
                    return Ok(FreeSlot::AppendCluster {
                        last_cluster: cluster,
                    });
                }
            }
        }
    }

    /// Write the 32 bytes of `entry` at `slot`.
    fn write_dir_entry(
        &mut self,
        dev: &mut dyn BlockDevice,
        slot: DirSlot,
        entry: &DirEntry,
    ) -> Result<(), FatErr> {
        let mut sector = [0u8; 512];
        dev.read(slot.lba, 1, &mut sector).map_err(|_| FatErr::Io)?;
        sector[slot.off..slot.off + 11].copy_from_slice(&entry.name);
        sector[slot.off + 11] = entry.attr;
        sector[slot.off + 12..slot.off + 32].fill(0);
        sector[slot.off + 20..slot.off + 22]
            .copy_from_slice(&((entry.cluster >> 16) as u16).to_le_bytes());
        sector[slot.off + 26..slot.off + 28].copy_from_slice(&(entry.cluster as u16).to_le_bytes());
        sector[slot.off + 28..slot.off + 32].copy_from_slice(&entry.size.to_le_bytes());
        self.write_sector(dev, slot.lba, &sector)
            .map_err(|_| FatErr::Io)
    }

    /// Zero the 32 bytes right after a published entry: it consumed the old
    /// end-of-directory slot, and the marker has to move one slot forward.
    fn zero_next_slot(&mut self, dev: &mut dyn BlockDevice, slot: DirSlot) -> Result<(), FatErr> {
        let (lba, off) = if slot.off + 32 < 512 {
            (slot.lba, slot.off + 32)
        } else {
            (slot.lba + 1, 0)
        };
        let mut sector = [0u8; 512];
        dev.read(lba, 1, &mut sector).map_err(|_| FatErr::Io)?;
        sector[off..off + 32].fill(0);
        self.write_sector(dev, lba, &sector).map_err(|_| FatErr::Io)
    }

    /// Publish `entry` into a free slot of `dir_cluster`, extending the
    /// directory when it is full and moving the end-of-directory marker when
    /// the entry takes its place. FAT-chain changes happen before the entry
    /// becomes visible, so a crash can at worst leave an empty appended
    /// cluster or a missing marker write — never a half-published name.
    fn publish_entry(
        &mut self,
        dev: &mut dyn BlockDevice,
        dir_cluster: u32,
        entry: &DirEntry,
    ) -> Result<(), FatErr> {
        match self.find_slot(dev, dir_cluster)? {
            FreeSlot::Slot { lba, off, term } => {
                if let Term::NewCluster(last) = term {
                    let c = self.alloc_cluster(dev).map_err(|_| FatErr::NoSpc)?;
                    self.set_fat16_value(dev, last, c as u16)
                        .map_err(|_| FatErr::Io)?;
                }
                let slot = DirSlot { lba, off };
                self.write_dir_entry(dev, slot, entry)?;
                if matches!(term, Term::ZeroNext) {
                    self.zero_next_slot(dev, slot)?;
                }
                Ok(())
            }
            FreeSlot::AppendCluster { last_cluster } => {
                let c = self.alloc_cluster(dev).map_err(|_| FatErr::NoSpc)?;
                self.set_fat16_value(dev, last_cluster, c as u16)
                    .map_err(|_| FatErr::Io)?;
                // The fresh cluster is zeroed: the entry goes into its first
                // slot and the zeroed rest is the new terminator.
                self.write_dir_entry(
                    dev,
                    DirSlot {
                        lba: self.cluster_lba(c),
                        off: 0,
                    },
                    entry,
                )
            }
        }
    }

    /// Allocate `count` clusters, link them into a chain and fill them with
    /// `data`. Any failure releases the partial chain: nothing points at it
    /// yet, so the cleanup cannot race a reader.
    fn write_chain(
        &mut self,
        dev: &mut dyn BlockDevice,
        data: &[u8],
    ) -> Result<alloc::vec::Vec<u32>, FatErr> {
        let cb = self.cluster_bytes();
        let count = data.len().div_ceil(cb);
        let mut clusters = alloc::vec::Vec::new();
        for _ in 0..count {
            match self.alloc_cluster(dev) {
                Ok(c) => clusters.push(c),
                Err(()) => {
                    for c in clusters {
                        let _ = self.set_fat16_value(dev, c, 0);
                    }
                    return Err(FatErr::NoSpc);
                }
            }
        }
        for pair in clusters.windows(2) {
            if self.set_fat16_value(dev, pair[0], pair[1] as u16).is_err() {
                for c in clusters {
                    let _ = self.set_fat16_value(dev, c, 0);
                }
                return Err(FatErr::Io);
            }
        }
        for (i, &cluster) in clusters.iter().enumerate() {
            let start = i * cb;
            let end = (start + cb).min(data.len());
            if self.write_cluster(dev, cluster, &data[start..end]).is_err() {
                for &c in &clusters {
                    let _ = self.set_fat16_value(dev, c, 0);
                }
                return Err(FatErr::Io);
            }
        }
        Ok(clusters)
    }

    /// Release a whole cluster chain. Best-effort by design: the callers run
    /// after the name is already gone, so a failed free leaks space rather
    /// than undoing the operation.
    fn free_chain_best_effort(&mut self, dev: &mut dyn BlockDevice, first: u32) {
        let mut cluster = first;
        let mut guard = 0;
        while cluster >= 2 {
            let Ok(next) = self.next_cluster(dev, cluster) else {
                return;
            };
            let _ = self.set_fat16_value(dev, cluster, 0);
            cluster = next.unwrap_or(0);
            guard += 1;
            if guard > self.total_clusters + 2 {
                return; // cyclic FAT: stop instead of looping forever
            }
        }
    }

    /// Create a regular file in `dir_cluster` (0 = the FAT16 root).
    ///
    /// With `replace` an existing regular file is swapped atomically — new
    /// payload in fresh clusters, then one sector write repoints the entry,
    /// then the old chain is released — so a crash shows the old or the new
    /// content, never a mix. Without it an existing name fails with
    /// [`FatErr::Exists`]. On any error before the publish the old file (or
    /// the free name) is left exactly as it was.
    pub fn create_file(
        &mut self,
        dev: &mut dyn BlockDevice,
        dir_cluster: u32,
        name: &str,
        data: &[u8],
        replace: bool,
    ) -> Result<DirEntry, FatErr> {
        if self.kind != FatKind::Fat16 || data.len() > u32::MAX as usize {
            return Err(FatErr::Inval);
        }
        let name83 = to_83(name).ok_or(FatErr::Inval)?;
        if let Some((old, slot)) = self.find_entry(dev, dir_cluster, &name83)? {
            if !replace {
                return Err(FatErr::Exists);
            }
            if old.is_dir() {
                return Err(FatErr::IsDir);
            }
            // The old file stays fully live until the single repointing write.
            let clusters = self.write_chain(dev, data)?;
            let new = DirEntry {
                name: old.name,
                attr: old.attr,
                cluster: clusters.first().copied().unwrap_or(0),
                size: data.len() as u32,
            };
            self.write_dir_entry(dev, slot, &new)?;
            self.free_chain_best_effort(dev, old.cluster);
            return Ok(new);
        }
        let clusters = self.write_chain(dev, data)?;
        let entry = DirEntry {
            name: name83,
            attr: 0x20,
            cluster: clusters.first().copied().unwrap_or(0),
            size: data.len() as u32,
        };
        // A failed publish leaks the fresh chain; it cannot leave a visible
        // half-created file, which is the ordering the module docs promise.
        self.publish_entry(dev, dir_cluster, &entry)?;
        Ok(entry)
    }

    /// Create one subdirectory in `dir_cluster` (0 = the FAT16 root). The
    /// fresh cluster gets its `.`/`..` entries before the parent entry names
    /// it, so a crash leaks at worst an unreachable directory cluster.
    pub fn mkdir(
        &mut self,
        dev: &mut dyn BlockDevice,
        dir_cluster: u32,
        name: &str,
    ) -> Result<DirEntry, FatErr> {
        if self.kind != FatKind::Fat16 {
            return Err(FatErr::Inval);
        }
        let name83 = to_83(name).ok_or(FatErr::Inval)?;
        if self.find_entry(dev, dir_cluster, &name83)?.is_some() {
            return Err(FatErr::Exists);
        }
        let cluster = self.alloc_cluster(dev).map_err(|_| FatErr::NoSpc)?;
        let mut sector = [0u8; 512];
        sector[0..11].fill(b' ');
        sector[0] = b'.';
        sector[11] = 0x10;
        sector[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
        sector[32..43].fill(b' ');
        sector[32] = b'.';
        sector[33] = b'.';
        sector[43] = 0x10;
        // ".." points at the parent cluster; 0 means the FAT16 root, which
        // has no cluster of its own.
        sector[58..60].copy_from_slice(&(dir_cluster as u16).to_le_bytes());
        self.write_sector(dev, self.cluster_lba(cluster), &sector)
            .map_err(|_| FatErr::Io)?;
        let entry = DirEntry {
            name: name83,
            attr: 0x10,
            cluster,
            size: 0,
        };
        self.publish_entry(dev, dir_cluster, &entry)?;
        Ok(entry)
    }

    /// Remove one regular file. Directories are refused: there is no rmdir
    /// yet, and recursively deleting a tree is a policy decision this layer
    /// does not make. The name is deleted before the chain is freed, so a
    /// crash leaks clusters but can never leave a live entry pointing at
    /// clusters the allocator has handed out again.
    pub fn unlink(
        &mut self,
        dev: &mut dyn BlockDevice,
        dir_cluster: u32,
        name: &str,
    ) -> Result<(), FatErr> {
        if self.kind != FatKind::Fat16 {
            return Err(FatErr::Inval);
        }
        let name83 = to_83(name).ok_or(FatErr::Inval)?;
        let Some((entry, slot)) = self.find_entry(dev, dir_cluster, &name83)? else {
            return Err(FatErr::NoEnt);
        };
        if entry.is_dir() {
            return Err(FatErr::IsDir);
        }
        let mut sector = [0u8; 512];
        dev.read(slot.lba, 1, &mut sector).map_err(|_| FatErr::Io)?;
        sector[slot.off] = 0xE5;
        self.write_sector(dev, slot.lba, &sector)
            .map_err(|_| FatErr::Io)?;
        self.free_chain_best_effort(dev, entry.cluster);
        Ok(())
    }

    /// Rename one entry within `dir_cluster`. The new name must not exist;
    /// moving between directories is deliberately not offered (create plus
    /// unlink covers it). The 11 name bytes are rewritten inside one
    /// directory sector, so a crash leaves the old or the new name and never
    /// touches the file's clusters.
    pub fn rename(
        &mut self,
        dev: &mut dyn BlockDevice,
        dir_cluster: u32,
        old_name: &str,
        new_name: &str,
    ) -> Result<(), FatErr> {
        if self.kind != FatKind::Fat16 {
            return Err(FatErr::Inval);
        }
        let old83 = to_83(old_name).ok_or(FatErr::Inval)?;
        let new83 = to_83(new_name).ok_or(FatErr::Inval)?;
        let Some((_entry, slot)) = self.find_entry(dev, dir_cluster, &old83)? else {
            return Err(FatErr::NoEnt);
        };
        if self.find_entry(dev, dir_cluster, &new83)?.is_some() {
            return Err(FatErr::Exists);
        }
        let mut sector = [0u8; 512];
        dev.read(slot.lba, 1, &mut sector).map_err(|_| FatErr::Io)?;
        sector[slot.off..slot.off + 11].copy_from_slice(&new83);
        self.write_sector(dev, slot.lba, &sector)
            .map_err(|_| FatErr::Io)
    }

    /// Next cluster in the chain, or None at end-of-chain.
    pub fn next_cluster(
        &mut self,
        dev: &mut dyn BlockDevice,
        cluster: u32,
    ) -> Result<Option<u32>, ()> {
        let fat_start = self.fat_start as u64;
        match self.kind {
            FatKind::Fat32 => {
                let off = cluster as u64 * 4;
                self.fat_sector(dev, fat_start + off / 512)?;
                let v = u32_at(&self.sector, (off % 512) as usize) & 0x0FFF_FFFF;
                Ok((v < 0x0FFF_FFF8).then_some(v))
            }
            FatKind::Fat16 => {
                let off = cluster as u64 * 2;
                self.fat_sector(dev, fat_start + off / 512)?;
                let v = u16_at(&self.sector, (off % 512) as usize) as u32;
                Ok((v < 0xFFF8).then_some(v))
            }
            FatKind::Fat12 => {
                let off = cluster as u64 * 3 / 2;
                self.fat_sector(dev, fat_start + off / 512)?;
                let lo = self.sector[(off % 512) as usize] as u32;
                // The second byte may live in the next sector.
                let hi = if (off % 512) as usize == 511 {
                    self.fat_sector(dev, fat_start + off / 512 + 1)?;
                    self.sector[0] as u32
                } else {
                    self.sector[(off % 512) as usize + 1] as u32
                };
                let raw = lo | hi << 8;
                let v = if cluster & 1 == 0 {
                    raw & 0xFFF
                } else {
                    raw >> 4
                };
                Ok((v < 0xFF8).then_some(v))
            }
        }
    }

    /// Iterate a directory's 32-byte entries, calling `f` for each real file
    /// or subdirectory. `dir_cluster == 0` means the (FAT12/16) root directory.
    pub fn read_dir(
        &mut self,
        dev: &mut dyn BlockDevice,
        dir_cluster: u32,
        buf: &mut [u8],
        mut f: impl FnMut(&DirEntry),
    ) -> Result<(), ()> {
        if dir_cluster == 0 && self.kind != FatKind::Fat32 {
            let sectors = (self.root_entries * 32).div_ceil(512) as usize;
            let mut lba = self.root_start as u64;
            let mut left = sectors;
            while left > 0 {
                let n = left.min(buf.len() / 512).min(16);
                dev.read(lba, n, &mut buf[..n * 512])?;
                if !scan_entries(&buf[..n * 512], &mut f) {
                    return Ok(());
                }
                lba += n as u64;
                left -= n;
            }
            return Ok(());
        }
        let mut cluster = if dir_cluster == 0 {
            self.root_cluster
        } else {
            dir_cluster
        };
        let cb = self.cluster_bytes();
        loop {
            self.read_cluster(dev, cluster, &mut buf[..cb])?;
            if !scan_entries(&buf[..cb], &mut f) {
                return Ok(());
            }
            match self.next_cluster(dev, cluster)? {
                Some(c) => cluster = c,
                None => return Ok(()),
            }
        }
    }

    pub fn read_cluster(
        &mut self,
        dev: &mut dyn BlockDevice,
        cluster: u32,
        out: &mut [u8],
    ) -> Result<(), ()> {
        let lba = self.cluster_lba(cluster);
        let sectors = self.sectors_per_cluster as usize;
        let mut done = 0;
        while done < sectors {
            let n = (sectors - done).min(16);
            dev.read(lba + done as u64, n, &mut out[done * 512..(done + n) * 512])?;
            done += n;
        }
        Ok(())
    }

    /// Copy a whole file into `out` (which must hold `entry.size` bytes).
    pub fn read_file(
        &mut self,
        dev: &mut dyn BlockDevice,
        entry: &DirEntry,
        scratch: &mut [u8],
        out: &mut [u8],
    ) -> Result<usize, ()> {
        let cb = self.cluster_bytes();
        let mut cluster = entry.cluster;
        let mut copied = 0usize;
        let size = entry.size as usize;
        while copied < size && cluster >= 2 {
            self.read_cluster(dev, cluster, &mut scratch[..cb])?;
            let n = (size - copied).min(cb);
            out[copied..copied + n].copy_from_slice(&scratch[..n]);
            copied += n;
            match self.next_cluster(dev, cluster)? {
                Some(c) => cluster = c,
                None => break,
            }
        }
        Ok(copied)
    }

    /// Copy part of a file: `out.len()` bytes starting at `offset`, read
    /// through `scratch` (which must hold at least one cluster). Returns how
    /// many bytes were produced — fewer than asked for at end of file.
    pub fn read_at(
        &mut self,
        dev: &mut dyn BlockDevice,
        entry: &DirEntry,
        offset: u64,
        scratch: &mut [u8],
        out: &mut [u8],
    ) -> Result<usize, ()> {
        let size = entry.size as usize;
        let start = offset as usize;
        if start >= size || out.is_empty() {
            return Ok(0);
        }
        let want = out.len().min(size - start);
        let cb = self.cluster_bytes();
        if scratch.len() < cb {
            return Err(());
        }
        // Walk to the cluster holding `offset`.
        let mut pos = start;
        let mut cluster = entry.cluster;
        while pos >= cb {
            pos -= cb;
            cluster = self.next_cluster(dev, cluster)?.ok_or(())?;
        }
        let mut done = 0usize;
        while done < want {
            self.read_cluster(dev, cluster, &mut scratch[..cb])?;
            let n = (want - done).min(cb - pos);
            out[done..done + n].copy_from_slice(&scratch[pos..pos + n]);
            done += n;
            pos = 0;
            if done < want {
                cluster = self.next_cluster(dev, cluster)?.ok_or(())?;
            }
        }
        Ok(done)
    }

    /// Find an entry by 8.3 name (case-insensitive, "NAME.EXT" form) in a
    /// directory. A name that does not fit 8.3 never matches rather than
    /// matching a truncated one.
    pub fn lookup(
        &mut self,
        dev: &mut dyn BlockDevice,
        dir_cluster: u32,
        name: &str,
        buf: &mut [u8],
    ) -> Result<Option<DirEntry>, ()> {
        let Some(want) = to_83(name) else {
            return Ok(None);
        };
        let mut found = None;
        self.read_dir(dev, dir_cluster, buf, |e| {
            if found.is_none() && e.name == want {
                found = Some(*e);
            }
        })?;
        Ok(found)
    }
}

/// Convert "name.ext" to its 11-byte 8.3 form, or `None` when it does not fit
/// or holds a byte FAT cannot store. Truncating instead would silently open a
/// different file than the caller asked for.
pub fn to_83(name: &str) -> Option<[u8; 11]> {
    let (base, ext) = match name.find('.') {
        Some(i) => (&name[..i], &name[i + 1..]),
        None => (name, ""),
    };
    if base.is_empty() || base.len() > 8 || ext.len() > 3 {
        return None;
    }
    let mut out = [b' '; 11];
    for (i, b) in base.bytes().enumerate() {
        if !valid_name_byte(b) {
            return None;
        }
        out[i] = b.to_ascii_uppercase();
    }
    for (i, b) in ext.bytes().enumerate() {
        if !valid_name_byte(b) {
            return None;
        }
        out[8 + i] = b.to_ascii_uppercase();
    }
    Some(out)
}

fn valid_name_byte(b: u8) -> bool {
    b.is_ascii_graphic()
        && !matches!(
            b,
            b' ' | b'"'
                | b'*'
                | b'+'
                | b','
                | b'.'
                | b'/'
                | b':'
                | b';'
                | b'<'
                | b'='
                | b'>'
                | b'?'
                | b'['
                | b'\\'
                | b']'
                | b'|'
        )
}

/// Returns false when the end-of-directory marker was reached.
fn scan_entries(buf: &[u8], f: &mut impl FnMut(&DirEntry)) -> bool {
    for chunk in buf.chunks_exact(32) {
        if chunk[0] == 0x00 {
            return false;
        }
        if chunk[0] == 0xE5 || chunk[11] & 0x08 != 0 {
            continue; // deleted, volume label or long-name entry
        }
        let mut name = [0u8; 11];
        name.copy_from_slice(&chunk[..11]);
        f(&DirEntry {
            name,
            attr: chunk[11],
            cluster: (u16_at(chunk, 20) as u32) << 16 | u16_at(chunk, 26) as u32,
            size: u32_at(chunk, 28),
        });
    }
    true
}
