//! System call dispatcher and user-memory access helpers.

use alloc::vec::Vec;
use x86_64::VirtAddr;

use crate::arch::x86_64::interrupts as irq;
use crate::ipc::{IpcError, Message};
use crate::mm::pmm::{FRAME_SIZE, phys_to_virt};
use crate::obj::{Capability, MemoryObject, Object, Rights};
use crate::{klog, sched};

pub const SYS_LOG: u64 = 0;
pub const SYS_EXIT: u64 = 1;
pub const SYS_YIELD: u64 = 2;
pub const SYS_SLEEP: u64 = 3;
pub const SYS_SEND: u64 = 4;
pub const SYS_RECV: u64 = 5;
pub const SYS_INFO: u64 = 6;
pub const SYS_SEND_CAP: u64 = 7;
pub const SYS_CAP_DROP: u64 = 8;
pub const SYS_MEM_CREATE: u64 = 9;
pub const SYS_MEM_MAP: u64 = 10;

pub const EPERM: i64 = -1;
pub const EAGAIN: i64 = -2;
pub const EFAULT: i64 = -3;
pub const EINVAL: i64 = -4;
pub const ENOSYS: i64 = -5;
pub const ENOMEM: i64 = -6;

/// `recv` reports "no capability attached" in word 3 with this value.
pub const NO_CAP: u64 = u64::MAX;

const USER_TOP: u64 = 0x0000_8000_0000_0000;
const MAX_LOG: u64 = 4096;
const MAX_MEM_PAGES: u64 = 1024;

fn user_range_ok(ptr: u64, len: u64) -> bool {
    len <= MAX_LOG && ptr.checked_add(len).is_some_and(|end| end <= USER_TOP)
}

/// Translate a user virtual address through the current task's page tables.
fn translate_user(va: u64) -> Option<u64> {
    sched::with_current(|t| {
        t.addr_space
            .as_ref()?
            .translate(VirtAddr::new(va))
            .map(|p| p.as_u64())
    })
}

fn copy_from_user(ptr: u64, len: u64) -> Result<Vec<u8>, i64> {
    if !user_range_ok(ptr, len) {
        return Err(EFAULT);
    }
    let mut out = Vec::with_capacity(len as usize);
    let mut va = ptr;
    let end = ptr + len;
    while va < end {
        let pa = translate_user(va).ok_or(EFAULT)?;
        let chunk = (FRAME_SIZE - (va % FRAME_SIZE)).min(end - va) as usize;
        let src = phys_to_virt(x86_64::PhysAddr::new(pa)).as_ptr::<u8>();
        out.extend_from_slice(unsafe { core::slice::from_raw_parts(src, chunk) });
        va += chunk as u64;
    }
    Ok(out)
}

fn copy_to_user(ptr: u64, data: &[u8]) -> Result<(), i64> {
    if !user_range_ok(ptr, data.len() as u64) {
        return Err(EFAULT);
    }
    let mut off = 0usize;
    while off < data.len() {
        let va = ptr + off as u64;
        let pa = translate_user(va).ok_or(EFAULT)?;
        let chunk = (FRAME_SIZE - (va % FRAME_SIZE)) as usize;
        let chunk = chunk.min(data.len() - off);
        let dst = phys_to_virt(x86_64::PhysAddr::new(pa)).as_mut_ptr::<u8>();
        unsafe { core::ptr::copy_nonoverlapping(data[off..].as_ptr(), dst, chunk) };
        off += chunk;
    }
    Ok(())
}

fn words_to_bytes(words: &[u64]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

fn lookup_endpoint(slot: u64, rights: Rights) -> Option<alloc::sync::Arc<crate::ipc::Endpoint>> {
    sched::with_current(|t| t.caps.lookup(slot as u32, rights)?.endpoint().cloned())
}

fn sys_log(ptr: u64, len: u64) -> i64 {
    match copy_from_user(ptr, len) {
        Ok(bytes) => {
            let name = sched::with_current(|t| t.name);
            let s = core::str::from_utf8(&bytes).unwrap_or("<invalid utf-8>");
            klog!(name, "{}", s);
            0
        }
        Err(e) => e,
    }
}

fn deliver(ep: &crate::ipc::Endpoint, msg: Message) -> i64 {
    match ep.send(msg) {
        Ok(()) => 0,
        Err(IpcError::QueueFull) => EAGAIN,
    }
}

fn sys_send(slot: u64, w0: u64, w1: u64, w2: u64) -> i64 {
    let Some(ep) = lookup_endpoint(slot, Rights::SEND) else {
        return EPERM;
    };
    deliver(&ep, Message::new(sched::current_id(), [w0, w1, w2, NO_CAP]))
}

/// Send `w0` plus a copy of capability `cap_slot` (rights ∩ `mask`) over `ep_slot`.
fn sys_send_cap(ep_slot: u64, cap_slot: u64, mask: u64, w0: u64) -> i64 {
    let Some(ep) = lookup_endpoint(ep_slot, Rights::SEND) else {
        return EPERM;
    };
    let Some(cap) = sched::with_current(|t| t.caps.derive(cap_slot as u32, Rights(mask as u32)))
    else {
        return EPERM;
    };
    let mut msg = Message::new(sched::current_id(), [w0, 0, 0, NO_CAP]);
    msg.cap = Some(cap);
    deliver(&ep, msg)
}

fn sys_recv(slot: u64, buf: u64) -> i64 {
    let Some(ep) = lookup_endpoint(slot, Rights::RECV) else {
        return EPERM;
    };
    if !user_range_ok(buf, 32) {
        return EFAULT;
    }
    let mut msg = ep.recv();
    msg.words[3] = match msg.cap.take() {
        Some(cap) => sched::with_current(|t| t.caps.insert(cap))
            .map(|s| s as u64)
            .unwrap_or(NO_CAP),
        None => NO_CAP,
    };
    match copy_to_user(buf, &words_to_bytes(&msg.words)) {
        Ok(()) => msg.sender as i64,
        Err(e) => e,
    }
}

fn sys_cap_drop(slot: u64) -> i64 {
    match sched::with_current(|t| t.caps.remove(slot as u32)) {
        Some(_) => 0,
        None => EINVAL,
    }
}

fn sys_mem_create(pages: u64) -> i64 {
    if pages == 0 || pages > MAX_MEM_PAGES {
        return EINVAL;
    }
    let Some(obj) = MemoryObject::new(pages as usize) else {
        return ENOMEM;
    };
    let cap = Capability {
        object: Object::Memory(obj),
        rights: Rights::MAP_READ
            .union(Rights::MAP_WRITE)
            .union(Rights::GRANT),
    };
    match sched::with_current(|t| t.caps.insert(cap)) {
        Some(slot) => slot as i64,
        None => ENOMEM,
    }
}

fn sys_mem_map(slot: u64, writable: u64) -> i64 {
    let writable = writable != 0;
    let need = if writable {
        Rights::MAP_READ.union(Rights::MAP_WRITE)
    } else {
        Rights::MAP_READ
    };
    let Some(obj) = sched::with_current(|t| t.caps.lookup(slot as u32, need)?.memory().cloned())
    else {
        return EPERM;
    };
    let mapped = sched::with_current(|t| t.addr_space.as_mut()?.map_shared(obj, writable));
    match mapped {
        Some(va) => va.as_u64() as i64,
        None => ENOMEM,
    }
}

fn sys_info(buf: u64) -> i64 {
    let words = [irq::uptime_ms(), sched::current_id() as u64];
    match copy_to_user(buf, &words_to_bytes(&words)) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn dispatch(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    match nr {
        SYS_LOG => sys_log(a0, a1),
        SYS_EXIT => sched::exit_current(a0 as i64),
        SYS_YIELD => {
            sched::yield_now();
            0
        }
        SYS_SLEEP => {
            sched::sleep_ms(a0.min(60_000));
            0
        }
        SYS_SEND => sys_send(a0, a1, a2, a3),
        SYS_RECV => sys_recv(a0, a1),
        SYS_INFO => sys_info(a0),
        SYS_SEND_CAP => sys_send_cap(a0, a1, a2, a3),
        SYS_CAP_DROP => sys_cap_drop(a0),
        SYS_MEM_CREATE => sys_mem_create(a0),
        SYS_MEM_MAP => sys_mem_map(a0, a1),
        _ => {
            let name = sched::with_current(|t| t.name);
            klog!("sys", "task '{}' invoked unknown syscall {}", name, nr);
            ENOSYS
        }
    }
}
