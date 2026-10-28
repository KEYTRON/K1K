//! sh — the first shell on K1K, an ordinary ring-3 service.
//!
//! It has no authority of its own beyond what its manifest line gives it: `tty`
//! (the keys the keyboard driver sends), `console` (raw text out) and `fs` (the
//! file service). Everything it can do is built from those three, so a shell
//! that crashes takes nothing with it and the supervisor starts a new one.
//!
//! It reads one key at a time, edits the line itself (echo, backspace) and runs
//! a handful of built-in commands. Programs are not started from here yet:
//! that needs `control`, and a C library and a process model first.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use k1k_rt::file::FileClient;
use k1k_rt::fsproto::DirRec;
use k1k_rt::{Cap, granted, heap_stats, info, log, recv};

const PROMPT: &str = "k1k> ";
const MAX_LINE: usize = 200;

struct Term {
    console: Cap,
}

impl Term {
    fn put(&self, s: &str) {
        let _ = k1k_rt::console_write(self.console, s.as_bytes());
    }
    fn line(&self, s: &str) {
        self.put(s);
        self.put("\n");
    }
}

fn main() -> ! {
    let (Some(tty), Some(console)) = (granted("tty"), granted("console")) else {
        log!("sh needs 'tty' and 'console' in its manifest entry");
        k1k_rt::exit(1);
    };
    let fs = granted("fs");
    let term = Term { console };
    log!("shell online (ring 3, task {})", info().task_id);
    term.line("");
    term.line("K1K shell. Type 'help'.");
    term.put(PROMPT);

    let mut line = String::new();
    loop {
        let Ok(msg) = recv(tty) else {
            k1k_rt::sleep_ms(100);
            continue;
        };
        match msg.words[0] as u8 {
            b'\n' | b'\r' => {
                term.put("\n");
                run(&term, fs, line.trim());
                line.clear();
                term.put(PROMPT);
            }
            0x08 | 0x7f => {
                if line.pop().is_some() {
                    term.put("\x08 \x08");
                }
            }
            c @ 0x20..=0x7e if line.len() < MAX_LINE => {
                line.push(c as char);
                let mut b = [0u8; 4];
                term.put((c as char).encode_utf8(&mut b));
            }
            _ => {}
        }
    }
}

fn run(term: &Term, fs: Option<Cap>, line: &str) {
    let (cmd, rest) = match line.split_once(' ') {
        Some((c, r)) => (c, r.trim()),
        None => (line, ""),
    };
    match cmd {
        "" => {}
        "help" => {
            term.line("help            this list");
            term.line("echo <text>     print text");
            term.line("ls [path]       list a directory (default /)");
            term.line("cat <path>      print a file");
            term.line("uptime          time since boot");
            term.line("mem             heap of this shell");
            term.line("ver             what this is");
        }
        "echo" => term.line(rest),
        "ver" => term.line(&format!(
            "K1K {} - shell, task {}",
            env!("CARGO_PKG_VERSION"),
            info().task_id
        )),
        "uptime" => {
            let ms = info().uptime_ms;
            term.line(&format!("up {}.{:03} s", ms / 1000, ms % 1000));
        }
        "mem" => {
            let h = heap_stats();
            term.line(&format!("heap: {} B in {} block(s)", h.in_use, h.live));
        }
        "ls" => with_fs(term, fs, |c| {
            ls(term, c, if rest.is_empty() { "/" } else { rest })
        }),
        "cat" if rest.is_empty() => term.line("usage: cat <path>"),
        "cat" => with_fs(term, fs, |c| cat(term, c, rest)),
        other => term.line(&format!("{}: not a built-in command (try 'help')", other)),
    }
}

/// A connection to the file service for the length of one command. The server
/// takes one client at a time, so a shell that kept its connection would keep
/// every other service out.
fn with_fs(term: &Term, fs: Option<Cap>, f: impl FnOnce(&FileClient)) {
    let Some(ep) = fs else {
        term.line("no 'fs' capability in my manifest entry");
        return;
    };
    match FileClient::new(ep, 16) {
        Ok(mut c) => {
            f(&c);
            c.disconnect();
        }
        Err(e) => term.line(&format!("cannot reach the file service: {:?}", e)),
    }
}

fn ls(term: &Term, fs: &FileClient, path: &str) {
    let mut entries = [DirRec {
        name: [0; 11],
        attr: 0,
        size: 0,
    }; 32];
    match fs.list(path, &mut entries) {
        Ok(n) => {
            for e in entries.iter().take(n) {
                let mut buf = [0u8; 13];
                let name = e.display(&mut buf);
                if e.attr & 0x10 != 0 {
                    term.line(&format!("{}/", name));
                } else {
                    term.line(&format!("{:<14}{} B", name, e.size));
                }
            }
            if n == 0 {
                term.line("(empty)");
            }
        }
        Err(e) => term.line(&format!("ls: {}: {:?}", path, e)),
    }
}

fn cat(term: &Term, fs: &FileClient, path: &str) {
    let file = match fs.open(path) {
        Ok(f) => f,
        Err(e) => {
            term.line(&format!("cat: {}: {:?}", path, e));
            return;
        }
    };
    let mut text = Vec::new();
    match fs.read_all(file, &mut text) {
        Ok(_) => {
            // Text only: anything else would drive the console with raw bytes.
            let shown: String = text
                .iter()
                .take(2048)
                .map(|&b| {
                    if b == b'\n' || (0x20..0x7f).contains(&b) {
                        b as char
                    } else {
                        '.'
                    }
                })
                .collect();
            term.put(&shown);
            if !shown.ends_with('\n') {
                term.put("\n");
            }
            if text.len() > 2048 {
                term.line(&format!("... ({} B in all)", text.len()));
            }
        }
        Err(e) => term.line(&format!("cat: read failed: {:?}", e)),
    }
    let _ = fs.close(file);
}

k1k_rt::main!(main);
