//! The 8254 PIT, used purely as a stopwatch to calibrate the APIC timer.
//!
//! Channel 2 is the one wired to the PC speaker, which makes it the only
//! channel whose output can be *polled* from software. That lets us measure a
//! precise interval before any interrupts exist.

use x86_64::instructions::port::Port;

/// The PIT's fixed input frequency, ~1.193182 MHz.
const PIT_FREQUENCY: u64 = 1_193_182;

const CHANNEL2_DATA: u16 = 0x42;
const COMMAND: u16 = 0x43;
/// Bit 0 gates channel 2, bit 1 drives the speaker, bit 5 reads its output.
const GATE_PORT: u16 = 0x61;

/// Busy-wait for roughly `micros` microseconds.
///
/// The speaker is explicitly held off throughout, so this is silent.
pub fn wait_micros(micros: u32) {
    let ticks = ((PIT_FREQUENCY * micros as u64) / 1_000_000).min(0xFFFF) as u16;

    unsafe {
        let mut gate = Port::<u8>::new(GATE_PORT);
        let mut command = Port::<u8>::new(COMMAND);
        let mut data = Port::<u8>::new(CHANNEL2_DATA);

        let original = gate.read();
        // Gate low and speaker off: the counter is loaded but held.
        gate.write(original & !0x03);

        // Channel 2, lobyte then hibyte, mode 0 (interrupt on terminal count).
        command.write(0xB0);
        data.write(ticks as u8);
        data.write((ticks >> 8) as u8);

        // Raising the gate starts the countdown.
        gate.write((original & !0x02) | 0x01);

        // Bit 5 reflects channel 2's output, which goes high at terminal count.
        while gate.read() & 0x20 == 0 {
            core::hint::spin_loop();
        }

        gate.write(original);
    }
}
