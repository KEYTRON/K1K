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

    fn flush(&mut self) -> Result<(), ()> {
        self.0.sync_data().map_err(|_| ())
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
        .open(&image)
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

    // 7. The CoW allocator: distinct data blocks, never a superblock slot.
    K1fs::mkfs(&mut disk, blocks).expect("mkfs for allocator tests");
    let mut fs = K1fs::mount(&mut disk, blocks).expect("mount for allocator tests");
    let a = fs.alloc().expect("alloc a");
    let b = fs.alloc().expect("alloc b");
    let c = fs.alloc().expect("alloc c");
    assert!(
        a < b && b < c,
        "allocation must be deterministic and distinct"
    );
    assert!(
        a >= k1fs::FIRST_DATA_BLOCK && fs.is_used(a) && fs.is_used(b) && fs.is_used(c),
        "allocated blocks must come from the data range and be marked used"
    );
    assert!(fs.is_used(k1fs::SLOT_A_BLOCK) && fs.is_used(k1fs::SLOT_B_BLOCK));
    println!("k1fs: allocator hands out distinct data blocks: ok");

    // 8. Commit publishes the new root: a real (empty) root directory this
    //    time - mount now scans the tree, and a garbage root must not pass.
    let slot_before = fs.slot;
    let next_gen = fs.sb.generation + 1;
    disk.write_block(a, &k1fs::Inode::empty_dir().encode(a, next_gen));
    fs.commit(&mut disk, a).expect("commit root a");
    assert_eq!(fs.sb.generation, 2);
    assert_ne!(fs.slot, slot_before, "commit must flip the slot");
    let mut fs = K1fs::mount(&mut disk, blocks).expect("remount after commit");
    let flipped = if slot_before == b'A' { b'B' } else { b'A' };
    assert_eq!(
        fs.slot, flipped,
        "remount must pick the freshly written slot"
    );
    assert_eq!(fs.sb.generation, 2);
    assert_eq!(
        fs.sb.root_block, a,
        "committed root must survive the remount"
    );
    assert!(fs.is_used(a), "the committed root block stays used");
    let d = fs.alloc().expect("alloc after commit");
    assert_ne!(d, a, "the committed root must not be handed out again");
    println!("k1fs: commit publishes the new root: ok");

    // 9. Freed blocks return to the pool after the commit that drops them.
    fs.free(a);
    fs.commit(&mut disk, k1fs::ROOT_NONE)
        .expect("commit empty root");
    let mut fs = K1fs::mount(&mut disk, blocks).expect("remount after free");
    assert_eq!(fs.sb.root_block, k1fs::ROOT_NONE);
    assert!(!fs.is_used(a), "the freed root is free again after remount");
    assert_eq!(
        fs.alloc().expect("realloc the freed block"),
        a,
        "the freed block must be allocatable again"
    );
    println!("k1fs: freed blocks return to the pool: ok");

    // 10. Exhaustion reports NoSpc: a volume of MIN_BLOCKS has total-2
    //     allocatable blocks and not one more.
    K1fs::mkfs(&mut disk, k1fs::MIN_BLOCKS).expect("mkfs tiny volume");
    let mut tiny = K1fs::mount(&mut disk, blocks).expect("mount tiny volume");
    for _ in k1fs::FIRST_DATA_BLOCK..k1fs::MIN_BLOCKS {
        tiny.alloc().expect("alloc within capacity");
    }
    assert!(
        matches!(tiny.alloc(), Err(K1fsErr::NoSpc)),
        "alloc past capacity must be NoSpc"
    );
    println!("k1fs: exhaustion reports NoSpc: ok");

    // 11. Build a real tree on a fresh volume: root with an inline file
    //     A.TXT and an empty directory DOC, all blocks CoW-allocated.
    K1fs::mkfs(&mut disk, blocks).expect("mkfs for tree tests");
    let mut fs = K1fs::mount(&mut disk, blocks).expect("mount for tree tests");
    let fi = fs.alloc().expect("alloc file inode");
    let di = fs.alloc().expect("alloc dir inode");
    let dp = fs.alloc().expect("alloc dir page");
    let root = fs.alloc().expect("alloc root inode");
    let next_gen = fs.sb.generation + 1;
    disk.write_block(
        fi,
        &k1fs::Inode::file(b"hello, K1FS")
            .expect("file")
            .encode(fi, next_gen),
    );
    disk.write_block(di, &k1fs::Inode::empty_dir().encode(di, next_gen));
    let page = k1fs::DirPage::new(vec![
        k1fs::DirEntry {
            name: b"A.TXT".to_vec(),
            kind: 1,
            target: fi,
        },
        k1fs::DirEntry {
            name: b"DOC".to_vec(),
            kind: 2,
            target: di,
        },
    ]);
    disk.write_block(dp, &page.encode(dp, next_gen).expect("encode dir page"));
    let mut root_inode = k1fs::Inode::empty_dir();
    root_inode.dir_first = dp;
    root_inode.dir_pages = 1;
    root_inode.size = 2;
    disk.write_block(root, &root_inode.encode(root, next_gen));
    fs.commit(&mut disk, root).expect("commit tree");
    println!("k1fs: tree written and committed: ok");

    // 12. A fresh mount scans the tree clean; lookups resolve; the inline
    //     file reads back whole and in slices.
    let mut fs = K1fs::mount(&mut disk, blocks).expect("remount tree");
    let f = fs.lookup(&mut disk, "/A.TXT").expect("lookup A.TXT");
    assert!(!f.is_dir() && f.size == 11, "A.TXT is an 11-byte file");
    let mut buf = [0u8; 11];
    let n = fs
        .read_file(&mut disk, &f, 0, &mut buf)
        .expect("read A.TXT");
    assert_eq!(n, 11);
    assert_eq!(&buf, b"hello, K1FS");
    let mut tail = [0u8; 4];
    fs.read_file(&mut disk, &f, 7, &mut tail)
        .expect("read tail");
    assert_eq!(&tail, b"K1FS");
    let doc = fs.lookup(&mut disk, "/DOC").expect("lookup DOC");
    assert!(doc.is_dir(), "DOC is a directory");
    assert!(
        matches!(fs.lookup(&mut disk, "/DOC/LOST"), Err(K1fsErr::NoEnt)),
        "a missing name is NoEnt"
    );
    let root_dir = fs.lookup(&mut disk, "/").expect("lookup /");
    let listing = fs.list_dir(&mut disk, &root_dir).expect("list /");
    assert_eq!(listing.len(), 2, "root lists both entries");
    println!("k1fs: lookup and inline read: ok");

    // 13. An extent file: two data blocks with a hole between them, read
    //     across the boundaries, hole zero-filled.
    let d1 = fs.alloc().expect("alloc data 1");
    let d2 = fs.alloc().expect("alloc data 2");
    let ef = fs.alloc().expect("alloc extent file inode");
    let mut payload_a = [0u8; k1fs::BLOCK];
    let mut payload_b = [0u8; k1fs::BLOCK];
    payload_a.fill(0xA5);
    payload_b.fill(0x5A);
    disk.write_block(d1, &payload_a);
    disk.write_block(d2, &payload_b);
    let extent_file = k1fs::Inode {
        mode: k1fs::MODE_REG,
        size: 3 * k1fs::BLOCK as u64,
        inline: Vec::new(),
        extents: vec![
            k1fs::Extent {
                lba: d1,
                nblocks: 1,
                hole: false,
            },
            k1fs::Extent {
                lba: 0,
                nblocks: 1,
                hole: true,
            },
            k1fs::Extent {
                lba: d2,
                nblocks: 1,
                hole: false,
            },
        ],
        ext_overflow: k1fs::ROOT_NONE,
        dir_first: k1fs::ROOT_NONE,
        dir_pages: 0,
    };
    let ef_raw = extent_file.encode(ef, next_gen + 1);
    disk.write_block(ef, &ef_raw);
    let np = fs.alloc().expect("alloc new dir page");
    let page = k1fs::DirPage::new(vec![
        k1fs::DirEntry {
            name: b"A.TXT".to_vec(),
            kind: 1,
            target: fi,
        },
        k1fs::DirEntry {
            name: b"BIG.BIN".to_vec(),
            kind: 1,
            target: ef,
        },
        k1fs::DirEntry {
            name: b"DOC".to_vec(),
            kind: 2,
            target: di,
        },
    ]);
    disk.write_block(
        np,
        &page.encode(np, next_gen + 1).expect("encode new dir page"),
    );
    let mut root_inode = k1fs::Inode::empty_dir();
    root_inode.dir_first = np;
    root_inode.dir_pages = 1;
    root_inode.size = 3;
    let root2 = fs.alloc().expect("alloc new root inode");
    disk.write_block(root2, &root_inode.encode(root2, next_gen + 1));
    let mut fs = fs;
    fs.commit(&mut disk, root2).expect("commit extent file");
    let fs = K1fs::mount(&mut disk, blocks).expect("remount with extent file");
    let f = fs.lookup(&mut disk, "/BIG.BIN").expect("lookup BIG.BIN");
    assert_eq!(f.size, 3 * k1fs::BLOCK as u64);
    let mut head = [0u8; 16];
    fs.read_file(&mut disk, &f, 0, &mut head)
        .expect("read head");
    assert!(head.iter().all(|&b| b == 0xA5), "first block payload");
    let mut hole = [0xFFu8; 16];
    fs.read_file(&mut disk, &f, k1fs::BLOCK as u64, &mut hole)
        .expect("read hole");
    assert!(hole.iter().all(|&b| b == 0), "hole reads as zeros");
    let mut mid = [0u8; 16];
    fs.read_file(&mut disk, &f, 2 * k1fs::BLOCK as u64, &mut mid)
        .expect("read third block");
    assert!(mid.iter().all(|&b| b == 0x5A), "third block payload");
    println!("k1fs: extent and hole reads: ok");

    // 14. A torn write inside a reachable block is an honest mount failure
    //     that names the block; rewriting the block repairs the volume.
    let mut broken = ef_raw;
    broken[k1fs::BLOCK - 40] ^= 0x01;
    disk.write_block(ef, &broken);
    assert!(
        matches!(K1fs::mount(&mut disk, blocks), Err(K1fsErr::MetaCrc(b)) if b == ef),
        "a flipped byte in a reachable block must fail the mount naming it"
    );
    disk.write_block(ef, &ef_raw);
    K1fs::mount(&mut disk, blocks).expect("remount after repair");
    println!("k1fs: torn write refuses to mount, repair remounts: ok");

    // 15. Two extents claiming one data block are corruption: the second
    //     claim is an Alias and the mount fails.
    let mut fs = K1fs::mount(&mut disk, blocks).expect("mount for alias test");
    let af = fs.alloc().expect("alloc alias file inode");
    let alias_file = k1fs::Inode {
        mode: k1fs::MODE_REG,
        size: k1fs::BLOCK as u64,
        inline: Vec::new(),
        extents: vec![k1fs::Extent {
            lba: d1,
            nblocks: 1,
            hole: false,
        }],
        ext_overflow: k1fs::ROOT_NONE,
        dir_first: k1fs::ROOT_NONE,
        dir_pages: 0,
    };
    disk.write_block(af, &alias_file.encode(af, next_gen + 2));
    let np2 = fs.alloc().expect("alloc alias dir page");
    let page = k1fs::DirPage::new(vec![
        k1fs::DirEntry {
            name: b"A.TXT".to_vec(),
            kind: 1,
            target: fi,
        },
        k1fs::DirEntry {
            name: b"ALIAS".to_vec(),
            kind: 1,
            target: af,
        },
        k1fs::DirEntry {
            name: b"BIG.BIN".to_vec(),
            kind: 1,
            target: ef,
        },
        k1fs::DirEntry {
            name: b"DOC".to_vec(),
            kind: 2,
            target: di,
        },
    ]);
    disk.write_block(
        np2,
        &page
            .encode(np2, next_gen + 2)
            .expect("encode alias dir page"),
    );
    let mut root_inode = k1fs::Inode::empty_dir();
    root_inode.dir_first = np2;
    root_inode.dir_pages = 1;
    root_inode.size = 4;
    let root3 = fs.alloc().expect("alloc alias root inode");
    disk.write_block(root3, &root_inode.encode(root3, next_gen + 2));
    fs.commit(&mut disk, root3).expect("commit alias tree");
    assert!(
        matches!(K1fs::mount(&mut disk, blocks), Err(K1fsErr::Alias(b)) if b == d1),
        "a shared data block must fail the mount as an alias"
    );
    println!("k1fs: shared data block is an alias: ok");

    // 16. Mutations on a fresh volume: create files and directories, each a
    //     full commit, surviving remounts.
    K1fs::mkfs(&mut disk, blocks).expect("mkfs for mutation tests");
    let mut fs = K1fs::mount(&mut disk, blocks).expect("mount for mutations");
    fs.init_root(&mut disk).expect("init root");
    fs.create_file(&mut disk, "/A.TXT", b"hello, mutations")
        .expect("create A.TXT");
    fs.mkdir(&mut disk, "/DOC").expect("mkdir DOC");
    fs.create_file(&mut disk, "/DOC/B.TXT", b"nested content")
        .expect("create DOC/B.TXT");
    let fs = K1fs::mount(&mut disk, blocks).expect("remount mutations");
    let f = fs.lookup(&mut disk, "/A.TXT").expect("A.TXT resolves");
    let mut buf = [0u8; 16];
    fs.read_file(&mut disk, &f, 0, &mut buf)
        .expect("read A.TXT");
    assert_eq!(&buf, b"hello, mutations");
    let nested = fs.lookup(&mut disk, "/DOC/B.TXT").expect("nested resolves");
    assert_eq!(nested.size, 14);
    println!("k1fs: create/mkdir survive a remount: ok");

    // 17. Refusals: duplicates, missing parents, files as directories,
    //     unlinked names.
    let mut fs = fs;
    assert!(
        matches!(
            fs.create_file(&mut disk, "/A.TXT", b"x"),
            Err(K1fsErr::Exists)
        ),
        "duplicate create is Exists"
    );
    assert!(
        matches!(
            fs.create_file(&mut disk, "/LOST/C.TXT", b"x"),
            Err(K1fsErr::NoEnt)
        ),
        "a missing parent is NoEnt"
    );
    assert!(
        matches!(fs.mkdir(&mut disk, "/A.TXT/D"), Err(K1fsErr::NotDir)),
        "a file is not a directory"
    );
    assert!(
        matches!(fs.unlink(&mut disk, "/DOC"), Err(K1fsErr::IsDir)),
        "unlink refuses directories"
    );
    assert!(
        matches!(fs.unlink(&mut disk, "/NOPE"), Err(K1fsErr::NoEnt)),
        "unlink of a missing name is NoEnt"
    );
    println!("k1fs: mutation refusals: ok");

    // 18. unlink removes the name; the blocks of the old version become free
    //     space again with the commit.
    fs.unlink(&mut disk, "/A.TXT").expect("unlink A.TXT");
    assert!(
        matches!(fs.lookup(&mut disk, "/A.TXT"), Err(K1fsErr::NoEnt)),
        "unlinked name is gone"
    );
    let fs = K1fs::mount(&mut disk, blocks).expect("remount after unlink");
    assert!(
        matches!(fs.lookup(&mut disk, "/A.TXT"), Err(K1fsErr::NoEnt)),
        "unlink survives the remount"
    );
    let root_dir = fs.lookup(&mut disk, "/").expect("lookup /");
    let listing = fs.list_dir(&mut disk, &root_dir).expect("list /");
    assert_eq!(listing.len(), 1, "only DOC remains");
    println!("k1fs: unlink frees the name: ok");

    // 19. rename within a directory, including onto an existing file (the
    //     WARP activation shape: name stays, target swaps, old blocks freed).
    let mut fs = fs;
    fs.rename(&mut disk, "/DOC/B.TXT", "/DOC/C.TXT")
        .expect("rename B to C");
    assert!(matches!(
        fs.lookup(&mut disk, "/DOC/B.TXT"),
        Err(K1fsErr::NoEnt)
    ));
    let c = fs.lookup(&mut disk, "/DOC/C.TXT").expect("C resolves");
    assert_eq!(c.size, 14, "rename keeps the inode");
    fs.create_file(&mut disk, "/DOC/D.TXT", b"replace me")
        .expect("create D");
    fs.rename(&mut disk, "/DOC/C.TXT", "/DOC/D.TXT")
        .expect("rename onto D");
    assert!(matches!(
        fs.lookup(&mut disk, "/DOC/C.TXT"),
        Err(K1fsErr::NoEnt)
    ));
    let d = fs.lookup(&mut disk, "/DOC/D.TXT").expect("D resolves");
    assert_eq!(d.size, 14, "overwrite swapped the target, not the name");
    let mut content = [0u8; 14];
    fs.read_file(&mut disk, &d, 0, &mut content)
        .expect("read overwritten D");
    assert_eq!(&content, b"nested content");
    fs.create_file(&mut disk, "/E.TXT", b"occupied")
        .expect("create E");
    assert!(
        matches!(fs.rename(&mut disk, "/DOC", "/E.TXT"), Err(K1fsErr::IsDir)),
        "a directory refuses to be renamed onto an existing name"
    );
    assert!(
        matches!(fs.rename(&mut disk, "/E.TXT", "/DOC"), Err(K1fsErr::IsDir)),
        "a file refuses to be renamed onto a directory"
    );
    let fs = K1fs::mount(&mut disk, blocks).expect("remount after renames");
    let doc = fs.lookup(&mut disk, "/DOC").expect("DOC survives");
    let listing = fs.list_dir(&mut disk, &doc).expect("list DOC");
    assert_eq!(listing.len(), 1, "only D.TXT remains in DOC");
    println!("k1fs: rename and overwrite survive a remount: ok");

    // 20. A large file exercises data extents and the EXT chain: 1 MiB of
    //     patterned data is 256 blocks - more extents than an inode holds.
    let mut fs = fs;
    let big: Vec<u8> = (0..(1024 * 1024)).map(|n| (n / 4096) as u8).collect();
    fs.create_file(&mut disk, "/BIG.BIN", &big)
        .expect("create BIG.BIN");
    let fs = K1fs::mount(&mut disk, blocks).expect("remount with BIG.BIN");
    let f = fs.lookup(&mut disk, "/BIG.BIN").expect("BIG.BIN resolves");
    assert_eq!(f.size, 1024 * 1024);
    for (offset, expect) in [
        (0usize, 0u8),
        (4096, 1),
        (4096 * 100, 100),
        (1024 * 1024 - 1, 255),
    ] {
        let mut sample = [0u8; 1];
        fs.read_file(&mut disk, &f, offset as u64, &mut sample)
            .expect("sample BIG.BIN");
        assert_eq!(sample[0], expect, "byte at {offset}");
    }
    let mut sample = [0u8; 64];
    fs.read_file(&mut disk, &f, 4096 * 128 + 100, &mut sample)
        .expect("read across extents");
    assert_eq!(sample[0], 128, "cross-extent read");
    println!("k1fs: 1 MiB file through EXT chain round-trips: ok");

    // 21. NoSpc is decided before anything is written: a full volume refuses
    //     the create and mounts unchanged afterwards.
    K1fs::mkfs(&mut disk, k1fs::MIN_BLOCKS).expect("mkfs tiny volume");
    let mut tiny = K1fs::mount(&mut disk, blocks).expect("mount tiny");
    tiny.init_root(&mut disk).expect("tiny root");
    let generation_before = tiny.sb.generation;
    let payload = [0u8; k1fs::BLOCK * 8];
    assert!(
        matches!(
            tiny.create_file(&mut disk, "/BIG", &payload),
            Err(K1fsErr::NoSpc)
        ),
        "a create that does not fit is NoSpc"
    );
    let tiny = K1fs::mount(&mut disk, blocks).expect("tiny remounts untouched");
    assert_eq!(
        tiny.sb.generation, generation_before,
        "NoSpc leaves no trace"
    );
    println!("k1fs: NoSpc is decided before any write: ok");

    // 22. Power cuts at every single write of a commit. The commit creates a
    //     file in a subdirectory, so the sequence propagates through the
    //     parent and the root. After each cut the volume must mount the old
    //     tree exactly - the same names, the same content, the same
    //     generation.
    K1fs::mkfs(&mut disk, blocks).expect("mkfs for crash tests");
    let mut fs = K1fs::mount(&mut disk, blocks).expect("mount for crash tests");
    fs.init_root(&mut disk).expect("crash-test root");
    fs.create_file(&mut disk, "/A.TXT", b"old")
        .expect("seed file");
    fs.mkdir(&mut disk, "/DOC").expect("seed dir");
    let generation = fs.sb.generation;
    disk.0
        .seek(SeekFrom::Start(0))
        .expect("rewind for snapshot");
    let mut snapshot = Vec::new();
    disk.0.read_to_end(&mut snapshot).expect("snapshot");

    let writes = {
        let mut cd = CountingDisk {
            file: restore(&image, &snapshot),
            writes: 0,
        };
        let mut fs = K1fs::mount(&mut cd, blocks).expect("counting mount");
        fs.create_file(&mut cd, "/DOC/B.TXT", b"propagated")
            .expect("counting create");
        cd.writes
    };
    for cut in 0..writes {
        let mut fd = FaultDisk {
            file: restore(&image, &snapshot),
            cut,
        };
        let mut fs = K1fs::mount(&mut fd, blocks).expect("fault mount");
        assert!(
            fs.create_file(&mut fd, "/DOC/B.TXT", b"propagated")
                .is_err(),
            "a cut at write {cut} must abort the commit"
        );
        expect_old_state(&image, blocks, generation);
    }
    println!("k1fs: power cut at every write of a commit keeps the old tree: ok ({writes} cuts)");

    println!("k1fs: all superblock, allocator, metadata and mutation checks passed");
}

/// A disk that dies after `cut` writes: power cuts at a chosen point of a
/// commit. Flush models the barrier as atomic - it either completes (making
/// everything before it durable) or the machine is off, and cutting the
/// writes before the barrier is already covered by cutting them directly.
/// Reads still answer: a power cut stops writes, it does not erase the disk.
struct FaultDisk {
    file: std::fs::File,
    cut: usize,
}

impl fat::BlockDevice for FaultDisk {
    fn read(&mut self, lba: u64, count: usize, out: &mut [u8]) -> Result<(), ()> {
        let bytes = count.checked_mul(512).ok_or(())?;
        if out.len() < bytes {
            return Err(());
        }
        self.file
            .seek(SeekFrom::Start(lba.checked_mul(512).ok_or(())?))
            .map_err(|_| ())?;
        self.file.read_exact(&mut out[..bytes]).map_err(|_| ())
    }

    fn write(&mut self, lba: u64, count: usize, data: &[u8]) -> Result<(), ()> {
        if self.cut == 0 {
            return Err(());
        }
        self.cut -= 1;
        let bytes = count.checked_mul(512).ok_or(())?;
        if data.len() < bytes {
            return Err(());
        }
        self.file
            .seek(SeekFrom::Start(lba.checked_mul(512).ok_or(())?))
            .map_err(|_| ())?;
        self.file.write_all(&data[..bytes]).map_err(|_| ())
    }

    fn flush(&mut self) -> Result<(), ()> {
        Ok(())
    }
}

/// Counts writes without ever failing them.
struct CountingDisk {
    file: std::fs::File,
    writes: usize,
}

impl fat::BlockDevice for CountingDisk {
    fn read(&mut self, lba: u64, count: usize, out: &mut [u8]) -> Result<(), ()> {
        let bytes = count.checked_mul(512).ok_or(())?;
        if out.len() < bytes {
            return Err(());
        }
        self.file
            .seek(SeekFrom::Start(lba.checked_mul(512).ok_or(())?))
            .map_err(|_| ())?;
        self.file.read_exact(&mut out[..bytes]).map_err(|_| ())
    }

    fn write(&mut self, lba: u64, count: usize, data: &[u8]) -> Result<(), ()> {
        let bytes = count.checked_mul(512).ok_or(())?;
        if data.len() < bytes {
            return Err(());
        }
        self.file
            .seek(SeekFrom::Start(lba.checked_mul(512).ok_or(())?))
            .map_err(|_| ())?;
        self.file.write_all(&data[..bytes]).map_err(|_| ())?;
        self.writes += 1;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), ()> {
        Ok(())
    }
}

/// Restore the image from a snapshot taken before the mutation under test.
fn restore(image: &str, snapshot: &[u8]) -> std::fs::File {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .expect("reopen image");
    file.seek(SeekFrom::Start(0)).expect("rewind");
    file.write_all(snapshot).expect("restore snapshot");
    file.set_len(snapshot.len() as u64).expect("truncate");
    file.seek(SeekFrom::Start(0)).expect("rewind 2");
    file
}

/// The old tree, exactly as before the mutation, must mount and answer after
/// a power cut at any point of the commit.
fn expect_old_state(image: &str, blocks: u64, generation: u64) {
    let mut disk = Disk(
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(image)
            .expect("open for check"),
    );
    let fs = K1fs::mount(&mut disk, blocks).expect("mount after power cut");
    assert_eq!(
        fs.sb.generation, generation,
        "no cut may bump the generation"
    );
    let f = fs.lookup(&mut disk, "/A.TXT").expect("old file resolves");
    let mut buf = [0u8; 3];
    fs.read_file(&mut disk, &f, 0, &mut buf)
        .expect("read old file");
    assert_eq!(&buf, b"old", "old content intact");
    let doc = fs.lookup(&mut disk, "/DOC").expect("old dir resolves");
    assert!(
        fs.list_dir(&mut disk, &doc)
            .expect("list old dir")
            .is_empty()
    );
    assert!(
        matches!(fs.lookup(&mut disk, "/DOC/B.TXT"), Err(K1fsErr::NoEnt)),
        "the new name must not exist after any cut"
    );
}
