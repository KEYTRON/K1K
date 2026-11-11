// Host regression for the filesystem code that is compiled into the K1K fs
// service. The disk image is freshly created by Make, so allocation/free is
// safe and leaves no live directory entries behind.
extern crate alloc;

#[path = "../user/svc/fs/src/fat.rs"]
mod fat;

use std::env;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};

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

fn main() {
    let image = env::args().nth(1).expect("usage: fat16-test <disk.img>");
    if env::args().nth(2).as_deref() == Some("verify-write") {
        verify_runtime_write(&image);
        return;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .expect("open disk image");
    let mut disk = Disk(file);
    let mut fs = fat::Fat::mount(&mut disk).expect("mount FAT");
    assert_eq!(fs.kind, fat::FatKind::Fat16, "K1K disk must stay FAT16");
    let cluster = fs.alloc_cluster(&mut disk).expect("allocate FAT16 cluster");
    assert_eq!(
        fs.next_cluster(&mut disk, cluster)
            .expect("read allocated entry"),
        None
    );
    let mut bytes = vec![0xA5; fs.cluster_bytes()];
    fs.read_cluster(&mut disk, cluster, &mut bytes)
        .expect("read zeroed cluster");
    assert!(bytes.iter().all(|&b| b == 0), "new cluster must be zeroed");
    fs.free_cluster(&mut disk, cluster)
        .expect("free FAT16 cluster");
    drop(fs);
    let mut remount = fat::Fat::mount(&mut disk).expect("remount FAT");
    let again = remount
        .alloc_cluster(&mut disk)
        .expect("allocate after remount");
    assert_eq!(again, cluster, "free cluster must be reusable");
    remount.free_cluster(&mut disk, again).expect("cleanup");
    let payload: Vec<u8> = (0..1301).map(|n| (n as u8).wrapping_mul(29)).collect();
    let created = remount
        .create_root_file(&mut disk, "WARP.TST", &payload)
        .expect("create root file");
    assert!(
        fat::to_83("BAD/NAME").is_none(),
        "reject illegal 8.3 character"
    );
    assert!(
        remount
            .create_root_file(&mut disk, "WARP.TST", b"duplicate")
            .is_err(),
        "refuse an existing root name"
    );
    let mut readback = vec![0u8; payload.len()];
    let scratch_len = remount.cluster_bytes();
    let mut scratch = vec![0u8; scratch_len];
    assert_eq!(
        remount
            .read_file(&mut disk, &created, &mut scratch, &mut readback)
            .expect("read created file"),
        payload.len()
    );
    assert_eq!(readback, payload, "created file contents");
    drop(remount);
    let mut remount = fat::Fat::mount(&mut disk).expect("second remount FAT");
    let mut scratch = vec![0u8; remount.cluster_bytes()];
    let found = remount
        .lookup(&mut disk, 0, "WARP.TST", &mut scratch)
        .expect("lookup after remount")
        .expect("created entry");
    let mut readback = vec![0u8; payload.len()];
    assert_eq!(
        remount
            .read_file(&mut disk, &found, &mut scratch, &mut readback)
            .expect("read remounted file"),
        payload.len()
    );
    assert_eq!(readback, payload, "remounted file contents");
    println!("fat16: allocation, create, mirrors, zeroing and remount: ok");

    // --- The compatibility API: mkdir, replace, rename, unlink ---
    let dir = remount.mkdir(&mut disk, 0, "DIR1").expect("mkdir DIR1");
    assert!(dir.is_dir(), "mkdir must report a directory");
    assert!(
        remount.mkdir(&mut disk, 0, "DIR1").is_err(),
        "duplicate mkdir refused"
    );
    let pa: Vec<u8> = (0..777)
        .map(|n| (n as u8).wrapping_mul(29).wrapping_add(7))
        .collect();
    let pb: Vec<u8> = (0..1301)
        .map(|n| (n as u8).wrapping_mul(31).wrapping_add(3))
        .collect();
    let created = remount
        .create_file(&mut disk, dir.cluster, "FILE.TXT", &pa, false)
        .expect("create in subdirectory");
    assert!(created.cluster >= 2, "file gets real clusters");
    assert!(
        remount
            .create_file(&mut disk, dir.cluster, "FILE.TXT", b"dup", false)
            .is_err(),
        "duplicate create refused"
    );
    let replaced = remount
        .create_file(&mut disk, dir.cluster, "FILE.TXT", &pb, true)
        .expect("controlled replace");
    let mut back = vec![0u8; pb.len()];
    assert_eq!(
        remount
            .read_file(&mut disk, &replaced, &mut scratch, &mut back)
            .expect("read replaced file"),
        pb.len()
    );
    assert_eq!(back, pb, "replaced contents");
    // Replace with the flag on a missing name simply creates the file.
    remount
        .create_file(&mut disk, dir.cluster, "OTHER.TXT", b"other", true)
        .expect("replace of a missing name creates it");
    remount
        .rename(&mut disk, dir.cluster, "FILE.TXT", "MOVED.TXT")
        .expect("rename");
    assert!(
        remount
            .lookup(&mut disk, dir.cluster, "FILE.TXT", &mut scratch)
            .expect("lookup old name")
            .is_none(),
        "old name gone after rename"
    );
    assert!(
        remount
            .rename(&mut disk, dir.cluster, "MOVED.TXT", "OTHER.TXT")
            .is_err(),
        "rename onto an existing name refused"
    );
    assert!(
        remount.unlink(&mut disk, 0, "DIR1").is_err(),
        "unlink of a directory refused"
    );
    remount
        .unlink(&mut disk, dir.cluster, "MOVED.TXT")
        .expect("unlink");
    assert!(
        remount
            .lookup(&mut disk, dir.cluster, "MOVED.TXT", &mut scratch)
            .expect("lookup unlinked")
            .is_none(),
        "unlinked name gone"
    );
    remount
        .unlink(&mut disk, dir.cluster, "OTHER.TXT")
        .expect("unlink second file");
    drop(remount);
    let mut remount = fat::Fat::mount(&mut disk).expect("third remount FAT");
    let mut scratch = vec![0u8; remount.cluster_bytes()];
    let dir = remount
        .lookup(&mut disk, 0, "DIR1", &mut scratch)
        .expect("lookup DIR1")
        .expect("DIR1 must survive remount");
    assert!(dir.is_dir());
    let mut live = 0;
    remount
        .read_dir(&mut disk, dir.cluster, &mut scratch, |e| {
            if e.name[0] != b'.' {
                live += 1;
            }
        })
        .expect("read DIR1");
    assert_eq!(live, 0, "DIR1 is empty after unlink");
    println!("fat16: mkdir, replace, rename, unlink and remount: ok");
}

/// Run after QEMU exits: this is a fresh host mount of the actual NVMe image,
/// proving the guest's IPC write survived beyond the process that issued it.
fn verify_runtime_write(image: &str) {
    let file = OpenOptions::new()
        .read(true)
        .write(false)
        .open(image)
        .expect("open disk image");
    let mut disk = Disk(file);
    let mut fs = fat::Fat::mount(&mut disk).expect("remount guest-written FAT");
    let mut scratch = vec![0u8; fs.cluster_bytes()];
    let entry = fs
        .lookup(&mut disk, 0, "WRITE.TST", &mut scratch)
        .expect("lookup guest-created file")
        .expect("guest-created file missing");
    assert_eq!(entry.size, 1301, "guest file length");
    let mut got = vec![0u8; entry.size as usize];
    let n = fs
        .read_file(&mut disk, &entry, &mut scratch, &mut got)
        .expect("read guest-created file");
    assert_eq!(n, 1301);
    for (i, byte) in got.iter().enumerate() {
        assert_eq!(*byte, (i as u8).wrapping_mul(29), "guest file byte {i}");
    }
    println!("fat16: guest write survived QEMU exit and remount: ok");
    // The guest also created /DIR1 and emptied it again through unlink; both
    // the directory and the absence of the file must survive the remount.
    let dir = fs
        .lookup(&mut disk, 0, "DIR1", &mut scratch)
        .expect("lookup guest-created dir")
        .expect("guest-created dir missing");
    assert!(dir.is_dir(), "DIR1 must be a directory");
    let mut live = 0;
    fs.read_dir(&mut disk, dir.cluster, &mut scratch, |e| {
        if e.name[0] != b'.' {
            live += 1;
        }
    })
    .expect("read guest-created dir");
    assert_eq!(live, 0, "guest unlink must leave DIR1 empty");
    println!("fat16: guest mkdir/unlink survived QEMU exit and remount: ok");
}
