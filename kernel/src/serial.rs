//! Minimal 16550 UART driver, used as the kernel's debug log.
//!
//! QEMU forwards COM1 to the host, so this is how we get output before the
//! framebuffer console exists — and the only output that survives a crash.

use core::fmt;
use spin::Mutex;
use x86_64::instructions::port::Port;

const COM1: u16 = 0x3F8;

pub struct SerialPort {
    base: u16,
}

impl SerialPort {
    const fn new(base: u16) -> Self {
        Self { base }
    }

    /// A lock-free handle to COM1, for the panic path where taking the global
    /// mutex could deadlock.
    pub const fn com1() -> Self {
        Self::new(COM1)
    }

    /// 38400 baud, 8N1, FIFOs on, interrupts off.
    pub fn init(&mut self) {
        unsafe {
            Port::<u8>::new(self.base + 1).write(0x00); // no interrupts
            Port::<u8>::new(self.base + 3).write(0x80); // DLAB on
            Port::<u8>::new(self.base).write(0x03); // divisor low
            Port::<u8>::new(self.base + 1).write(0x00); // divisor high
            Port::<u8>::new(self.base + 3).write(0x03); // DLAB off, 8N1
            Port::<u8>::new(self.base + 2).write(0xC7); // enable + clear FIFOs
            Port::<u8>::new(self.base + 4).write(0x0B); // RTS/DSR set
        }
    }

    fn can_transmit(&self) -> bool {
        unsafe { Port::<u8>::new(self.base + 5).read() & 0x20 != 0 }
    }

    /// Line status bit 0: a received byte is waiting.
    fn has_received(&self) -> bool {
        unsafe { Port::<u8>::new(self.base + 5).read() & 0x01 != 0 }
    }

    fn read_byte(&mut self) -> Option<u8> {
        if !self.has_received() {
            return None;
        }
        Some(unsafe { Port::<u8>::new(self.base).read() })
    }

    pub fn write_byte(&mut self, byte: u8) {
        while !self.can_transmit() {
            core::hint::spin_loop();
        }
        unsafe { Port::<u8>::new(self.base).write(byte) }
    }
}

impl fmt::Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            // The host expects CRLF line endings on a serial console.
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
        Ok(())
    }
}

pub static SERIAL: Mutex<SerialPort> = Mutex::new(SerialPort::new(COM1));

/// Where an incoming ANSI escape sequence has got to.
///
/// Terminals send arrow keys as `ESC [ A` and friends, so the bytes have to be
/// reassembled into the same key codes the PS/2 driver produces.
#[derive(Clone, Copy, PartialEq)]
enum Escape {
    None,
    Saw(u8),
    Bracket,
}

static ESCAPE: Mutex<Escape> = Mutex::new(Escape::None);

/// Read one keypress from the serial console, if any is waiting.
///
/// This is the only usable input on machines with no PS/2 controller — notably
/// Hyper-V Generation 2, whose keyboard is a synthetic VMBus device.
pub fn read() -> Option<u8> {
    use crate::keyboard::{KEY_DELETE, KEY_DOWN, KEY_END, KEY_HOME, KEY_LEFT, KEY_RIGHT, KEY_UP};

    x86_64::instructions::interrupts::without_interrupts(|| {
        let byte = SERIAL.lock().read_byte()?;
        let mut escape = ESCAPE.lock();

        match *escape {
            Escape::None => match byte {
                0x1B => {
                    *escape = Escape::Saw(0x1B);
                    None
                }
                // Terminals send CR for enter and DEL for backspace.
                b'\r' => Some(b'\n'),
                0x7F => Some(0x08),
                byte => Some(byte),
            },

            Escape::Saw(_) => {
                *escape = if byte == b'[' { Escape::Bracket } else { Escape::None };
                None
            }

            Escape::Bracket => {
                *escape = Escape::None;
                match byte {
                    b'A' => Some(KEY_UP),
                    b'B' => Some(KEY_DOWN),
                    b'C' => Some(KEY_RIGHT),
                    b'D' => Some(KEY_LEFT),
                    b'H' => Some(KEY_HOME),
                    b'F' => Some(KEY_END),
                    // `ESC [ 3 ~` is delete; swallow the trailing tilde by
                    // treating the digit as the end of the sequence.
                    b'3' => Some(KEY_DELETE),
                    _ => None,
                }
            }
        }
    })
}
