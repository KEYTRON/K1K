//! Legacy 8259A PIC pair. Limine hands us the PICs fully masked; we remap them
//! to vectors 32..48 and unmask only what we use.

use x86_64::instructions::port::Port;

const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;
const EOI: u8 = 0x20;

pub fn init(offset: u8) {
    unsafe {
        let mut p1c = Port::<u8>::new(PIC1_CMD);
        let mut p1d = Port::<u8>::new(PIC1_DATA);
        let mut p2c = Port::<u8>::new(PIC2_CMD);
        let mut p2d = Port::<u8>::new(PIC2_DATA);
        let mut wait = Port::<u8>::new(0x80);

        p1c.write(0x11);
        wait.write(0);
        p2c.write(0x11);
        wait.write(0);
        p1d.write(offset);
        wait.write(0);
        p2d.write(offset + 8);
        wait.write(0);
        p1d.write(4);
        wait.write(0);
        p2d.write(2);
        wait.write(0);
        p1d.write(0x01);
        wait.write(0);
        p2d.write(0x01);
        wait.write(0);

        p1d.write(0xFF);
        p2d.write(0xFF);
    }
}

pub fn unmask(irq: u8) {
    unsafe {
        let (port, bit) = if irq < 8 {
            (PIC1_DATA, irq)
        } else {
            (PIC2_DATA, irq - 8)
        };
        let mut p = Port::<u8>::new(port);
        let v = p.read();
        p.write(v & !(1 << bit));
        if irq >= 8 {
            let mut p1 = Port::<u8>::new(PIC1_DATA);
            let v1 = p1.read();
            p1.write(v1 & !(1 << 2));
        }
    }
}

pub fn eoi(irq: u8) {
    unsafe {
        if irq >= 8 {
            Port::<u8>::new(PIC2_CMD).write(EOI);
        }
        Port::<u8>::new(PIC1_CMD).write(EOI);
    }
}
