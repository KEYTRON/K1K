//! Minimal NVMe driver: admin queue, one I/O queue pair, polled completions.
//! Everything talks to the controller through BAR0 (mapped by the kernel on
//! our behalf) and DMA pages whose physical addresses the kernel told us.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{Ordering, fence};

use k1k_rt::{Cap, Error, log, mem_create_dma, mem_map, mem_phys, recv, sleep_ms};

const REG_CAP: usize = 0x00;
const REG_VS: usize = 0x08;
const REG_CC: usize = 0x14;
const REG_CSTS: usize = 0x1C;
const REG_AQA: usize = 0x24;
const REG_ASQ: usize = 0x28;
const REG_ACQ: usize = 0x30;
const DOORBELL_BASE: usize = 0x1000;

const QUEUE_DEPTH: usize = 64;
const SQE_SIZE: usize = 64;
const CQE_SIZE: usize = 16;

const OPC_ADMIN_CREATE_IO_SQ: u8 = 0x01;
const OPC_ADMIN_CREATE_IO_CQ: u8 = 0x05;
const OPC_ADMIN_IDENTIFY: u8 = 0x06;
const OPC_IO_READ: u8 = 0x02;

pub struct DmaPage {
    pub virt: *mut u8,
    pub phys: u64,
}

pub fn dma_page() -> Result<DmaPage, Error> {
    let cap: Cap = mem_create_dma(1)?;
    let virt = mem_map(cap, true)?;
    let phys = mem_phys(cap)?;
    Ok(DmaPage { virt, phys })
}

struct Queue {
    sq: DmaPage,
    cq: DmaPage,
    depth: usize,
    sq_tail: usize,
    cq_head: usize,
    phase: u16,
    id: u16,
    next_cid: u16,
}

impl Queue {
    fn new(id: u16) -> Result<Self, Error> {
        Ok(Self {
            sq: dma_page()?,
            cq: dma_page()?,
            depth: QUEUE_DEPTH,
            sq_tail: 0,
            cq_head: 0,
            phase: 1,
            id,
            next_cid: 0,
        })
    }
}

pub struct Nvme {
    regs: *mut u8,
    stride: usize,
    admin: Queue,
    io: Option<Queue>,
    irq_ep: Option<Cap>,
    pub block_size: u32,
    pub blocks: u64,
    pub model: [u8; 40],
    pub serial: [u8; 20],
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // payloads are reported through Debug
pub enum NvmeError {
    Sys(Error),
    Timeout,
    Status(u16),
    NotReady,
}

impl From<Error> for NvmeError {
    fn from(e: Error) -> Self {
        NvmeError::Sys(e)
    }
}

impl Nvme {
    fn read32(&self, off: usize) -> u32 {
        unsafe { read_volatile(self.regs.add(off) as *const u32) }
    }
    fn read64(&self, off: usize) -> u64 {
        unsafe { read_volatile(self.regs.add(off) as *const u64) }
    }
    fn write32(&self, off: usize, v: u32) {
        unsafe { write_volatile(self.regs.add(off) as *mut u32, v) }
    }
    fn write64(&self, off: usize, v: u64) {
        unsafe { write_volatile(self.regs.add(off) as *mut u64, v) }
    }

    fn wait_ready(&self, ready: bool) -> Result<(), NvmeError> {
        for _ in 0..500 {
            let rdy = self.read32(REG_CSTS) & 1 != 0;
            if rdy == ready {
                return Ok(());
            }
            sleep_ms(10);
        }
        Err(NvmeError::Timeout)
    }

    /// `irq_ep`: endpoint that receives the controller's MSI-X vector 0; when
    /// absent, completions are polled.
    pub fn init(regs: *mut u8, irq_ep: Option<Cap>) -> Result<Self, NvmeError> {
        let mut dev = Self {
            regs,
            stride: 0,
            admin: Queue::new(0)?,
            io: None,
            irq_ep,
            block_size: 0,
            blocks: 0,
            model: [0; 40],
            serial: [0; 20],
        };
        let cap = dev.read64(REG_CAP);
        let vs = dev.read32(REG_VS);
        dev.stride = 4usize << ((cap >> 32) & 0xF);
        let mqes = (cap & 0xFFFF) as usize + 1;
        log!(
            "nvme {}.{}: CAP={:#x} MQES={} doorbell stride {}",
            vs >> 16,
            (vs >> 8) & 0xFF,
            cap,
            mqes,
            dev.stride
        );
        if mqes < QUEUE_DEPTH {
            dev.admin.depth = mqes;
        }

        // Reset, program the admin queues, enable.
        dev.write32(REG_CC, dev.read32(REG_CC) & !1);
        dev.wait_ready(false)?;
        let depth = dev.admin.depth as u32;
        dev.write32(REG_AQA, (depth - 1) << 16 | (depth - 1));
        dev.write64(REG_ASQ, dev.admin.sq.phys);
        dev.write64(REG_ACQ, dev.admin.cq.phys);
        // EN | CSS=NVM | MPS=0 (4 KiB) | AMS=RR | IOSQES=6 (64 B) | IOCQES=4 (16 B)
        dev.write32(REG_CC, 1 | 6 << 16 | 4 << 20);
        dev.wait_ready(true)?;

        // Identify controller.
        let ident = dma_page()?;
        let mut cmd = [0u32; 16];
        cmd[0] = OPC_ADMIN_IDENTIFY as u32;
        cmd[6] = ident.phys as u32;
        cmd[7] = (ident.phys >> 32) as u32;
        cmd[10] = 1; // CNS 1: controller
        dev.admin_cmd(&cmd)?;
        unsafe {
            core::ptr::copy_nonoverlapping(ident.virt.add(4), dev.serial.as_mut_ptr(), 20);
            core::ptr::copy_nonoverlapping(ident.virt.add(24), dev.model.as_mut_ptr(), 40);
        }

        // Identify namespace 1.
        let mut cmd = [0u32; 16];
        cmd[0] = OPC_ADMIN_IDENTIFY as u32;
        cmd[1] = 1; // NSID
        cmd[6] = ident.phys as u32;
        cmd[7] = (ident.phys >> 32) as u32;
        cmd[10] = 0; // CNS 0: namespace
        dev.admin_cmd(&cmd)?;
        let ns = ident.virt as *const u8;
        let nsze = unsafe { read_volatile(ns as *const u64) };
        let flbas = unsafe { read_volatile(ns.add(26)) } & 0xF;
        let lbaf = unsafe { read_volatile(ns.add(128 + 4 * flbas as usize) as *const u32) };
        let lbads = (lbaf >> 16) & 0xFF;
        dev.blocks = nsze;
        dev.block_size = 1u32 << lbads;

        // One I/O queue pair (CQ first, then SQ pointing at it).
        let io = Queue::new(1)?;
        let mut cmd = [0u32; 16];
        cmd[0] = OPC_ADMIN_CREATE_IO_CQ as u32;
        cmd[6] = io.cq.phys as u32;
        cmd[7] = (io.cq.phys >> 32) as u32;
        cmd[10] = ((io.depth as u32 - 1) << 16) | io.id as u32;
        // PC (physically contiguous); IEN when we have an interrupt endpoint (vector 0).
        cmd[11] = 1 | if dev.irq_ep.is_some() { 1 << 1 } else { 0 };
        dev.admin_cmd(&cmd)?;
        let mut cmd = [0u32; 16];
        cmd[0] = OPC_ADMIN_CREATE_IO_SQ as u32;
        cmd[6] = io.sq.phys as u32;
        cmd[7] = (io.sq.phys >> 32) as u32;
        cmd[10] = ((io.depth as u32 - 1) << 16) | io.id as u32;
        cmd[11] = (io.id as u32) << 16 | 1; // CQID, PC
        dev.admin_cmd(&cmd)?;
        dev.io = Some(io);
        Ok(dev)
    }

    fn submit(&mut self, admin: bool, cmd: &[u32; 16]) -> Result<u32, NvmeError> {
        let regs = self.regs;
        let stride = self.stride;
        let q = if admin {
            &mut self.admin
        } else {
            self.io.as_mut().ok_or(NvmeError::NotReady)?
        };
        let cid = q.next_cid;
        q.next_cid = q.next_cid.wrapping_add(1);

        let mut entry = *cmd;
        entry[0] = (entry[0] & 0xFF) | (cid as u32) << 16;
        unsafe {
            let slot = q.sq.virt.add(q.sq_tail * SQE_SIZE) as *mut u32;
            for (i, dw) in entry.iter().enumerate() {
                write_volatile(slot.add(i), *dw);
            }
        }
        q.sq_tail = (q.sq_tail + 1) % q.depth;
        fence(Ordering::SeqCst);
        let sq_db = DOORBELL_BASE + (2 * q.id as usize) * stride;
        unsafe { write_volatile(regs.add(sq_db) as *mut u32, q.sq_tail as u32) };

        // Wait for our completion: sleep on the interrupt endpoint when we
        // have one (a stale or coalesced interrupt just makes us re-check),
        // otherwise spin.
        let irq_ep = self.irq_ep;
        let mut idle_spins = 0u32;
        loop {
            let cqe = unsafe { q.cq.virt.add(q.cq_head * CQE_SIZE) as *const u32 };
            let dw3 = unsafe { read_volatile(cqe.add(3)) };
            let phase = ((dw3 >> 16) & 1) as u16;
            if phase != q.phase {
                match irq_ep {
                    Some(ep) => {
                        if recv(ep).is_err() {
                            return Err(NvmeError::Timeout);
                        }
                    }
                    None => {
                        idle_spins += 1;
                        if idle_spins > 2_000_000 {
                            return Err(NvmeError::Timeout);
                        }
                        core::hint::spin_loop();
                    }
                }
                continue;
            }
            {
                let result = unsafe { read_volatile(cqe) };
                let status = (dw3 >> 17) as u16;
                let got_cid = (dw3 & 0xFFFF) as u16;
                q.cq_head += 1;
                if q.cq_head == q.depth {
                    q.cq_head = 0;
                    q.phase ^= 1;
                }
                let cq_db = DOORBELL_BASE + (2 * q.id as usize + 1) * stride;
                unsafe { write_volatile(regs.add(cq_db) as *mut u32, q.cq_head as u32) };
                if got_cid != cid {
                    log!("nvme: unexpected cid {} (wanted {})", got_cid, cid);
                }
                return if status == 0 {
                    Ok(result)
                } else {
                    Err(NvmeError::Status(status))
                };
            }
        }
    }

    fn admin_cmd(&mut self, cmd: &[u32; 16]) -> Result<u32, NvmeError> {
        self.submit(true, cmd)
    }

    /// Read `count` blocks starting at `lba` into physically contiguous memory
    /// at `phys`. Two PRP entries cover up to 8 KiB, so `count * block_size`
    /// must not exceed that.
    pub fn read_phys(&mut self, lba: u64, count: u16, phys: u64) -> Result<(), NvmeError> {
        let bytes = count as u64 * self.block_size as u64;
        if bytes == 0 || bytes > 8192 {
            return Err(NvmeError::NotReady);
        }
        let mut cmd = [0u32; 16];
        cmd[0] = OPC_IO_READ as u32;
        cmd[1] = 1; // NSID
        cmd[6] = phys as u32;
        cmd[7] = (phys >> 32) as u32;
        if bytes > 4096 {
            let prp2 = (phys & !0xFFF) + 4096;
            cmd[8] = prp2 as u32;
            cmd[9] = (prp2 >> 32) as u32;
        }
        cmd[10] = lba as u32;
        cmd[11] = (lba >> 32) as u32;
        cmd[12] = (count as u32 - 1) & 0xFFFF;
        self.submit(false, &cmd).map(|_| ())
    }
}
