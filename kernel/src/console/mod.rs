//! Kernel console: mirrors output to COM1 and to the Limine framebuffer.

pub mod fb;

use core::fmt::{self, Write};
use spin::Mutex;

use crate::arch::x86_64::serial::SERIAL;

pub static FB_CONSOLE: Mutex<Option<fb::FbConsole>> = Mutex::new(None);

pub fn init_framebuffer() {
    if let Some(resp) = crate::boot::FRAMEBUFFER.response()
        && let Some(fb) = resp.framebuffers().first()
    {
        let console = fb::FbConsole::new(fb);
        *FB_CONSOLE.lock() = Some(console);
    }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        SERIAL.lock().write_fmt(args).ok();
        if let Some(con) = FB_CONSOLE.lock().as_mut() {
            con.write_fmt(args).ok();
        }
    });
}

/// Raw bytes from a service holding the console capability: to the serial port
/// as they are and to the framebuffer through its own control-character
/// handling (a backspace erases there). Bytes that are not ASCII are shown as `?`.
pub fn write_raw(bytes: &[u8]) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut serial = SERIAL.lock();
        let mut fb = FB_CONSOLE.lock();
        for &b in bytes {
            let b = if b.is_ascii() { b } else { b'?' };
            serial.write_char(b as char).ok();
            if let Some(con) = fb.as_mut() {
                con.put_char(b);
            }
        }
    });
}

/// Best-effort output used by the panic handler: bypasses locks that may be
/// held by the panicking context.
#[doc(hidden)]
pub fn _print_force(args: fmt::Arguments) {
    unsafe {
        SERIAL.force_unlock();
        FB_CONSOLE.force_unlock();
    }
    _print(args);
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::console::_print(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::console::_print(format_args!("{}\n", format_args!($($arg)*))) };
}

#[macro_export]
macro_rules! klog {
    ($tag:expr, $($arg:tt)*) => {
        $crate::println!("[{:>6}] {}", $tag, format_args!($($arg)*))
    };
}
