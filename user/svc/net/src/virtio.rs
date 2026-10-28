//! A modern (virtio 1.x) PCI network device, driven by polling. The register
//! windows are found by walking the vendor capabilities in configuration
//! space; everything else is plain MMIO and DMA pages whose physical
//! addresses the kernel told us.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{Ordering, fence};

use k1k_rt::{Cap, Error, dev_config, dev_map, log, mem_create_dma, mem_map, mem_phys, sleep_ms};

const CAP_COMMON: u8 = 1;
const CAP_NOTIFY: u8 = 2;
const CAP_DEVICE: u8 = 4;

const ST_ACK: u8 = 1;
const ST_DRIVER: u8 = 2;
const ST_DRIVER_OK: u8 = 4;
const ST_FEATURES_OK: u8 = 8;

const F_MAC: u32 = 1 << 5;
const F_VERSION_1: u32 = 1 << 0; // bit 32, in the second feature word

const QSIZE: usize = 16;
const BUF: usize = 2048;
/// virtio-net header with `VIRTIO_F_VERSION_1`: flags, gso, hdr_len, gso_size,
/// csum_start, csum_offset, num_buffers.
pub const HDR: usize = 12;

const DESC_WRITE: u16 = 2;

struct Dma {
    virt: *mut u8,
    phys: u64,
}

fn dma(pages: usize) -> Result<Dma, Error> {
    let cap = mem_create_dma(pages)?;
    Ok(Dma {
        virt: mem_map(cap, true)?,
        phys: mem_phys(cap)?,
    })
}

struct Queue {
    index: u16,
    ring: Dma,
    bufs: Dma,
    notify: *mut u16,
    last_used: u16,
    avail_idx: u16,
}

// Ring layout inside one page: descriptors, available ring, used ring.
const OFF_AVAIL: usize = 256;
const OFF_USED: usize = 512;

impl Queue {
    fn rd16(&self, off: usize) -> u16 {
        unsafe { read_volatile(self.ring.virt.add(off) as *const u16) }
    }
    fn wr16(&self, off: usize, v: u16) {
        unsafe { write_volatile(self.ring.virt.add(off) as *mut u16, v) }
    }
    fn desc(&self, i: usize, addr: u64, len: u32, flags: u16) {
        let d = unsafe { self.ring.virt.add(i * 16) };
        unsafe {
            write_volatile(d as *mut u64, addr);
            write_volatile(d.add(8) as *mut u32, len);
            write_volatile(d.add(12) as *mut u16, flags);
            write_volatile(d.add(14) as *mut u16, 0);
        }
    }
    fn buf(&self, i: usize) -> *mut u8 {
        unsafe { self.bufs.virt.add(i * BUF) }
    }
    /// Make descriptor `i` available to the device.
    fn post(&mut self, i: usize) {
        self.wr16(
            OFF_AVAIL + 4 + 2 * (self.avail_idx as usize % QSIZE),
            i as u16,
        );
        fence(Ordering::SeqCst);
        self.avail_idx = self.avail_idx.wrapping_add(1);
        self.wr16(OFF_AVAIL + 2, self.avail_idx);
        fence(Ordering::SeqCst);
        unsafe { write_volatile(self.notify, self.index) };
    }
    /// One completed buffer: (descriptor, length written by the device).
    fn pop(&mut self) -> Option<(usize, usize)> {
        fence(Ordering::SeqCst);
        if self.rd16(OFF_USED + 2) == self.last_used {
            return None;
        }
        let slot = OFF_USED + 4 + 8 * (self.last_used as usize % QSIZE);
        let (id, len) = unsafe {
            (
                read_volatile(self.ring.virt.add(slot) as *const u32),
                read_volatile(self.ring.virt.add(slot + 4) as *const u32),
            )
        };
        self.last_used = self.last_used.wrapping_add(1);
        Some((id as usize, len as usize))
    }
}

pub struct Nic {
    pub mac: [u8; 6],
    rx: Queue,
    tx: Queue,
}

fn rd32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

impl Nic {
    pub fn init(dev: Cap) -> Result<Nic, Error> {
        let cfg = dev_config(dev)?;
        // (bar, offset) of each virtio window, plus the notify multiplier.
        let mut win: [Option<(usize, usize)>; 5] = [None; 5];
        let mut mult = 0usize;
        let mut ptr = cfg[0x34] as usize & 0xFC;
        for _ in 0..48 {
            if ptr == 0 || ptr + 16 > 256 {
                break;
            }
            if cfg[ptr] == 0x09 {
                let kind = cfg[ptr + 3] as usize;
                if kind < win.len() {
                    win[kind] = Some((cfg[ptr + 4] as usize, rd32(&cfg, ptr + 8) as usize));
                    if kind == CAP_NOTIFY as usize && ptr + 20 <= 256 {
                        mult = rd32(&cfg, ptr + 16) as usize;
                    }
                }
            }
            ptr = cfg[ptr + 1] as usize & 0xFC;
        }
        let (Some(common), Some(notify), Some(device)) = (
            win[CAP_COMMON as usize],
            win[CAP_NOTIFY as usize],
            win[CAP_DEVICE as usize],
        ) else {
            log!("not a modern virtio device (missing capabilities)");
            return Err(Error::Inval);
        };
        let mut bars: [Option<*mut u8>; 6] = [None; 6];
        let mut at = |bar: usize| -> Result<*mut u8, Error> {
            if bars[bar].is_none() {
                bars[bar] = Some(dev_map(dev, bar)?);
            }
            Ok(bars[bar].unwrap())
        };
        let c = unsafe { at(common.0)?.add(common.1) };
        let nbase = unsafe { at(notify.0)?.add(notify.1) };
        let dcfg = unsafe { at(device.0)?.add(device.1) };

        let r8 = |o: usize| unsafe { read_volatile(c.add(o)) };
        let w8 = |o: usize, v: u8| unsafe { write_volatile(c.add(o), v) };
        let r16 = |o: usize| unsafe { read_volatile(c.add(o) as *const u16) };
        let w16 = |o: usize, v: u16| unsafe { write_volatile(c.add(o) as *mut u16, v) };
        let r32 = |o: usize| unsafe { read_volatile(c.add(o) as *const u32) };
        let w32 = |o: usize, v: u32| unsafe { write_volatile(c.add(o) as *mut u32, v) };
        let w64 = |o: usize, v: u64| unsafe { write_volatile(c.add(o) as *mut u64, v) };

        w8(0x14, 0); // reset
        while r8(0x14) != 0 {
            sleep_ms(1);
        }
        w8(0x14, ST_ACK);
        w8(0x14, ST_ACK | ST_DRIVER);

        w32(0x00, 0);
        let lo = r32(0x04);
        w32(0x00, 1);
        let hi = r32(0x04);
        if hi & F_VERSION_1 == 0 {
            log!("device offers no VIRTIO_F_VERSION_1");
            return Err(Error::Inval);
        }
        w32(0x08, 0);
        w32(0x0C, lo & F_MAC);
        w32(0x08, 1);
        w32(0x0C, F_VERSION_1);
        w8(0x14, ST_ACK | ST_DRIVER | ST_FEATURES_OK);
        if r8(0x14) & ST_FEATURES_OK == 0 {
            log!("device rejected the feature set");
            return Err(Error::Inval);
        }

        let mut mk = |index: u16| -> Result<Queue, Error> {
            w16(0x16, index);
            if r16(0x18) < QSIZE as u16 {
                return Err(Error::Inval);
            }
            w16(0x18, QSIZE as u16);
            let ring = dma(1)?;
            let bufs = dma(QSIZE * BUF / 4096)?;
            unsafe { core::ptr::write_bytes(ring.virt, 0, 4096) };
            w64(0x20, ring.phys);
            w64(0x28, ring.phys + OFF_AVAIL as u64);
            w64(0x30, ring.phys + OFF_USED as u64);
            let off = r16(0x1E) as usize;
            w16(0x1C, 1);
            Ok(Queue {
                index,
                ring,
                bufs,
                notify: unsafe { nbase.add(off * mult) } as *mut u16,
                last_used: 0,
                avail_idx: 0,
            })
        };
        let mut rx = mk(0)?;
        let tx = mk(1)?;
        for i in 0..QSIZE {
            rx.desc(i, rx.bufs.phys + (i * BUF) as u64, BUF as u32, DESC_WRITE);
        }
        for i in 0..QSIZE {
            rx.post(i);
        }
        w8(0x14, ST_ACK | ST_DRIVER | ST_FEATURES_OK | ST_DRIVER_OK);

        let mut mac = [0u8; 6];
        for (i, m) in mac.iter_mut().enumerate() {
            *m = unsafe { read_volatile(dcfg.add(i)) };
        }
        Ok(Nic { mac, rx, tx })
    }

    /// Send one Ethernet frame; waits for the device to take it.
    pub fn send(&mut self, frame: &[u8]) -> bool {
        if frame.len() + HDR > BUF {
            return false;
        }
        let b = self.tx.buf(0);
        unsafe {
            core::ptr::write_bytes(b, 0, HDR);
            core::ptr::copy_nonoverlapping(frame.as_ptr(), b.add(HDR), frame.len());
        }
        self.tx
            .desc(0, self.tx.bufs.phys, (HDR + frame.len()) as u32, 0);
        self.tx.post(0);
        for _ in 0..1000 {
            if self.tx.pop().is_some() {
                return true;
            }
            sleep_ms(1);
        }
        false
    }

    /// Copy one received frame into `out`; returns its length.
    pub fn recv(&mut self, out: &mut [u8]) -> Option<usize> {
        let (id, len) = self.rx.pop()?;
        let n = len.saturating_sub(HDR).min(out.len());
        unsafe {
            core::ptr::copy_nonoverlapping(self.rx.buf(id).add(HDR), out.as_mut_ptr(), n);
        }
        self.rx.post(id);
        Some(n)
    }
}
