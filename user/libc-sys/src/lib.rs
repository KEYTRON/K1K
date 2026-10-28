//! k1kc-sys — the part of the C library that needs a system call.
//!
//! The rest of the C library (`libc/src/*.c`) is plain C on top of the handful of
//! functions here, so everything that touches K1K sits in one small file:
//! console and keyboard through the capabilities the manifest granted (`console`,
//! `tty`), the heap `k1k-rt` provides, and whole-file reads through the file
//! service (`fs`). This crate is linked into a C program as a static library; it
//! brings the runtime's `_start`, which calls `k1k_main`, which calls `main`.
#![no_std]

extern crate alloc;

use alloc::alloc::{Layout, alloc, dealloc};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int};

use k1k_rt::file::FileClient;
use k1k_rt::{Cap, console_write, granted, info, log_bytes, recv};

unsafe extern "C" {
    fn main(argc: c_int, argv: *const *const c_char) -> c_int;
}

/// Entry point of every C program: `_start` in `k1k-rt` jumps here.
#[unsafe(no_mangle)]
pub extern "C" fn k1k_main() -> ! {
    static NAME: &[u8] = b"program\0";
    let argv: [*const c_char; 2] = [NAME.as_ptr().cast(), core::ptr::null()];
    let code = unsafe { main(1, argv.as_ptr()) };
    k1k_rt::exit(code as i64)
}

/// Terminate the program with `code`.
#[unsafe(no_mangle)]
pub extern "C" fn k1kc_exit(code: c_int) -> ! {
    k1k_rt::exit(code as i64)
}

/// Write bytes to the console (descriptors 1 and 2). Without a `console`
/// capability the text goes to the kernel log instead, prefix and all.
///
/// # Safety
/// `buf` must be valid for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn k1kc_write(_fd: c_int, buf: *const u8, len: usize) -> isize {
    let s = unsafe { core::slice::from_raw_parts(buf, len) };
    match granted("console") {
        Some(c) => match console_write(c, s) {
            Ok(()) => len as isize,
            Err(_) => -1,
        },
        None => {
            log_bytes(s);
            len as isize
        }
    }
}

/// The next key from the terminal, blocking; -1 without a `tty` capability.
#[unsafe(no_mangle)]
pub extern "C" fn k1kc_getkey() -> c_int {
    let Some(tty) = granted("tty") else {
        return -1;
    };
    match recv(tty) {
        Ok(m) => (m.words[0] & 0xff) as c_int,
        Err(_) => -1,
    }
}

/// Milliseconds since boot.
#[unsafe(no_mangle)]
pub extern "C" fn k1kc_uptime_ms() -> u64 {
    info().uptime_ms
}

#[unsafe(no_mangle)]
pub extern "C" fn k1kc_task_id() -> u32 {
    info().task_id
}

/// Memory from the task heap, 16-byte aligned; null when it is exhausted.
#[unsafe(no_mangle)]
pub extern "C" fn k1kc_alloc(size: usize) -> *mut u8 {
    match Layout::from_size_align(size.max(1), 16) {
        Ok(l) => unsafe { alloc(l) },
        Err(_) => core::ptr::null_mut(),
    }
}

/// Give back memory from [`k1kc_alloc`], with the size it was asked for.
///
/// # Safety
/// `ptr` and `size` must be exactly what `k1kc_alloc` was called with.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn k1kc_free(ptr: *mut u8, size: usize) {
    if let Ok(l) = Layout::from_size_align(size.max(1), 16) {
        unsafe { dealloc(ptr, l) };
    }
}

/// Read a whole file through the file service into a fresh buffer. Returns 0 and
/// sets `*out`/`*out_len`, or a negative number: -1 no `fs` capability, -2 cannot
/// reach the service, -3 no such file, -4 read failed. The connection lasts only
/// for this call, because the file service takes one client at a time. Free the
/// buffer with [`k1kc_unload`].
///
/// # Safety
/// `path` must be valid for `path_len` bytes; `out` and `out_len` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn k1kc_load(
    path: *const u8,
    path_len: usize,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> c_int {
    let Ok(path) = core::str::from_utf8(unsafe { core::slice::from_raw_parts(path, path_len) })
    else {
        return -3;
    };
    let Some(ep) = granted("fs") else {
        return -1;
    };
    let Ok(mut fs) = FileClient::new(Cap(ep.0), 16) else {
        return -2;
    };
    let result = (|| {
        let file = fs.open(path).map_err(|_| -3)?;
        let mut data = Vec::new();
        let read = fs.read_all(file, &mut data);
        let _ = fs.close(file);
        read.map_err(|_| -4)?;
        Ok(data)
    })();
    fs.disconnect();
    match result {
        Ok(data) => {
            let boxed = data.into_boxed_slice();
            unsafe {
                *out_len = boxed.len();
                *out = Box::into_raw(boxed).cast();
            }
            0
        }
        Err(code) => code,
    }
}

/// Free a buffer from [`k1kc_load`].
///
/// # Safety
/// `ptr`/`len` must be exactly what `k1kc_load` returned.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn k1kc_unload(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        drop(unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(ptr, len)) });
    }
}
