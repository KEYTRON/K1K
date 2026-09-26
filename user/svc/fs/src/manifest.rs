//! The service manifest: which programs `fs` starts from disk, and what
//! authority each of them gets.
//!
//! One line per service, `#` starts a comment:
//!
//! ```text
//! # name   image           capabilities (comma separated, optional)
//! hello    HELLO.EOF       fs
//! flaky    FLAKY.EOF
//! console  CONSOLE.ELF     fs,control
//! ```
//!
//! The image is a path on the volume — relative names resolve under `/SVC`.
//! Capability names are resolved by `fs` against the capabilities it holds
//! itself, so a manifest can only hand out authority that already exists
//! somewhere in the system; a typo is an error, never a silent "no access".
//!
//! Nothing is started unless the manifest lists it: dropping a file into
//! `/SVC` no longer runs it, which is the whole point of the file.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// One service to start.
pub struct Entry {
    /// Name the supervisor knows the service by.
    pub name: String,
    /// Path of the ELF on the volume.
    pub image: String,
    /// Capability names from the manifest, in order.
    pub caps: Vec<String>,
    /// Manifest line, for error messages.
    pub line: usize,
}

pub struct Parsed {
    pub entries: Vec<Entry>,
    /// `(line, reason)` for every line that was rejected.
    pub errors: Vec<(usize, String)>,
}

/// Parse a manifest. Bad lines are reported and skipped; one typo must not
/// keep the rest of the system from booting.
pub fn parse(text: &str) -> Parsed {
    let mut entries = Vec::new();
    let mut errors = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let text = match raw.find('#') {
            Some(h) => &raw[..h],
            None => raw,
        };
        let mut fields = text.split_whitespace();
        let (Some(name), Some(image), third) = (fields.next(), fields.next(), fields.next()) else {
            if !text.trim().is_empty() {
                errors.push((line, "expected: name image [caps]".to_string()));
            }
            continue;
        };
        if fields.next().is_some() {
            errors.push((line, "too many fields".to_string()));
            continue;
        }
        if name.is_empty() || name.len() > 32 || !name.bytes().all(|b| b.is_ascii_graphic()) {
            errors.push((line, "invalid service name".to_string()));
            continue;
        }
        if image.is_empty() {
            errors.push((line, "empty image path".to_string()));
            continue;
        }
        let mut caps = Vec::new();
        let mut bad_caps = false;
        if let Some(list) = third {
            for cap in list.split(',') {
                let cap = cap.trim();
                if cap.is_empty() {
                    errors.push((line, "empty capability name".to_string()));
                    bad_caps = true;
                    break;
                }
                caps.push(cap.to_string());
            }
        }
        if bad_caps {
            continue;
        }
        entries.push(Entry {
            name: name.to_string(),
            image: image.to_string(),
            caps,
            line,
        });
    }
    Parsed { entries, errors }
}

/// The `caps=` launch argument for a service: the slot each granted capability
/// ended up in. The supervisor numbers them in manifest order, and the
/// service looks them up with `k1k_rt::granted`.
pub fn caps_arg(granted: &[(String, u32)]) -> String {
    let mut s = String::from("caps=");
    for (i, (name, _)) in granted.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(name);
        s.push('=');
        s.push_str(&alloc::format!("{i}"));
    }
    s
}
