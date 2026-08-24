//! Text console drawn directly into the framebuffer Limine hands us.

use core::fmt;
use limine::framebuffer::Framebuffer;
use spin::Mutex;

use crate::font::{self, GLYPH_HEIGHT, GLYPH_WIDTH};

/// Left/top inset so text isn't jammed against the bezel, in unscaled pixels.
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
    scale: usize,
    cell_w: usize,
    cell_h: usize,
    margin: usize,
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

        // The glyphs are 8x16. How many times to repeat each pixel is a
        // property of the screen, not of the font: the same magnification that
        // reads well at 1280x800 is a postage stamp on a 4K panel. `theme`
        // answers that for the desktop too, so there is one rule rather than
        // two that drift.
        let scale = crate::theme::console_scale_for(width, height);
        let cell_w = GLYPH_WIDTH * scale;
        // Two font rows of leading, or lines with descenders run together.
        let cell_h = (GLYPH_HEIGHT + 2) * scale;
        let margin = MARGIN * scale;

        let mut console = Self {
            base: fb.address() as *mut u8,
            width,
            height,
            pitch: fb.pitch as usize,
            bytes_per_pixel: (fb.bpp as usize).div_ceil(8),
            red_shift: fb.red_mask_shift,
            green_shift: fb.green_mask_shift,
            blue_shift: fb.blue_mask_shift,
            scale,
            cell_w,
            cell_h,
            margin,
            cols: (width - 2 * margin) / cell_w,
            rows: (height - 2 * margin) / cell_h,
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
        let origin_x = self.margin + col * self.cell_w;
        let origin_y = self.margin + row * self.cell_h;
        let (fg, bg) = (self.fg, self.bg);

        for (gy, bits) in glyph.iter().enumerate() {
            for gx in 0..GLYPH_WIDTH {
                // Bit 0 is the leftmost pixel of the row.
                let colour = if bits & (1 << gx) != 0 { fg } else { bg };
                for sy in 0..self.scale {
                    for sx in 0..self.scale {
                        self.put_pixel(
                            origin_x + gx * self.scale + sx,
                            origin_y + gy * self.scale + sy,
                            colour,
                        );
                    }
                }
            }
        }
    }

    /// Move every text row up by one and blank the last one.
    fn scroll(&mut self) {
        let row_bytes = self.cell_h * self.pitch;
        let top = self.margin * self.pitch;
        let text_bytes = self.rows * row_bytes;

        unsafe {
            core::ptr::copy(
                self.base.add(top + row_bytes),
                self.base.add(top),
                text_bytes - row_bytes,
            );
        }

        let last_row_y = self.margin + (self.rows - 1) * self.cell_h;
        let bg = self.bg;
        for y in last_row_y..last_row_y + self.cell_h {
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

        let left = self.margin;
        let top = self.margin;

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
        self.row = (top + size).div_ceil(self.cell_h) + 1;
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
