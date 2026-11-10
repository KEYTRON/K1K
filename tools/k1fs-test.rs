// Host regression for the K1FS slice that tools/k1fs-test.rs builds out of
// user/svc/fs/src/k1fs.rs. The image is a scratch file created by Make; the
// test rewrites every state it needs, so order matters and is spelled out
// step by step.
extern crate alloc;

#[path = "../user/svc/fs/src/fat.rs"]
mod fat;
#[path = "../user/svc/fs/src/k1fs.rs"]
mod k1fs;

use std::env;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};

use k1fs::{BLOCK, K1fs, K1fsErr, ROOT_NONE, SECTORS_PER_BLOCK, Superblock};

struct Disk(std::fs::File);

impl fat::BlockDevice for Disk {
    fn read(&mut self, lba: u64, count: usize, out: &mut [u8]) -> Result<(), ()> {
        let bytes = count.checked_mul(512).ok_or(())?;
        if out.len() < bytes {
            return Err(());
        }
        self.0
            .seek(SeekFrom::Start(lba.checked_mul(512).ok_or(())?))
            .map_err(|_| ())?;
        self.0.read_exact(&mut out[..bytes]).map_err(|_| ())
    }

    fn write(&mut self, lba: u64, count: usize, data: &[u8]) -> Result<(), ()> {
        let bytes = count.checked_mul(512).ok_or(())?;
        if data.len() < bytes {
            return Err(());
        }
        self.0
            .seek(SeekFrom::Start(lba.checked_mul(512).ok_or(())?))
            .map_err(|_| ())?;
        self.0.write_all(&data[..bytes]).map_err(|_| ())?;
        self.0.flush().map_err(|_| ())
    }
}

impl Disk {
    fn read_block(&mut self, block: u64) -> [u8; BLOCK] {
        let mut buf = [0u8; BLOCK];
        fat::BlockDevice::read(
            self,
            block * SECTORS_PER_BLOCK,
            SECTORS_PER_BLOCK as usize,
            &mut buf,
        )
        .expect("read block");
        buf
    }

    fn write_block(&mut self, block: u64, buf: &[u8; BLOCK]) {
        fat::BlockDevice::write(
            self,
            block * SECTORS_PER_BLOCK,
            SECTORS_PER_BLOCK as usize,
            buf,
        )
        .expect("write block");
    }
}

fn expect_mount(disk: &mut Disk, blocks: u64, slot: u8, generation: u64) {
    let m = K1fs::mount(disk, blocks).expect("mount");
    assert_eq!(m.slot, slot, "mount must pick slot {slot}");
    assert_eq!(
        m.sb.generation, generation,
        "mount must see generation {generation}"
    );
    assert_eq!(m.sb.root_block, ROOT_NONE, "fresh volume has no root yet");
    assert_eq!(m.sb.total_blocks, blocks);
}

fn write_superblock(disk: &mut Disk, slot_block: u64, generation: u64, blocks: u64) {
    let sb = Superblock {
        generation,
        root_block: ROOT_NONE,
        total_blocks: blocks,
        log_block_size: BLOCK.trailing_zeros(),
    };
    let mut raw = [0u8; BLOCK];
    sb.encode(&mut raw);
    disk.write_block(slot_block, &raw);
}

fn garbage_block() -> [u8; BLOCK] {
    let mut g = [0u8; BLOCK];
    for (i, b) in g.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(167).wrapping_add(42);
    }
    g
}

fn main() {
    let image = env::args().nth(1).expect("usage: k1fs-test <disk.img>");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .expect("open disk image");
    let blocks = file.metadata().expect("stat").len() / BLOCK as u64;
    assert!(blocks >= k1fs::MIN_BLOCKS, "test image too small");
    let mut disk = Disk(file);

    // mkfs refuses a volume with no room for the two slots.
    assert_eq!(
        K1fs::mkfs(&mut disk, k1fs::MIN_BLOCKS - 1),
        Err(K1fsErr::Inval),
        "mkfs must reject a device below MIN_BLOCKS"
    );

    // 1. mkfs writes both slots at generation 1; mount picks A on the tie.
    K1fs::mkfs(&mut disk, blocks).expect("mkfs");
    expect_mount(&mut disk, blocks, b'A', 1);
    println!("k1fs: mkfs/mount: ok");

    // 2. A slot destroyed -> B carries the volume.
    disk.write_block(k1fs::SLOT_A_BLOCK, &garbage_block());
    expect_mount(&mut disk, blocks, b'B', 1);
    write_superblock(&mut disk, k1fs::SLOT_A_BLOCK, 1, blocks);
    println!("k1fs: corrupt A falls back to B: ok");

    // 3. Newer generation wins over a healthy older slot.
    write_superblock(&mut disk, k1fs::SLOT_B_BLOCK, 2, blocks);
    expect_mount(&mut disk, blocks, b'B', 2);
    write_superblock(&mut disk, k1fs::SLOT_B_BLOCK, 1, blocks);
    println!("k1fs: newer generation wins: ok");

    // 4. A header byte flipped under a valid magic is a CRC miss, not a
    //    mountable superblock.
    let mut raw = disk.read_block(k1fs::SLOT_B_BLOCK);
    raw[100] ^= 0x01;
    disk.write_block(k1fs::SLOT_B_BLOCK, &raw);
    expect_mount(&mut disk, blocks, b'A', 1);
    write_superblock(&mut disk, k1fs::SLOT_B_BLOCK, 1, blocks);
    println!("k1fs: crc mismatch is rejected: ok");

    // 5. Both slots unreadable: nothing to mount.
    disk.write_block(k1fs::SLOT_A_BLOCK, &garbage_block());
    disk.write_block(k1fs::SLOT_B_BLOCK, &garbage_block());
    assert!(
        matches!(K1fs::mount(&mut disk, blocks), Err(K1fsErr::NoSuperblock)),
        "no valid slot must be NoSuperblock"
    );
    println!("k1fs: no valid superblock: ok");

    // 6. A structurally valid volume that does not fit the device is Inval,
    //    distinct from an absent one.
    write_superblock(&mut disk, k1fs::SLOT_A_BLOCK, 1, blocks);
    write_superblock(&mut disk, k1fs::SLOT_B_BLOCK, 1, blocks);
    assert!(
        matches!(K1fs::mount(&mut disk, blocks - 1), Err(K1fsErr::Inval)),
        "volume past the device must be Inval"
    );
    println!("k1fs: geometry bounds: ok");

    println!("k1fs: all superblock checks passed");
}
