//! Path handling for the file service.
//!
//! Paths are absolute, `/`-separated and case-insensitive (FAT 8.3 names).
//! `.` is skipped, `..` walks back towards the root and never above it, and a
//! component that does not fit 8.3 is rejected rather than truncated — opening
//! the wrong file because a name was shortened would be worse than failing.

use crate::fat::{BlockDevice, Fat, to_83};

/// Longest path the service accepts.
pub const MAX_PATH: usize = 255;
/// Deepest directory nesting a path may describe.
const MAX_DEPTH: usize = 8;

/// The last component of a path.
pub fn leaf_name(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// Resolve everything but the last component: returns the directory that holds
/// it (0 = the FAT root) and the 8.3 form of the last component.
pub fn split_parent(
    fs: &mut Fat,
    dev: &mut dyn BlockDevice,
    path: &str,
    scratch: &mut [u8],
) -> Result<(u32, [u8; 11]), ()> {
    if path.is_empty() || path.len() > MAX_PATH || !path.starts_with('/') {
        return Err(());
    }
    // (cluster, name) of each directory we walked through.
    let mut stack = [(0u32, [b' '; 11]); MAX_DEPTH];
    let mut depth = 0usize;
    let mut parts = path
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .peekable();
    while let Some(comp) = parts.next() {
        let last = parts.peek().is_none();
        if comp == ".." {
            depth = depth.saturating_sub(1);
            continue;
        }
        let leaf = to_83(comp).ok_or(())?;
        let dir = if depth == 0 { 0 } else { stack[depth - 1].0 };
        if last {
            return Ok((dir, leaf));
        }
        if depth >= MAX_DEPTH {
            return Err(());
        }
        let entry = fs.lookup(dev, dir, comp, scratch)?.ok_or(())?;
        if !entry.is_dir() {
            return Err(());
        }
        stack[depth] = (entry.cluster, leaf);
        depth += 1;
    }
    // The path was "/" or a chain of dots: there is no last component.
    Err(())
}
