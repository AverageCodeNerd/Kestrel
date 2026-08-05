//! A text terminal that can be driven by the kernel's own print stream.
//!
//! This is what lets the shell run inside a desktop window without touching a
//! single command implementation. Every command already writes with `print!`;
//! when capture is on, those bytes are staged here instead of going to the
//! framebuffer console, and the desktop drains them into a window.
//!
//! The shell's line editor does not append text, it *repaints* a line: carriage
//! return, prompt, contents, spaces over whatever the last line left behind,
//! then a walk back to the cursor. So this needs a real cursor with overwrite
//! semantics, not a list of finished lines. `print::move_left` feeds its
//! movement in as backspace bytes, which keeps everything a single stream.

use alloc::string::String;
use alloc::vec::Vec;
use spin::Mutex;

/// Bytes written since the last drain. `None` means capture is off, which is
/// also the fast path: no desktop, no staging, no allocation.
static PENDING: Mutex<Option<Vec<u8>>> = Mutex::new(None);

pub fn start_capture() {
    *PENDING.lock() = Some(Vec::new());
}

pub fn stop_capture() {
    *PENDING.lock() = None;
}

pub fn capturing() -> bool {
    PENDING.lock().is_some()
}

pub fn push(bytes: &[u8]) {
    if let Some(pending) = PENDING.lock().as_mut() {
        pending.extend_from_slice(bytes);
    }
}

/// Record a cursor movement as backspaces, so callers of `print::move_left`
/// land in the same byte stream as ordinary output.
pub fn push_move_left(count: usize) {
    if let Some(pending) = PENDING.lock().as_mut() {
        for _ in 0..count {
            pending.push(0x08);
        }
    }
}

/// Take everything staged so far.
///
/// The buffer is swapped out under the lock and processed after it is
/// released: anything that printed while this held the lock would deadlock,
/// and the whole point of this module is that printing happens everywhere.
pub fn drain() -> Option<Vec<u8>> {
    let mut pending = PENDING.lock();
    let staged = pending.as_mut()?;
    if staged.is_empty() {
        return None;
    }
    Some(core::mem::take(staged))
}

/// A grid of text with a cursor on the last line.
pub struct Terminal {
    lines: Vec<String>,
    /// Column of the cursor within the last line.
    cursor: usize,
    /// How much scrollback to keep. Beyond this, the oldest lines are dropped.
    limit: usize,
}

impl Terminal {
    pub fn new(limit: usize) -> Self {
        Self {
            lines: alloc::vec![String::new()],
            cursor: 0,
            limit,
        }
    }

    fn current(&mut self) -> &mut String {
        // There is always a line to write into.
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.lines.last_mut().unwrap()
    }

    pub fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match byte {
                b'\n' => {
                    self.lines.push(String::new());
                    self.cursor = 0;
                    if self.lines.len() > self.limit {
                        let excess = self.lines.len() - self.limit;
                        self.lines.drain(0..excess);
                    }
                }
                // Return to column zero without clearing: the line editor
                // relies on overwriting what is already there.
                b'\r' => self.cursor = 0,
                0x08 => self.cursor = self.cursor.saturating_sub(1),
                byte if byte.is_ascii_graphic() || byte == b' ' => {
                    let column = self.cursor;
                    // Only ASCII ever reaches a line, so a byte index is also a
                    // character index and `replace_range` cannot split a char.
                    let single = [byte];
                    let single = core::str::from_utf8(&single).unwrap_or(" ");
                    let line = self.current();
                    if column < line.len() {
                        line.replace_range(column..column + 1, single);
                    } else {
                        // Pad rather than assume the cursor is at the end;
                        // a bare `\r` then a move right would leave a gap.
                        while line.len() < column {
                            line.push(' ');
                        }
                        line.push(byte as char);
                    }
                    self.cursor = column + 1;
                }
                // Everything else (bells, stray control bytes) is ignored
                // rather than drawn as a glyph.
                _ => {}
            }
        }
    }

    /// The last `rows` lines, oldest first — what a window should display.
    pub fn visible(&self, rows: usize) -> Vec<String> {
        let start = self.lines.len().saturating_sub(rows.max(1));
        self.lines[start..].to_vec()
    }
}
