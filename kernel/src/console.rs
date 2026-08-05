//! Text console drawn directly into the framebuffer Limine hands us.

use core::fmt;
use limine::framebuffer::Framebuffer;
use spin::Mutex;

use crate::font::{self, GLYPH_HEIGHT, GLYPH_WIDTH};

/// Glyphs are 8x8, which is unreadably small at 1024x768, so double them.
const SCALE: usize = 2;

const CELL_W: usize = GLYPH_WIDTH * SCALE;
/// One extra font row of leading, or lines with descenders run together.
const CELL_H: usize = (GLYPH_HEIGHT + 1) * SCALE;

/// Left/top inset so text isn't jammed against the bezel.
const MARGIN: usize = 8;

pub struct Console {
    base: *mut u8,
    width: usize,
    height: usize,
    pitch: usize,
    bytes_per_pixel: usize,
    red_shift: u8,
    green_shift: u8,
    blue_shift: u8,
    cols: usize,
    rows: usize,
    col: usize,
    row: usize,
    fg: u32,
    bg: u32,
}

// The framebuffer is a fixed MMIO region owned by this struct; access is
// serialised by the Mutex around it.
unsafe impl Send for Console {}

impl Console {
    /// # Safety
    /// `fb` must be a framebuffer description returned by the bootloader, and
    /// only one `Console` may exist for it.
    pub unsafe fn new(fb: &Framebuffer) -> Self {
        let width = fb.width as usize;
        let height = fb.height as usize;
        let mut console = Self {
            base: fb.address() as *mut u8,
            width,
            height,
            pitch: fb.pitch as usize,
            bytes_per_pixel: (fb.bpp as usize).div_ceil(8),
            red_shift: fb.red_mask_shift,
            green_shift: fb.green_mask_shift,
            blue_shift: fb.blue_mask_shift,
            cols: (width - 2 * MARGIN) / CELL_W,
            rows: (height - 2 * MARGIN) / CELL_H,
            col: 0,
            row: 0,
            fg: 0,
            bg: 0,
        };
        console.fg = console.rgb(0xD0, 0xD6, 0xE0);
        console.bg = console.rgb(0x0A, 0x0C, 0x12);
        console.clear();
        console
    }

    /// Pack an RGB triple according to the framebuffer's channel layout.
    pub fn rgb(&self, r: u8, g: u8, b: u8) -> u32 {
        (r as u32) << self.red_shift
            | (g as u32) << self.green_shift
            | (b as u32) << self.blue_shift
    }

    pub fn set_fg(&mut self, r: u8, g: u8, b: u8) {
        self.fg = self.rgb(r, g, b);
    }

    #[inline]
    fn put_pixel(&mut self, x: usize, y: usize, colour: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let offset = y * self.pitch + x * self.bytes_per_pixel;
        unsafe {
            // Framebuffers are not guaranteed to be 4-byte aligned at every
            // offset, so write the channel bytes individually.
            let px = self.base.add(offset);
            for byte in 0..self.bytes_per_pixel.min(4) {
                px.add(byte).write_volatile((colour >> (byte * 8)) as u8);
            }
        }
    }

    pub fn clear(&mut self) {
        let bg = self.bg;
        for y in 0..self.height {
            for x in 0..self.width {
                self.put_pixel(x, y, bg);
            }
        }
        self.col = 0;
        self.row = 0;
    }

    fn draw_glyph(&mut self, c: u8, col: usize, row: usize) {
        let glyph = font::glyph(c);
        let origin_x = MARGIN + col * CELL_W;
        let origin_y = MARGIN + row * CELL_H;
        let (fg, bg) = (self.fg, self.bg);

        for (gy, bits) in glyph.iter().enumerate() {
            for gx in 0..GLYPH_WIDTH {
                // Bit 0 is the leftmost pixel of the row.
                let colour = if bits & (1 << gx) != 0 { fg } else { bg };
                for sy in 0..SCALE {
                    for sx in 0..SCALE {
                        self.put_pixel(
                            origin_x + gx * SCALE + sx,
                            origin_y + gy * SCALE + sy,
                            colour,
                        );
                    }
                }
            }
        }
    }

    /// Move every text row up by one and blank the last one.
    fn scroll(&mut self) {
        let row_bytes = CELL_H * self.pitch;
        let top = MARGIN * self.pitch;
        let text_bytes = self.rows * row_bytes;

        unsafe {
            core::ptr::copy(
                self.base.add(top + row_bytes),
                self.base.add(top),
                text_bytes - row_bytes,
            );
        }

        let last_row_y = MARGIN + (self.rows - 1) * CELL_H;
        let bg = self.bg;
        for y in last_row_y..last_row_y + CELL_H {
            for x in 0..self.width {
                self.put_pixel(x, y, bg);
            }
        }
        self.row = self.rows - 1;
    }

    /// Move the cursor left without erasing, for repositioning within a line
    /// that has already been drawn.
    pub fn move_left(&mut self, count: usize) {
        self.col = self.col.saturating_sub(count);
    }

    /// Draw the Kestrel mark, and leave the text cursor below it.
    ///
    /// Supersampled: the shape is all diagonals, and without it the edges
    /// stair-step badly.
    pub fn draw_logo(&mut self, size: usize) {
        const SAMPLES: usize = 3;

        let left = MARGIN;
        let top = MARGIN;

        for y in 0..size {
            for x in 0..size {
                let (mut red, mut green, mut blue) = (0u32, 0u32, 0u32);

                for sub_y in 0..SAMPLES {
                    for sub_x in 0..SAMPLES {
                        let u = (x * SAMPLES + sub_x) as f32 / (size * SAMPLES) as f32;
                        let v = (y * SAMPLES + sub_y) as f32 / (size * SAMPLES) as f32;

                        let colour = logo::colour(u, v);
                        red += (colour >> 16) & 0xFF;
                        green += (colour >> 8) & 0xFF;
                        blue += colour & 0xFF;
                    }
                }

                let count = (SAMPLES * SAMPLES) as u32;
                let packed = self.rgb(
                    (red / count) as u8,
                    (green / count) as u8,
                    (blue / count) as u8,
                );
                self.put_pixel(left + x, top + y, packed);
            }
        }

        // Continue text below the mark.
        self.row = (top + size).div_ceil(CELL_H) + 1;
        self.col = 0;
    }

    fn newline(&mut self) {
        self.col = 0;
        self.row += 1;
        if self.row >= self.rows {
            self.scroll();
        }
    }

    pub fn write_byte(&mut self, byte: u8) {
        match byte {
            b'\n' => self.newline(),
            b'\r' => self.col = 0,
            b'\t' => {
                for _ in 0..4 - (self.col % 4) {
                    self.write_byte(b' ');
                }
            }
            byte => {
                if self.col >= self.cols {
                    self.newline();
                }
                let (col, row) = (self.col, self.row);
                self.draw_glyph(byte, col, row);
                self.col += 1;
            }
        }
    }
}

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            self.write_byte(byte);
        }
        Ok(())
    }
}

pub static CONSOLE: Mutex<Option<Console>> = Mutex::new(None);
