//! `print!` / `println!` that fan out to both the serial log and the
//! framebuffer console (the latter only once it has been initialised).

use core::fmt::{self, Write};

use crate::console::CONSOLE;
use crate::serial::SERIAL;
use crate::terminal;

/// Feeds formatted output into the terminal capture buffer a piece at a time,
/// so nothing has to be allocated into a `String` first.
struct Capture;

impl Write for Capture {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        terminal::push(text.as_bytes());
        Ok(())
    }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    // Masking interrupts here is what makes printing safe under preemption:
    // otherwise a task could be switched out mid-print while holding these
    // locks, and the next task to print would spin on them forever.
    x86_64::instructions::interrupts::without_interrupts(|| {
        // Serial first: if drawing to the framebuffer faults, the log still
        // made it out to the host.
        SERIAL.lock().write_fmt(args).ok();

        // With a terminal window open, the desktop owns the framebuffer and
        // the output belongs in that window. Writing to the text console as
        // well would paint over the compositor and force a full repaint on
        // every line.
        if terminal::capturing() {
            Capture.write_fmt(args).ok();
            return;
        }

        if let Some(console) = CONSOLE.lock().as_mut() {
            console.write_fmt(args).ok();
            // The console paints the framebuffer directly. If the compositor is
            // up, it now owns that surface and only repaints what it thinks
            // changed, so tell it the screen was disturbed underneath it.
            crate::desktop::note_screen_disturbed();
        }
    });
}

/// Move the cursor left `count` places, leaving the text alone.
pub fn move_left(count: usize) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut serial = SERIAL.lock();
        for _ in 0..count {
            // A bare backspace moves without erasing on a terminal.
            serial.write_byte(0x08);
        }
        drop(serial);

        // Cursor movement is part of the same byte stream as the text, so the
        // terminal window can reproduce the line editor's repaints exactly.
        if terminal::capturing() {
            terminal::push_move_left(count);
            return;
        }

        if let Some(console) = CONSOLE.lock().as_mut() {
            console.move_left(count);
        }
    });
}

/// Draw the Kestrel mark on the framebuffer, if there is one.
pub fn logo(size: usize) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        if let Some(console) = CONSOLE.lock().as_mut() {
            console.draw_logo(size);
        }
    });
}

pub fn clear() {
    x86_64::instructions::interrupts::without_interrupts(|| {
        // With a terminal window open the console is not what anyone is
        // looking at, so clearing it would appear to do nothing. A form feed
        // carries the instruction along the same byte stream as the text, so
        // it arrives in order with it rather than racing ahead.
        if terminal::capturing() {
            terminal::push(&[0x0C]);
            return;
        }

        if let Some(console) = CONSOLE.lock().as_mut() {
            console.clear();
        }
    });
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::print::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}
