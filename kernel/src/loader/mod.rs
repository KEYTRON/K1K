//! Static ELF64 loader for ring-3 services.

use x86_64::VirtAddr;

use crate::mm::pmm::FRAME_SIZE;
use crate::mm::vmm::{AddressSpace, Flags};

const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 0x3E;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;

/// Lowest address a segment may occupy; keeps the null page and the first
/// 4 MiB unmapped so wild pointers fault instead of hitting code.
const USER_MIN: u64 = 0x0040_0000;
const USER_MAX: u64 = 0x0000_7fff_0000_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadError {
    BadMagic,
    Unsupported,
    Truncated,
    BadSegment,
    OutOfMemory,
}

pub struct LoadedImage {
    pub entry: u64,
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

/// Map and copy every `PT_LOAD` segment of `image` into `asp`.
pub fn load(asp: &mut AddressSpace, image: &[u8]) -> Result<LoadedImage, LoadError> {
    if image.len() < 64 || image[..4] != ELF_MAGIC {
        return Err(LoadError::BadMagic);
    }
    if image[4] != ELFCLASS64 || image[5] != ELFDATA2LSB {
        return Err(LoadError::Unsupported);
    }
    let e_type = u16_at(image, 16).ok_or(LoadError::Truncated)?;
    let e_machine = u16_at(image, 18).ok_or(LoadError::Truncated)?;
    if e_type != ET_EXEC || e_machine != EM_X86_64 {
        return Err(LoadError::Unsupported);
    }
    let entry = u64_at(image, 24).ok_or(LoadError::Truncated)?;
    let phoff = u64_at(image, 32).ok_or(LoadError::Truncated)? as usize;
    let phentsize = u16_at(image, 54).ok_or(LoadError::Truncated)? as usize;
    let phnum = u16_at(image, 56).ok_or(LoadError::Truncated)? as usize;
    if phentsize < 56 {
        return Err(LoadError::Unsupported);
    }

    let mut segments = 0;
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        let p_type = u32_at(image, ph).ok_or(LoadError::Truncated)?;
        if p_type != PT_LOAD {
            continue;
        }
        let p_flags = u32_at(image, ph + 4).ok_or(LoadError::Truncated)?;
        let p_offset = u64_at(image, ph + 8).ok_or(LoadError::Truncated)?;
        let p_vaddr = u64_at(image, ph + 16).ok_or(LoadError::Truncated)?;
        let p_filesz = u64_at(image, ph + 32).ok_or(LoadError::Truncated)?;
        let p_memsz = u64_at(image, ph + 40).ok_or(LoadError::Truncated)?;

        if p_memsz == 0 {
            continue;
        }
        let end = p_vaddr.checked_add(p_memsz).ok_or(LoadError::BadSegment)?;
        if p_vaddr < USER_MIN || end > USER_MAX || p_filesz > p_memsz {
            return Err(LoadError::BadSegment);
        }
        let file_end = p_offset
            .checked_add(p_filesz)
            .ok_or(LoadError::BadSegment)? as usize;
        if file_end > image.len() {
            return Err(LoadError::Truncated);
        }

        let mut flags = Flags::empty();
        if p_flags & PF_W != 0 {
            flags |= Flags::WRITABLE;
        }
        if p_flags & PF_X == 0 {
            flags |= Flags::NO_EXECUTE;
        }

        let first_page = p_vaddr & !(FRAME_SIZE - 1);
        let pages = ((end - first_page) as usize).div_ceil(FRAME_SIZE as usize);
        asp.map_user_range(VirtAddr::new(first_page), pages, flags)
            .map_err(|_| LoadError::OutOfMemory)?;
        asp.write_user(VirtAddr::new(p_vaddr), &image[p_offset as usize..file_end]);
        segments += 1;
    }

    if segments == 0 || entry < USER_MIN || entry >= USER_MAX {
        return Err(LoadError::BadSegment);
    }
    Ok(LoadedImage { entry })
}
