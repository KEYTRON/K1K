//! FAT16 write-path smoke test.
//!
//! It uses the public file-service IPC rather than an in-process test so the
//! boot test covers fs -> blk -> NVMe persistence boundaries as well.
#![no_std]
#![no_main]

use k1k_rt::file::FileClient;
use k1k_rt::{exit, granted, log};

const NAME: &str = "/WRITE.TST";
const LEN: usize = 1301;
const DIR: &str = "/DIR1";
const FILE_A: &str = "/DIR1/FILE.TXT";
const FILE_B: &str = "/DIR1/MOVED.TXT";

fn fail(what: &str) -> ! {
    log!("fatwrite: {}", what);
    exit(1)
}

fn fill(buf: &mut [u8], mul: u8, add: u8) {
    for (i, byte) in buf.iter_mut().enumerate() {
        *byte = (i as u8).wrapping_mul(mul).wrapping_add(add);
    }
}

/// Open `path`, read it back and require exactly `want`.
fn verify(fs: &FileClient, path: &str, want: &[u8]) {
    let file = fs.open(path).unwrap_or_else(|_| fail("open for verify"));
    let mut got = [0u8; LEN];
    let n = fs
        .read(file, 0, &mut got[..want.len()])
        .unwrap_or_else(|_| fail("read for verify"));
    let _ = fs.close(file);
    if n != want.len() || got[..n] != *want {
        fail("readback mismatch");
    }
}

fn main() -> ! {
    let Some(ep) = granted("fs") else {
        fail("no fs capability");
    };
    let fs = FileClient::new(ep, 16).unwrap_or_else(|_| fail("cannot connect"));

    // The supervisor restarts a failed service on the same disk, so every
    // step below tolerates the leftovers of a previous run: names are cleared
    // or accepted as already present before anything is asserted.
    let _ = fs.unlink(NAME);
    let mut want = [0u8; LEN];
    fill(&mut want, 29, 0);
    fs.create_root(NAME, &want)
        .unwrap_or_else(|_| fail("create failed"));
    verify(&fs, NAME, &want);
    log!("fatwrite: root create/read: ok");

    // The compatibility API: a subdirectory, controlled replace, rename and
    // unlink, all through the same public IPC.
    if fs.mkdir(DIR).is_err() && !fs.stat(DIR).is_ok_and(|s| s.is_dir) {
        fail("mkdir failed");
    }
    let _ = fs.unlink(FILE_A);
    let _ = fs.unlink(FILE_B);
    let mut pa = [0u8; 777];
    fill(&mut pa, 29, 7);
    let mut pb = [0u8; LEN];
    fill(&mut pb, 31, 3);
    fs.create(FILE_A, &pa, false)
        .unwrap_or_else(|_| fail("subdir create failed"));
    verify(&fs, FILE_A, &pa);
    if fs.create(FILE_A, b"x", false).is_ok() {
        fail("duplicate create must fail");
    }
    fs.create(FILE_A, &pb, true)
        .unwrap_or_else(|_| fail("replace failed"));
    verify(&fs, FILE_A, &pb);
    fs.rename(FILE_A, FILE_B)
        .unwrap_or_else(|_| fail("rename failed"));
    if fs.stat(FILE_A).is_ok() {
        fail("old name still there");
    }
    verify(&fs, FILE_B, &pb);
    fs.unlink(FILE_B).unwrap_or_else(|_| fail("unlink failed"));
    if fs.stat(FILE_B).is_ok() {
        fail("unlinked file still there");
    }
    log!("fatwrite: mkdir/replace/rename/unlink: ok");
    exit(0)
}

k1k_rt::main!(main);
