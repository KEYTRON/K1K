//! Legacy 8259A PICs. We run on the APIC; the PICs are remapped away from the
//! exception vectors (so a stray IRQ cannot masquerade as a fault) and masked.

use x86_64::instructions::port::Port;

pub fn disable() {
    unsafe {
        let mut p1c = Port::<u8>::new(0x20);
        let mut p1d = Port::<u8>::new(0x21);
        let mut p2c = Port::<u8>::new(0xA0);
        let mut p2d = Port::<u8>::new(0xA1);

        p1c.write(0x11);
        p2c.write(0x11);
        p1d.write(0xF0);
        p2d.write(0xF8);
        p1d.write(4);
        p2d.write(2);
        p1d.write(0x01);
        p2d.write(0x01);

        p1d.write(0xFF);
        p2d.write(0xFF);
    }
}
