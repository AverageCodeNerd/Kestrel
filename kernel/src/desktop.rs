//! A windowed desktop drawn straight into the framebuffer.
//!
//! Composited back to front into an off-screen buffer, then copied to the
//! screen in one pass. Drawing directly would mean every window visibly
//! painting over the last, and a cursor that flickers wherever it moves.

use core::sync::atomic::{AtomicBool, Ordering};

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use spin::Mutex;

use crate::font::{self, GLYPH_HEIGHT, GLYPH_WIDTH};

// A compact, Breeze-inspired palette.  The experimental desktop deliberately
// keeps its own palette so the kernel console can remain a separate surface.
const DESKTOP_TOP: u32 = 0x2B6B9A;
const DESKTOP_BOTTOM: u32 = 0x15324B;
const WINDOW_BODY: u32 = 0xF4F6F8;
const WINDOW_BORDER: u32 = 0x6A7785;
const TITLE_ACTIVE: u32 = 0x3C8DBC;
const TITLE_INACTIVE: u32 = 0x6B7886;
const PANEL: u32 = 0x18232E;
const PANEL_EDGE: u32 = 0x405363;
const LAUNCHER: u32 = 0x263746;
const LAUNCHER_SELECTED: u32 = 0x3C8DBC;
const TEXT: u32 = 0x18232E;
const TEXT_DIM: u32 = 0x65717D;
const LIGHT_TEXT: u32 = 0xF4F6F8;
const CURSOR: u32 = 0xFFFFFF;
const CURSOR_EDGE: u32 = 0x101828;

/// Height of a window's title bar, in pixels.
const TITLE_HEIGHT: usize = 20;
const PANEL_HEIGHT: usize = 30;
/// Wide enough for the longest entry name at the current font size, rather
/// than clipping them.
const LAUNCHER_WIDTH: usize = 320;
const LAUNCHER_X: isize = 6;
/// Each row holds two lines of text, so it must be taller than two cells.
const LAUNCHER_ROW_HEIGHT: usize = 40;
const LAUNCHER_ROW_STEP: isize = 46;
/// Space above the first row, taken by the heading.
const LAUNCHER_HEADER: isize = 34;
const LAUNCHER_ENTRIES: usize = 2;
const LAUNCHER_HEIGHT: usize =
    LAUNCHER_HEADER as usize + LAUNCHER_ENTRIES * LAUNCHER_ROW_STEP as usize + 6;
/// Sized to hold an eight-character name plus its padding, so the common
/// window titles are not abbreviated in the panel.
const TASK_WIDTH: usize = 8 * CELL_W + 14;
const TASK_STEP: isize = TASK_WIDTH as isize + 6;
/// The launcher button, sized to actually fit its label rather than clipping
/// the product name.
const LAUNCHER_BUTTON_X: isize = 6;
const LAUNCHER_BUTTON_WIDTH: usize = 124;
/// Where the task buttons begin, clear of the launcher button.
const TASK_STRIP_X: isize = 138;
/// Text is drawn at double the font size, as in the console.
const SCALE: usize = 2;
const CELL_W: usize = GLYPH_WIDTH * SCALE;
const CELL_H: usize = (GLYPH_HEIGHT + 1) * SCALE;

/// Desktop shortcuts: a square icon with two lines of text beside it. The
/// label is sized in whole characters so the hit test and the drawing agree.
const SHORTCUT_X: isize = 18;
const SHORTCUT_TOP: isize = 20;
const SHORTCUT_SECOND_TOP: isize = 98;
const SHORTCUT_ICON: usize = 48;
const SHORTCUT_LABEL_X: isize = 58;
const SHORTCUT_LABEL_WIDTH: usize = 8 * CELL_W;

/// A rectangle that has changed since the last presentation.  The compositor
/// redraws every layer through this clip, so overlapping windows are still
/// composed correctly without touching unrelated framebuffer pixels.
#[derive(Clone, Copy)]
struct Damage {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

pub struct Window {
    pub title: String,
    pub x: isize,
    pub y: isize,
    pub width: usize,
    pub height: usize,
    /// Text content, one entry per line.
    pub lines: Vec<String>,
}

impl Window {
    pub fn new(title: &str, x: isize, y: isize, width: usize, height: usize) -> Self {
        Self {
            title: String::from(title),
            x,
            y,
            width,
            height,
            lines: Vec::new(),
        }
    }

    /// How many text rows fit inside this window.
    pub fn rows(&self) -> usize {
        self.height.saturating_sub(TITLE_HEIGHT + 8) / CELL_H
    }

    pub fn push(&mut self, line: &str) {
        self.lines.push(String::from(line));
        let rows = self.rows();
        if self.lines.len() > rows {
            let excess = self.lines.len() - rows;
            self.lines.drain(0..excess);
        }
    }

    fn contains_title(&self, x: isize, y: isize) -> bool {
        x >= self.x
            && x < self.x + self.width as isize
            && y >= self.y
            && y < self.y + TITLE_HEIGHT as isize
    }
}

pub struct Desktop {
    /// Off-screen buffer, one u32 per pixel.
    buffer: Vec<u32>,
    width: usize,
    height: usize,
    /// Framebuffer details, for the final copy.
    base: *mut u8,
    pitch: usize,
    bytes_per_pixel: usize,
    red_shift: u8,
    green_shift: u8,
    blue_shift: u8,

    pub windows: Vec<Window>,
    /// Index of the focused window, drawn last and on top.
    focused: usize,
    /// Window being dragged, and the grab offset within its title bar.
    dragging: Option<(usize, isize, isize)>,
    /// The launcher is a small application menu, opened from the panel.
    launcher_open: bool,
    /// Kept locally so one held mouse button produces exactly one panel click.
    left_was_down: bool,
    /// The previous cursor position, used to erase only the old arrow.
    last_cursor: (isize, isize),
    /// Union of the screen regions that need recompositing.
    damage: Option<Damage>,
    /// Active only while drawing a damaged region.
    clip: Option<Damage>,
}

// The framebuffer is a fixed region owned by this struct.
unsafe impl Send for Desktop {}

impl Desktop {
    /// # Safety
    /// `fb` must be the bootloader's framebuffer, and only one Desktop may
    /// exist for it.
    pub unsafe fn new(fb: &limine::framebuffer::Framebuffer) -> Self {
        let width = fb.width as usize;
        let height = fb.height as usize;
        let (cursor_x, cursor_y) = crate::mouse::position();

        Self {
            buffer: vec![0; width * height],
            width,
            height,
            base: fb.address() as *mut u8,
            pitch: fb.pitch as usize,
            bytes_per_pixel: (fb.bpp as usize).div_ceil(8),
            red_shift: fb.red_mask_shift,
            green_shift: fb.green_mask_shift,
            blue_shift: fb.blue_mask_shift,
            windows: Vec::new(),
            focused: 0,
            dragging: None,
            launcher_open: false,
            left_was_down: false,
            last_cursor: (cursor_x as isize, cursor_y as isize),
            // A new desktop has no valid backing image yet.
            damage: Some(Damage {
                x: 0,
                y: 0,
                width,
                height,
            }),
            clip: None,
        }
    }

    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn plot(&mut self, x: usize, y: usize, colour: u32) {
        if x < self.width
            && y < self.height
            && self.clip.map_or(true, |clip| {
                x >= clip.x
                    && x < clip.x + clip.width
                    && y >= clip.y
                    && y < clip.y + clip.height
            })
        {
            self.buffer[y * self.width + x] = colour;
        }
    }

    fn fill(&mut self, x: isize, y: isize, width: usize, height: usize, colour: u32) {
        let left = x.max(0) as usize;
        let top = y.max(0) as usize;
        let right = x.saturating_add(width as isize).max(0) as usize;
        let bottom = y.saturating_add(height as isize).max(0) as usize;
        let clip = self.clip.unwrap_or(Damage {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        });
        let start_x = left.max(clip.x);
        let start_y = top.max(clip.y);
        let end_x = right.min(self.width).min(clip.x + clip.width);
        let end_y = bottom.min(self.height).min(clip.y + clip.height);

        for py in start_y..end_y {
            for px in start_x..end_x {
                self.buffer[py * self.width + px] = colour;
            }
        }
    }

    fn region(&self, x: isize, y: isize, width: usize, height: usize) -> Option<Damage> {
        let left = x.max(0) as usize;
        let top = y.max(0) as usize;
        let right = x.saturating_add(width as isize).max(0) as usize;
        let bottom = y.saturating_add(height as isize).max(0) as usize;
        let right = right.min(self.width);
        let bottom = bottom.min(self.height);
        (left < right && top < bottom).then_some(Damage {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        })
    }

    fn invalidate(&mut self, region: Option<Damage>) {
        let Some(region) = region else { return };
        self.damage = Some(match self.damage {
            None => region,
            Some(existing) => {
                let left = existing.x.min(region.x);
                let top = existing.y.min(region.y);
                let right = (existing.x + existing.width).max(region.x + region.width);
                let bottom = (existing.y + existing.height).max(region.y + region.height);
                Damage {
                    x: left,
                    y: top,
                    width: right - left,
                    height: bottom - top,
                }
            }
        });
    }

    fn invalidate_rect(&mut self, x: isize, y: isize, width: usize, height: usize) {
        self.invalidate(self.region(x, y, width, height));
    }

    /// Mark one window's rectangle as needing a repaint.
    ///
    /// Used when a window's *contents* change rather than its position — the
    /// terminal window as output arrives — so a busy command does not force a
    /// full-screen repaint per line.
    pub fn invalidate_window(&mut self, index: usize) {
        let Some(window) = self.windows.get(index) else {
            return;
        };
        let (x, y, width, height) = (window.x, window.y, window.width, window.height);
        self.invalidate_rect(x, y, width, height);
    }

    fn invalidate_all(&mut self) {
        self.damage = Some(Damage {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        });
    }

    fn launcher_region(&self) -> Option<Damage> {
        let bottom = self.height.saturating_sub(PANEL_HEIGHT + 4);
        self.region(
            LAUNCHER_X,
            bottom.saturating_sub(LAUNCHER_HEIGHT) as isize,
            LAUNCHER_WIDTH,
            LAUNCHER_HEIGHT,
        )
    }

    fn draw_text(&mut self, x: isize, y: isize, text: &str, colour: u32) {
        for (index, byte) in text.bytes().enumerate() {
            let glyph = font::glyph(byte);
            let origin_x = x + (index * CELL_W) as isize;

            for (row, bits) in glyph.iter().enumerate() {
                for column in 0..GLYPH_WIDTH {
                    if bits & (1 << column) == 0 {
                        continue;
                    }
                    for sy in 0..SCALE {
                        for sx in 0..SCALE {
                            let px = origin_x + (column * SCALE + sx) as isize;
                            let py = y + (row * SCALE + sy) as isize;
                            if px >= 0 && py >= 0 {
                                self.plot(px as usize, py as usize, colour);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Redraw everything into the off-screen buffer and present it.
    pub fn render(&mut self) {
        // Anything that bypassed the compositor invalidates the whole screen:
        // the off-screen buffer is still correct, but the framebuffer is not.
        if SCREEN_DISTURBED.swap(false, Ordering::Relaxed) {
            self.invalidate_all();
        }

        let Some(damage) = self.damage.take() else {
            return;
        };
        self.clip = Some(damage);

        // Background first, but only within the invalidated region.
        for y in damage.y..damage.y + damage.height {
            let t = y as f32 / self.height as f32;
            let colour = logo::blend(DESKTOP_TOP, DESKTOP_BOTTOM, t);
            for x in damage.x..damage.x + damage.width {
                self.buffer[y * self.width + x] = colour;
            }
        }

        // Desktop shortcuts make the otherwise static experimental shell feel
        // like a real workspace.  They are also mouse targets for the two
        // built-in applications.
        self.draw_shortcut(SHORTCUT_X as usize, SHORTCUT_TOP as usize, "Terminal", "Shell");
        self.draw_shortcut(
            SHORTCUT_X as usize,
            SHORTCUT_SECOND_TOP as usize,
            "System",
            "Monitor",
        );

        // Back to front, so the focused window ends up on top.
        let order: Vec<usize> = (0..self.windows.len())
            .filter(|&i| i != self.focused)
            .chain(core::iter::once(self.focused))
            .filter(|&i| i < self.windows.len())
            .collect();

        for index in order {
            self.draw_window(index);
        }

        self.draw_panel();
        if self.launcher_open {
            self.draw_launcher();
        }
        self.draw_cursor();
        self.present(damage);
        self.clip = None;
    }

    fn draw_logo(&mut self, x: usize, y: usize, size: usize) {
        for row in 0..size {
            for column in 0..size {
                let u = column as f32 / size as f32;
                let v = row as f32 / size as f32;
                if !logo::in_badge(u, v, 0.22) {
                    continue;
                }
                let colour = logo::colour(u, v);
                self.plot(x + column, y + row, colour);
            }
        }
    }

    /// Draw text truncated to `max_width` pixels.
    ///
    /// Every label here sits in a box — a button, a title bar, a panel slot —
    /// and unbounded text simply runs past it and over whatever is next. The
    /// truncation is marked so a clipped label is not mistaken for the name.
    fn draw_text_within(&mut self, x: isize, y: isize, text: &str, colour: u32, max_width: usize) {
        let fits = max_width / CELL_W;
        if fits == 0 {
            return;
        }

        if text.chars().count() <= fits {
            self.draw_text(x, y, text, colour);
            return;
        }

        let mut shortened: String = text.chars().take(fits.saturating_sub(1)).collect();
        shortened.push('~');
        self.draw_text(x, y, &shortened, colour);
    }

    fn draw_shortcut(&mut self, x: usize, y: usize, name: &str, detail: &str) {
        self.fill(x as isize, y as isize, SHORTCUT_ICON, SHORTCUT_ICON, 0xD7E6F2);
        self.fill(
            x as isize + 3,
            y as isize + 3,
            SHORTCUT_ICON - 6,
            SHORTCUT_ICON - 6,
            TITLE_ACTIVE,
        );
        self.draw_logo(x + 10, y + 8, 28);
        let label_x = x as isize + SHORTCUT_LABEL_X;
        self.draw_text_within(label_x, y as isize + 5, name, LIGHT_TEXT, SHORTCUT_LABEL_WIDTH);
        self.draw_text_within(label_x, y as isize + 27, detail, 0xC7D9E8, SHORTCUT_LABEL_WIDTH);
    }

    /// Whether `(x, y)` is inside the shortcut whose icon starts at `top`.
    ///
    /// The label is part of the shortcut as far as a user is concerned, so the
    /// target covers the text too — measured from where the text is actually
    /// drawn rather than guessed, which is how it came to stop short of it.
    fn in_shortcut(&self, x: isize, y: isize, top: isize) -> bool {
        x >= SHORTCUT_X
            && x < SHORTCUT_X + SHORTCUT_LABEL_X + SHORTCUT_LABEL_WIDTH as isize
            && y >= top
            && y < top + SHORTCUT_ICON as isize
    }

    fn draw_panel(&mut self) {
        let y = self.height.saturating_sub(PANEL_HEIGHT) as isize;
        self.fill(0, y, self.width, PANEL_HEIGHT, PANEL);
        self.fill(0, y, self.width, 1, PANEL_EDGE);

        // Launcher button, task strip, and a small static status area.  A
        // clock needs a time service; until that exists this names the build.
        self.fill(
            LAUNCHER_BUTTON_X,
            y + 4,
            LAUNCHER_BUTTON_WIDTH,
            PANEL_HEIGHT - 8,
            TITLE_ACTIVE,
        );
        self.draw_text_within(
            LAUNCHER_BUTTON_X + 8,
            y + 7,
            "Kestrel",
            LIGHT_TEXT,
            LAUNCHER_BUTTON_WIDTH - 12,
        );

        let mut x = TASK_STRIP_X;
        let titles: Vec<String> = self.windows.iter().map(|window| window.title.clone()).collect();
        for (index, title) in titles.iter().enumerate() {
            let colour = if index == self.focused { 0x3A5266 } else { 0x263746 };
            self.fill(x, y + 4, TASK_WIDTH, PANEL_HEIGHT - 8, colour);
            // The panel is intentionally compact.  A window's first word is
            // readable here while its full title remains in the title bar.
            let label = title.split_whitespace().next().unwrap_or(title);
            self.draw_text_within(x + 7, y + 7, label, LIGHT_TEXT, TASK_WIDTH - 14);
            x += TASK_STEP;
        }

        // Right-aligned from its actual width, so it cannot run off the edge.
        const STATUS: &str = "Experimental OS";
        let status_width = STATUS.len() * CELL_W;
        if self.width > status_width + 24 {
            let status_x = (self.width - status_width - 12) as isize;
            // Never let it collide with the last task button.
            if status_x > x + 8 {
                self.draw_text(status_x, y + 7, STATUS, 0xC7D9E8);
            }
        }
    }

    fn draw_launcher(&mut self) {
        let bottom = self.height.saturating_sub(PANEL_HEIGHT + 4);
        let y = bottom.saturating_sub(LAUNCHER_HEIGHT) as isize;
        self.fill(LAUNCHER_X, y, LAUNCHER_WIDTH, LAUNCHER_HEIGHT, LAUNCHER);
        self.fill(LAUNCHER_X, y, LAUNCHER_WIDTH, 1, 0x82B9D8);
        // Closing the launcher only invalidates the launcher's own rectangle,
        // so nothing here may be drawn beyond it either.
        self.draw_text_within(
            LAUNCHER_X + 12,
            y + 10,
            "Applications",
            LIGHT_TEXT,
            LAUNCHER_WIDTH - 24,
        );

        let entries = [
            ("Terminal", "The Kestrel shell"),
            ("System Monitor", "Hardware overview"),
        ];
        let row_x = LAUNCHER_X + 6;
        let row_width = LAUNCHER_WIDTH - 12;
        let text_width = row_width - 20;

        for (index, (name, description)) in entries.iter().enumerate() {
            let row_y = y + LAUNCHER_HEADER + index as isize * LAUNCHER_ROW_STEP;
            self.fill(row_x, row_y, row_width, LAUNCHER_ROW_HEIGHT, LAUNCHER_SELECTED);
            // Two cells apart, so the description clears the name.
            self.draw_text_within(row_x + 10, row_y + 4, name, LIGHT_TEXT, text_width);
            self.draw_text_within(
                row_x + 10,
                row_y + 4 + CELL_H as isize,
                description,
                0xD7E6F2,
                text_width,
            );
        }
    }

    fn draw_window(&mut self, index: usize) {
        let (x, y, width, height, focused) = {
            let window = &self.windows[index];
            (
                window.x,
                window.y,
                window.width,
                window.height,
                index == self.focused,
            )
        };

        // Border, then body inset by one pixel.
        self.fill(x, y, width, height, WINDOW_BORDER);
        self.fill(
            x + 1,
            y + TITLE_HEIGHT as isize,
            width.saturating_sub(2),
            height.saturating_sub(TITLE_HEIGHT + 1),
            WINDOW_BODY,
        );

        let title_colour = if focused { TITLE_ACTIVE } else { TITLE_INACTIVE };
        self.fill(x + 1, y + 1, width.saturating_sub(2), TITLE_HEIGHT - 1, title_colour);

        let title = self.windows[index].title.clone();
        self.draw_text_within(x + 8, y + 3, &title, LIGHT_TEXT, width.saturating_sub(16));

        // Wrapped to the window's interior, then clipped to it as a backstop.
        //
        // Nothing may be drawn outside the window's own rectangle: a drag
        // invalidates the rectangle it left and the one it moved to, so pixels
        // beyond that are never erased and smear across the desktop.
        let colour = if focused { TEXT } else { TEXT_DIM };
        let interior = width.saturating_sub(16);
        let columns = interior / CELL_W;
        let lines = self.windows[index].lines.clone();

        let mut row = 0;
        'lines: for line in lines.iter() {
            for piece in wrap(line, columns) {
                let py = y + (TITLE_HEIGHT + 4 + row * CELL_H) as isize;
                if py + CELL_H as isize > y + height as isize {
                    break 'lines;
                }
                self.draw_text_within(x + 8, py, piece, colour, interior);
                row += 1;
            }
        }
    }

    /// An arrow, drawn from a small bitmap so it reads at any background.
    ///
    /// It is drawn at `last_cursor`, not at the live pointer position. The
    /// mouse interrupt moves the pointer asynchronously, so re-reading it here
    /// could place the arrow outside the region `handle_mouse` invalidated for
    /// it — half of it would be clipped away, and the other half would never be
    /// erased. Drawing where the damage says it is keeps the two in step; the
    /// next event invalidates the real position a frame later.
    fn draw_cursor(&mut self) {
        const ARROW: [&str; 12] = [
            "#...........",
            "##..........",
            "#*#.........",
            "#**#........",
            "#***#.......",
            "#****#......",
            "#*****#.....",
            "#******#....",
            "#***####....",
            "#**#........",
            "##.#........",
            "#...........",
        ];

        let (mx, my) = self.last_cursor;

        for (row, line) in ARROW.iter().enumerate() {
            for (column, glyph) in line.bytes().enumerate() {
                let colour = match glyph {
                    b'#' => CURSOR_EDGE,
                    b'*' => CURSOR,
                    _ => continue,
                };
                let x = mx + column as isize;
                let y = my + row as isize;
                if x >= 0 && y >= 0 {
                    self.plot(x as usize, y as usize, colour);
                }
            }
        }
    }

    /// Copy the finished buffer to the screen.
    ///
    /// A whole frame is a million pixels, so this is the hot path: on the
    /// common 32-bit layout each one is a single word write rather than four
    /// separate byte writes.
    fn present(&mut self, damage: Damage) {
        let fast = self.bytes_per_pixel == 4;

        for y in damage.y..damage.y + damage.height {
            let row = y * self.pitch;

            for x in damage.x..damage.x + damage.width {
                let colour = self.buffer[y * self.width + x];
                let packed = ((colour >> 16) & 0xFF) << self.red_shift
                    | ((colour >> 8) & 0xFF) << self.green_shift
                    | (colour & 0xFF) << self.blue_shift;

                let offset = row + x * self.bytes_per_pixel;
                unsafe {
                    let pixel = self.base.add(offset);
                    if fast {
                        pixel.cast::<u32>().write_volatile(packed);
                    } else {
                        for byte in 0..self.bytes_per_pixel.min(4) {
                            pixel.add(byte).write_volatile((packed >> (byte * 8)) as u8);
                        }
                    }
                }
            }
        }
    }

    /// Focus, raise and drag windows with the mouse.
    pub fn handle_mouse(&mut self) {
        let (x, y) = crate::mouse::position();
        let (x, y) = (x as isize, y as isize);
        let (left, _, _) = crate::mouse::buttons();

        // The cursor itself is damage: erase its old 12x12 bitmap and draw it
        // at the new location.  When it has not moved, an idle desktop costs
        // no framebuffer work at all.
        if (x, y) != self.last_cursor {
            let (old_x, old_y) = self.last_cursor;
            self.invalidate_rect(old_x, old_y, 12, 12);
            self.invalidate_rect(x, y, 12, 12);
            self.last_cursor = (x, y);
        }

        if !left {
            self.dragging = None;
            self.left_was_down = false;
            return;
        }

        if let Some((index, offset_x, offset_y)) = self.dragging {
            if let Some(window) = self.windows.get_mut(index) {
                let (old_x, old_y) = (window.x, window.y);
                let (new_x, new_y) = (x - offset_x, y - offset_y);
                if (old_x, old_y) != (new_x, new_y) {
                    let (width, height) = (window.width, window.height);
                    window.x = new_x;
                    window.y = new_y;
                    self.invalidate_rect(old_x, old_y, width, height);
                    self.invalidate_rect(new_x, new_y, width, height);
                }
            }
            return;
        }

        // All non-drag actions happen only on the press edge.  This avoids a
        // held button repeatedly opening and closing the launcher between
        // expensive full-frame repaints.
        if self.left_was_down {
            return;
        }
        self.left_was_down = true;

        let panel_y = self.height.saturating_sub(PANEL_HEIGHT) as isize;
        if y >= panel_y {
            if x >= LAUNCHER_BUTTON_X && x < LAUNCHER_BUTTON_X + LAUNCHER_BUTTON_WIDTH as isize {
                self.launcher_open = !self.launcher_open;
                self.invalidate(self.launcher_region());
                return;
            }

            if x >= TASK_STRIP_X {
                let offset = x - TASK_STRIP_X;
                let index = (offset / TASK_STEP) as usize;
                // The buttons are narrower than their spacing, so a click can
                // land in the gap between two; that is not a click on either.
                let within = offset % TASK_STEP < TASK_WIDTH as isize;

                if within && index < self.windows.len() {
                    self.focused = index;
                    self.launcher_open = false;
                    self.invalidate_all();
                }
            }
            return;
        }

        if self.launcher_open {
            let launcher_y = self
                .height
                .saturating_sub(PANEL_HEIGHT + 4 + LAUNCHER_HEIGHT) as isize;
            let inside_x =
                x >= LAUNCHER_X && x < LAUNCHER_X + LAUNCHER_WIDTH as isize;

            if inside_x && y >= launcher_y + LAUNCHER_HEADER {
                let offset = y - launcher_y - LAUNCHER_HEADER;
                let index = (offset / LAUNCHER_ROW_STEP) as usize;
                // Rows are shorter than their spacing; a click in the gap
                // between them selects neither.
                let within = offset % LAUNCHER_ROW_STEP < LAUNCHER_ROW_HEIGHT as isize;

                if within && index < self.windows.len() {
                    self.focused = index;
                }
                self.launcher_open = false;
                self.invalidate_all();
                return;
            }
            self.launcher_open = false;
            self.invalidate(self.launcher_region());
        }

        // The desktop shortcuts mirror the launcher entries.
        if self.in_shortcut(x, y, SHORTCUT_TOP) && !self.windows.is_empty() {
            self.focused = 0;
            self.invalidate_all();
            return;
        }
        if self.in_shortcut(x, y, SHORTCUT_SECOND_TOP) && self.windows.len() > 1 {
            self.focused = 1;
            self.invalidate_all();
            return;
        }

        // A fresh press: find the topmost window whose title bar was hit.
        for index in (0..self.windows.len()).rev() {
            if self.windows[index].contains_title(x, y) {
                self.focused = index;
                self.dragging = Some((index, x - self.windows[index].x, y - self.windows[index].y));
                self.invalidate_all();
                return;
            }
        }
    }
}

/// Break `text` into pieces of at most `columns` characters, preferring to
/// split at a space.
///
/// Window text is wrapped rather than truncated because a window is a page,
/// not a label: cutting a sentence off at the frame loses the half that
/// mattered, whereas a panel button has nowhere to put a second line.
/// A word longer than the whole line is broken mid-word — there is no better
/// answer, and refusing to break it would put pixels outside the window.
fn wrap(text: &str, columns: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    if columns == 0 {
        return pieces;
    }

    let mut rest = text;
    while rest.chars().count() > columns {
        let limit = rest
            .char_indices()
            .nth(columns)
            .map_or(rest.len(), |(index, _)| index);
        // Break at the last space that fits; if there is none, break flush.
        let split = match rest[..limit].rfind(' ') {
            Some(0) | None => limit,
            Some(space) => space,
        };
        let (piece, remainder) = rest.split_at(split);
        pieces.push(piece.trim_end());
        rest = remainder.trim_start();
    }

    // A blank line is meaningful spacing, so an empty remainder still counts.
    if !rest.is_empty() || pieces.is_empty() {
        pieces.push(rest);
    }
    pieces
}

pub static DESKTOP: Mutex<Option<Desktop>> = Mutex::new(None);

/// Set when something has painted over the framebuffer behind the compositor's
/// back — in practice `println!`, which writes to the text console directly.
///
/// Before damage tracking, a stray kernel message was erased by the next
/// full-screen repaint. Now that only changed pixels reach the screen, it would
/// stay there for as long as the desktop is open. This is a bare atomic rather
/// than a call into the desktop because `_print` can run *inside*
/// `desktop::with`, and taking `DESKTOP` again there would deadlock.
pub static SCREEN_DISTURBED: AtomicBool = AtomicBool::new(false);

/// Called from the console when it writes to the framebuffer.
pub fn note_screen_disturbed() {
    SCREEN_DISTURBED.store(true, Ordering::Relaxed);
}

pub fn with<T>(body: impl FnOnce(&mut Desktop) -> T) -> Option<T> {
    x86_64::instructions::interrupts::without_interrupts(|| DESKTOP.lock().as_mut().map(body))
}

pub fn active() -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| DESKTOP.lock().is_some())
}
