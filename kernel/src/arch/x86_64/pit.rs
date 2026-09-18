//! 8253/8254 programmable interval timer, channel 0, rate generator.

use x86_64::instructions::port::Port;

const BASE_HZ: u32 = 1_193_182;

pub fn init(hz: u32) {
    let divisor = (BASE_HZ / hz).clamp(1, 65535) as u16;
    unsafe {
        Port::<u8>::new(0x43).write(0x36);
        let mut data = Port::<u8>::new(0x40);
        data.write((divisor & 0xFF) as u8);
        data.write((divisor >> 8) as u8);
    }
}
