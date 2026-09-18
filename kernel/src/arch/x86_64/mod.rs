pub mod acpi;
pub mod apic;
pub mod context;
pub mod gdt;
pub mod idt;
pub mod interrupts;
pub mod pic;
pub mod pit;
pub mod serial;
pub mod syscall;

pub fn halt_loop() -> ! {
    loop {
        x86_64::instructions::hlt();
    }
}

/// QEMU `isa-debug-exit` device: exit code is `(value << 1) | 1`.
pub fn qemu_exit(code: u8) -> ! {
    unsafe {
        let mut port = x86_64::instructions::port::Port::<u32>::new(0xF4);
        port.write(code as u32);
    }
    halt_loop()
}

pub fn early_init() {
    serial::init();
    gdt::init();
    idt::init();
}
