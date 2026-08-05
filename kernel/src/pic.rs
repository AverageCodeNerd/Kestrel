//! The legacy 8259 PIC — which we set up only in order to switch it off.
//!
//! On boot the PICs deliver IRQs on vectors 8..15, which collide with the CPU
//! exception vectors. Even when using the APIC instead, the PICs must be
//! remapped before being masked, because spurious interrupts can still slip
//! through and would otherwise look like a double fault.

use x86_64::instructions::port::Port;

const PIC1_COMMAND: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_COMMAND: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

const ICW1_INIT: u8 = 0x11; // init, expect ICW4
const ICW4_8086: u8 = 0x01;

/// Remap the PICs above the exception range, then mask every line.
pub fn disable() {
    unsafe {
        let mut pic1_cmd = Port::<u8>::new(PIC1_COMMAND);
        let mut pic1_data = Port::<u8>::new(PIC1_DATA);
        let mut pic2_cmd = Port::<u8>::new(PIC2_COMMAND);
        let mut pic2_data = Port::<u8>::new(PIC2_DATA);
        // Writing to an unused port burns a few hundred nanoseconds, which the
        // ancient 8259 needs between writes.
        let mut wait_port = Port::<u8>::new(0x80);
        let mut wait = || wait_port.write(0u8);

        pic1_cmd.write(ICW1_INIT);
        wait();
        pic2_cmd.write(ICW1_INIT);
        wait();

        pic1_data.write(0x20); // primary   -> vectors 0x20..0x27
        wait();
        pic2_data.write(0x28); // secondary -> vectors 0x28..0x2F
        wait();

        pic1_data.write(4); // secondary is on IRQ2
        wait();
        pic2_data.write(2); // ...and this is its cascade identity
        wait();

        pic1_data.write(ICW4_8086);
        wait();
        pic2_data.write(ICW4_8086);
        wait();

        // Mask everything: the APIC takes over from here.
        pic1_data.write(0xFF);
        pic2_data.write(0xFF);
    }
}
