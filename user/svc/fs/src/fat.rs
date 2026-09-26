//! Read-only FAT12/16/32 on top of a block reader.

pub trait BlockDevice {
    /// Read `count` 512-byte sectors starting at `lba` into `out`.
    fn read(&mut self, lba: u64, count: usize, out: &mut [u8]) -> Result<(), ()>;
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

pub struct Fat {
    pub kind: FatKind,
    bytes_per_sector: u32,
    sectors_per_cluster: u32,
    fat_start: u32,
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
    b.is_ascii_graphic() && b != b'.' && b != b' '
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
