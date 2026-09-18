//! 8254 PIT, used only as a calibration reference for the local APIC timer:
//! channel 2 in one-shot mode, gated and read back through port 0x61.

use x86_64::instructions::port::Port;

const BASE_HZ: u32 = 1_193_182;

/// Spin for `ms` milliseconds (ms ≤ 50) using PIT channel 2.
pub fn busy_wait_ms(ms: u32) {
    let count = (BASE_HZ / 1000 * ms).min(0xFFFF) as u16;
    unsafe {
        let mut gate = Port::<u8>::new(0x61);
        let mut cmd = Port::<u8>::new(0x43);
        let mut ch2 = Port::<u8>::new(0x42);

        // Gate low, speaker off.
        let g = gate.read() & !0x03;
        gate.write(g);
        // Channel 2, lo/hi byte, mode 0 (interrupt on terminal count).
        cmd.write(0xB0);
        ch2.write((count & 0xFF) as u8);
        ch2.write((count >> 8) as u8);
        // Gate high starts the countdown.
        gate.write(g | 0x01);
        while gate.read() & 0x20 == 0 {
            core::hint::spin_loop();
        }
        gate.write(g);
    }
}
