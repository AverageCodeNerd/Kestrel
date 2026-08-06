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

use crate::font;
use crate::theme::{Theme, Wallpaper};

/// Fixed layout, in pixels, that nothing gains from making configurable.
const LAUNCHER_X: isize = 6;
const LAUNCHER_BUTTON_X: isize = 6;
const LAUNCHER_ENTRIES: usize = 2;

/// Desktop shortcuts: a square icon with two lines of text beside it.
const SHORTCUT_X: isize = 18;
const SHORTCUT_TOP: isize = 20;
const SHORTCUT_SECOND_TOP: isize = 98;
const SHORTCUT_ICON: usize = 48;
const SHORTCUT_LABEL_X: isize = 58;

/// Layout derived from the theme.
///
/// Everything that measures itself in characters has to be recomputed when the
/// text scale changes, so it is worked out once per theme change rather than
/// being const. Keeping it in one struct is what stops the drawing code and
/// the hit tests drifting apart — the bug this file has produced most often.
#[derive(Clone, Copy)]
struct Metrics {
    cell_w: usize,
    cell_h: usize,
    title_height: usize,
    panel_height: usize,
    panel_at_top: bool,
    border: usize,

    task_width: usize,
    task_step: isize,
    task_strip_x: isize,
    launcher_button_width: usize,

    launcher_width: usize,
    launcher_row_height: usize,
    launcher_row_step: isize,
    launcher_header: isize,
    launcher_height: usize,

    shortcut_label_width: usize,
}

impl Metrics {
    fn from(theme: &Theme) -> Self {
        let cell_w = theme.cell_width();
        let cell_h = theme.cell_height();

        // Wide enough for an eight-character name plus padding, so the common
        // window titles are not abbreviated in the panel.
        let task_width = 8 * cell_w + 14;
        // Sized to actually fit "Kestrel" rather than clipping the name.
        let launcher_button_width = 7 * cell_w + 12;

        let launcher_row_height = 2 * cell_h + 4;
        let launcher_row_step = launcher_row_height as isize + 6;
        let launcher_header = cell_h as isize + 16;

        Self {
            cell_w,
            cell_h,
            title_height: theme.title_height,
            panel_height: theme.panel_height,
            panel_at_top: theme.panel_at_top,
            border: theme.window_border_width,

            task_width,
            task_step: task_width as isize + 6,
            task_strip_x: LAUNCHER_BUTTON_X + launcher_button_width as isize + 8,
            launcher_button_width,

            // Holds the longest entry name at the current scale.
            launcher_width: 20 * cell_w,
            launcher_row_height,
            launcher_row_step,
            launcher_header,
            launcher_height: launcher_header as usize
                + LAUNCHER_ENTRIES * launcher_row_step as usize
                + 6,

            shortcut_label_width: 8 * cell_w,
        }
    }
}

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

/// What a window is for.
///
/// Windows are found by role rather than by position, because closing one
/// shifts every index after it — and the terminal being "window 0" is exactly
/// the assumption that would quietly send shell output into the System Monitor
/// the first time somebody closed it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Terminal,
    Monitor,
}

impl Kind {
    /// The order the launcher lists them in.
    pub const ALL: [Kind; 2] = [Kind::Terminal, Kind::Monitor];

    pub fn title(self) -> &'static str {
        match self {
            Kind::Terminal => "Terminal",
            Kind::Monitor => "System Monitor",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Kind::Terminal => "The Kestrel shell",
            Kind::Monitor => "Hardware overview",
        }
    }
}

pub struct Window {
    pub kind: Kind,
    pub title: String,
    pub x: isize,
    pub y: isize,
    pub width: usize,
    pub height: usize,
    /// Text content, one entry per line.
    pub lines: Vec<String>,
    /// Copied from the theme rather than looked up, so `rows` and `push` keep
    /// signatures the shell can call without knowing about theming. The
    /// compositor refreshes these whenever the theme changes.
    title_height: usize,
    cell_height: usize,
}

impl Window {
    pub fn new(kind: Kind, x: isize, y: isize, width: usize, height: usize) -> Self {
        let theme = crate::theme::current();
        Self {
            kind,
            title: String::from(kind.title()),
            x,
            y,
            width,
            height,
            lines: Vec::new(),
            title_height: theme.title_height,
            cell_height: theme.cell_height(),
        }
    }

    /// How many text rows fit inside this window.
    pub fn rows(&self) -> usize {
        self.height
            .saturating_sub(self.title_height + 8)
            .checked_div(self.cell_height)
            .unwrap_or(0)
            .max(1)
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
            && y < self.y + self.title_height as isize
    }

    /// The close button's square, at the right end of the title bar.
    ///
    /// Inset by two pixels so it does not sit flush against the frame, and
    /// square so it stays proportionate as the title bar is resized.
    fn close_button(&self) -> (isize, isize, usize) {
        let size = self.title_height.saturating_sub(8).max(8);
        let x = self.x + self.width as isize - size as isize - 4;
        let y = self.y + (self.title_height as isize - size as isize) / 2;
        (x, y, size)
    }

    fn contains_close(&self, x: isize, y: isize) -> bool {
        let (bx, by, size) = self.close_button();
        x >= bx && x < bx + size as isize && y >= by && y < by + size as isize
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
    /// A window the user clicked for in the launcher that is not open yet.
    open_request: Option<Kind>,
    /// The appearance in force, copied so drawing never locks.
    theme: Theme,
    /// Layout derived from `theme`, recomputed only when it changes.
    m: Metrics,
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
        let theme = crate::theme::current();
        let m = Metrics::from(&theme);

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
            open_request: None,
            theme,
            m,
        }
    }

    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Adopt a changed theme and repaint everything.
    ///
    /// The whole screen is invalidated because a theme change can move the
    /// panel, resize every glyph and recolour every pixel at once — there is
    /// no region small enough to be worth working out.
    pub fn apply_theme(&mut self, theme: Theme) {
        self.m = Metrics::from(&theme);

        // Windows cache the two measurements they need, so they have to be
        // told; otherwise their text keeps the old line spacing.
        for window in &mut self.windows {
            window.title_height = self.m.title_height;
            window.cell_height = self.m.cell_h;
        }

        self.theme = theme;
        self.invalidate_all();
    }

    /// Where a window of this kind is, if it is open.
    pub fn find(&self, kind: Kind) -> Option<usize> {
        self.windows.iter().position(|window| window.kind == kind)
    }

    /// Close a window, keeping the focus somewhere sensible.
    pub fn close(&mut self, index: usize) {
        if index >= self.windows.len() {
            return;
        }
        self.windows.remove(index);

        // Every index after the removed one has shifted, including the one
        // being dragged — simplest and safest is to drop the drag entirely.
        self.dragging = None;
        self.focused = self.focused.min(self.windows.len().saturating_sub(1));
        self.invalidate_all();
    }

    /// A window the user asked for that does not exist yet.
    ///
    /// The compositor knows what was clicked but not how to build a window —
    /// where it goes, how big it is, what is in it — so it records the request
    /// and the shell services it on the next pass.
    pub fn take_open_request(&mut self) -> Option<Kind> {
        self.open_request.take()
    }

    /// Add a window and give it the focus, as opening something should.
    pub fn open(&mut self, window: Window) {
        self.windows.push(window);
        self.focused = self.windows.len() - 1;
        self.invalidate_all();
    }

    /// Y coordinate of the panel's top edge.
    fn panel_y(&self) -> isize {
        if self.m.panel_at_top {
            0
        } else {
            self.height.saturating_sub(self.m.panel_height) as isize
        }
    }

    /// Y coordinate of the launcher's top edge, opening away from the panel.
    fn launcher_y(&self) -> isize {
        if self.m.panel_at_top {
            self.m.panel_height as isize + 4
        } else {
            self.height
                .saturating_sub(self.m.panel_height + 4 + self.m.launcher_height) as isize
        }
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
        self.region(
            LAUNCHER_X,
            self.launcher_y(),
            self.m.launcher_width,
            self.m.launcher_height,
        )
    }

    fn draw_text(&mut self, x: isize, y: isize, text: &str, colour: u32) {
        let (cell_w, scale) = (self.m.cell_w, self.theme.scale);

        for (index, byte) in text.bytes().enumerate() {
            let glyph = font::glyph(byte);
            let origin_x = x + (index * cell_w) as isize;

            for (row, bits) in glyph.iter().enumerate() {
                for column in 0..font::GLYPH_WIDTH {
                    if bits & (1 << column) == 0 {
                        continue;
                    }
                    for sy in 0..scale {
                        for sx in 0..scale {
                            let px = origin_x + (column * scale + sx) as isize;
                            let py = y + (row * scale + sy) as isize;
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

        self.draw_wallpaper(damage);

        // Desktop shortcuts make the otherwise static experimental shell feel
        // like a real workspace.  They are also mouse targets for the two
        // built-in applications.
        if self.theme.show_shortcuts {
            self.draw_shortcut(SHORTCUT_X as usize, SHORTCUT_TOP as usize, "Terminal", "Shell");
            self.draw_shortcut(
                SHORTCUT_X as usize,
                SHORTCUT_SECOND_TOP as usize,
                "System",
                "Monitor",
            );
        }

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

    /// Paint the background within the damaged region.
    ///
    /// Written straight into the buffer rather than through `fill`, because
    /// the colour changes per row (or per column) and this is the one layer
    /// that always covers every damaged pixel.
    fn draw_wallpaper(&mut self, damage: Damage) {
        let (top, bottom) = (self.theme.desktop_top, self.theme.desktop_bottom);
        let style = self.theme.wallpaper;

        for y in damage.y..damage.y + damage.height {
            let down = y as f32 / self.height.max(1) as f32;

            for x in damage.x..damage.x + damage.width {
                let colour = match style {
                    Wallpaper::Solid => top,
                    Wallpaper::Gradient => logo::blend(top, bottom, down),
                    Wallpaper::Horizontal => {
                        logo::blend(top, bottom, x as f32 / self.width.max(1) as f32)
                    }
                    Wallpaper::Grid => {
                        let base = logo::blend(top, bottom, down);
                        // A line every 48 pixels, lightened towards the top
                        // colour so it reads on both dark and light themes.
                        if x % 48 == 0 || y % 48 == 0 {
                            logo::blend(base, 0xFFFFFF, 0.06)
                        } else {
                            base
                        }
                    }
                };
                self.buffer[y * self.width + x] = colour;
            }
        }
    }

    /// The Kestrel mark, drawn in the current theme's colours.
    ///
    /// `logo::part` gives the shape without committing to a palette, so the
    /// falcon takes the theme rather than staying its own blue — an icon that
    /// ignores the theme is the one thing that gives away a recoloured desktop
    /// as a recolouring.
    fn draw_logo(&mut self, x: usize, y: usize, size: usize) {
        let badge = self.theme.title_active;
        let bird = self.theme.light_text;
        let beak = self.theme.text_dim;

        for row in 0..size {
            for column in 0..size {
                let u = column as f32 / size as f32;
                let v = row as f32 / size as f32;
                if !logo::in_badge(u, v, 0.22) {
                    continue;
                }

                let colour = match logo::part(u, v) {
                    logo::Part::Backdrop => badge,
                    logo::Part::Beak => beak,
                    _ => bird,
                };
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
        let fits = max_width / self.m.cell_w.max(1);
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
        let (light, accent) = (self.theme.light_text, self.theme.title_active);
        // The labels sit on the wallpaper, not on the icon, so they follow the
        // desktop's text colour rather than the icon's.
        let label = self.theme.desktop_text;
        let label_width = self.m.shortcut_label_width;

        self.fill(x as isize, y as isize, SHORTCUT_ICON, SHORTCUT_ICON, light);
        self.fill(
            x as isize + 3,
            y as isize + 3,
            SHORTCUT_ICON - 6,
            SHORTCUT_ICON - 6,
            accent,
        );
        self.draw_logo(x + 10, y + 8, 28);

        let label_x = x as isize + SHORTCUT_LABEL_X;
        self.draw_text_within(label_x, y as isize + 5, name, label, label_width);
        self.draw_text_within(label_x, y as isize + 27, detail, label, label_width);
    }

    /// Whether `(x, y)` is inside the shortcut whose icon starts at `top`.
    ///
    /// The label is part of the shortcut as far as a user is concerned, so the
    /// target covers the text too — measured from where the text is actually
    /// drawn rather than guessed, which is how it came to stop short of it.
    fn in_shortcut(&self, x: isize, y: isize, top: isize) -> bool {
        x >= SHORTCUT_X
            && x < SHORTCUT_X + SHORTCUT_LABEL_X + self.m.shortcut_label_width as isize
            && y >= top
            && y < top + SHORTCUT_ICON as isize
    }

    fn draw_panel(&mut self) {
        let y = self.panel_y();
        let (panel, edge) = (self.theme.panel, self.theme.panel_edge);
        let (light, accent) = (self.theme.panel_text, self.theme.title_active);
        let height = self.m.panel_height;
        // Text sits one line in from the top of the bar, centred by eye.
        let text_y = y + (height as isize - self.m.cell_h as isize) / 2;
        let box_height = height.saturating_sub(8);

        self.fill(0, y, self.width, height, panel);
        // The rule goes along whichever edge faces the rest of the screen.
        let rule_y = if self.m.panel_at_top { y + height as isize - 1 } else { y };
        self.fill(0, rule_y, self.width, 1, edge);

        // Launcher button, task strip, and a small status area.  A clock needs
        // a time service; until that exists this says whatever the user wants.
        self.fill(
            LAUNCHER_BUTTON_X,
            y + 4,
            self.m.launcher_button_width,
            box_height,
            accent,
        );
        self.draw_text_within(
            LAUNCHER_BUTTON_X + 8,
            text_y,
            "Kestrel",
            light,
            self.m.launcher_button_width - 12,
        );

        let mut x = self.m.task_strip_x;
        let titles: Vec<String> = self.windows.iter().map(|window| window.title.clone()).collect();
        for (index, title) in titles.iter().enumerate() {
            let colour = if index == self.focused {
                self.theme.launcher_selected
            } else {
                self.theme.launcher
            };
            self.fill(x, y + 4, self.m.task_width, box_height, colour);
            // The panel is intentionally compact.  A window's first word is
            // readable here while its full title remains in the title bar.
            let label = title.split_whitespace().next().unwrap_or(title);
            self.draw_text_within(x + 7, text_y, label, light, self.m.task_width - 14);
            x += self.m.task_step;
        }

        // Right-aligned from its actual width, so it cannot run off the edge.
        if self.theme.show_status {
            let status = self.theme.status_text.clone();
            let status_width = status.chars().count() * self.m.cell_w;
            if self.width > status_width + 24 {
                let status_x = (self.width - status_width - 12) as isize;
                // Never let it collide with the last task button.
                if status_x > x + 8 {
                    self.draw_text(status_x, text_y, &status, light);
                }
            }
        }
    }

    fn draw_launcher(&mut self) {
        let y = self.launcher_y();
        let (width, height) = (self.m.launcher_width, self.m.launcher_height);
        let light = self.theme.light_text;

        self.fill(LAUNCHER_X, y, width, height, self.theme.launcher);
        self.fill(LAUNCHER_X, y, width, 1, self.theme.panel_edge);
        // Closing the launcher only invalidates the launcher's own rectangle,
        // so nothing here may be drawn beyond it either.
        self.draw_text_within(LAUNCHER_X + 12, y + 10, "Applications", light, width - 24);

        // Driven by `Kind::ALL`, so the rows and the click handling cannot
        // disagree about which entry is which.
        let entries: Vec<(&str, &str)> = Kind::ALL
            .iter()
            .map(|kind| (kind.title(), kind.description()))
            .collect();
        let row_x = LAUNCHER_X + 6;
        let row_width = width - 12;
        let text_width = row_width - 20;

        for (index, (name, description)) in entries.iter().enumerate() {
            let row_y = y + self.m.launcher_header + index as isize * self.m.launcher_row_step;
            self.fill(
                row_x,
                row_y,
                row_width,
                self.m.launcher_row_height,
                self.theme.launcher_selected,
            );
            // One cell apart, so the description clears the name.
            self.draw_text_within(row_x + 10, row_y + 4, name, light, text_width);
            self.draw_text_within(
                row_x + 10,
                row_y + 4 + self.m.cell_h as isize,
                description,
                light,
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

        let title_height = self.m.title_height;
        let border = self.m.border;
        let inset = border as isize;

        // Border, then body inset by the border width.
        self.fill(x, y, width, height, self.theme.window_border);
        self.fill(
            x + inset,
            y + title_height as isize,
            width.saturating_sub(border * 2),
            height.saturating_sub(title_height + border),
            self.theme.window_body,
        );

        let title_colour = if focused {
            self.theme.title_active
        } else {
            self.theme.title_inactive
        };
        self.fill(
            x + inset,
            y + inset,
            width.saturating_sub(border * 2),
            title_height.saturating_sub(border),
            title_colour,
        );

        // Centred in the title bar, so it stays put as the bar is resized.
        let title_y = y + (title_height as isize - self.m.cell_h as isize) / 2;
        let title = self.windows[index].title.clone();
        let (close_x, close_y, close_size) = self.windows[index].close_button();

        // The title stops before the close button rather than running under it.
        let title_room = (close_x - (x + 8)).max(0) as usize;
        self.draw_text_within(x + 8, title_y, &title, self.theme.light_text, title_room);

        // A cross, drawn as two diagonals rather than a glyph so it stays
        // square and centred at any title-bar height.
        let mark = self.theme.light_text;
        self.fill(close_x, close_y, close_size, close_size, self.theme.window_border);
        for step in 2..close_size.saturating_sub(2) {
            let far = close_size - 1 - step;
            self.plot((close_x + step as isize) as usize, (close_y + step as isize) as usize, mark);
            self.plot((close_x + step as isize) as usize, (close_y + far as isize) as usize, mark);
        }

        // Wrapped to the window's interior, then clipped to it as a backstop.
        //
        // Nothing may be drawn outside the window's own rectangle: a drag
        // invalidates the rectangle it left and the one it moved to, so pixels
        // beyond that are never erased and smear across the desktop.
        let colour = if focused { self.theme.text } else { self.theme.text_dim };
        let interior = width.saturating_sub(16);
        let columns = interior / self.m.cell_w.max(1);
        let cell_h = self.m.cell_h;
        let lines = self.windows[index].lines.clone();

        let mut row = 0;
        'lines: for line in lines.iter() {
            for piece in wrap(line, columns) {
                let py = y + (title_height + 4 + row * cell_h) as isize;
                if py + cell_h as isize > y + height as isize {
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
                    b'#' => self.theme.cursor_edge,
                    b'*' => self.theme.cursor,
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

        let panel_y = self.panel_y();
        let in_panel = y >= panel_y && y < panel_y + self.m.panel_height as isize;
        if in_panel {
            if x >= LAUNCHER_BUTTON_X
                && x < LAUNCHER_BUTTON_X + self.m.launcher_button_width as isize
            {
                self.launcher_open = !self.launcher_open;
                self.invalidate(self.launcher_region());
                return;
            }

            if x >= self.m.task_strip_x {
                let offset = x - self.m.task_strip_x;
                let index = (offset / self.m.task_step) as usize;
                // The buttons are narrower than their spacing, so a click can
                // land in the gap between two; that is not a click on either.
                let within = offset % self.m.task_step < self.m.task_width as isize;

                if within && index < self.windows.len() {
                    self.focused = index;
                    self.launcher_open = false;
                    self.invalidate_all();
                }
            }
            return;
        }

        if self.launcher_open {
            let launcher_y = self.launcher_y();
            let inside_x = x >= LAUNCHER_X && x < LAUNCHER_X + self.m.launcher_width as isize;

            if inside_x && y >= launcher_y + self.m.launcher_header {
                let offset = y - launcher_y - self.m.launcher_header;
                let row = (offset / self.m.launcher_row_step) as usize;
                // Rows are shorter than their spacing; a click in the gap
                // between them selects neither.
                let within =
                    offset % self.m.launcher_row_step < self.m.launcher_row_height as isize;

                if within {
                    if let Some(&kind) = Kind::ALL.get(row) {
                        // Focus it if it is open, otherwise ask for it to be
                        // opened — which is what makes closing a window
                        // recoverable rather than permanent.
                        match self.find(kind) {
                            Some(index) => self.focused = index,
                            None => self.open_request = Some(kind),
                        }
                    }
                }
                self.launcher_open = false;
                self.invalidate_all();
                return;
            }
            self.launcher_open = false;
            self.invalidate(self.launcher_region());
        }

        // The desktop shortcuts mirror the launcher entries, and behave the
        // same way: focus what is open, open what is not. By kind rather than
        // by index — a closed window shifts every index after it, so "shortcut
        // two means window one" stops being true the moment anything closes.
        if self.theme.show_shortcuts {
            for (row, top) in [SHORTCUT_TOP, SHORTCUT_SECOND_TOP].iter().enumerate() {
                if !self.in_shortcut(x, y, *top) {
                    continue;
                }
                if let Some(&kind) = Kind::ALL.get(row) {
                    match self.find(kind) {
                        Some(index) => self.focused = index,
                        None => self.open_request = Some(kind),
                    }
                    self.invalidate_all();
                }
                return;
            }
        }

        // A fresh press: find the topmost window whose title bar was hit.
        //
        // Checked from the top down so a window covering another takes the
        // click, and the close button is tested before the drag — otherwise
        // pressing it would start a drag instead of closing anything.
        for index in (0..self.windows.len()).rev() {
            if self.windows[index].contains_close(x, y) {
                self.close(index);
                return;
            }
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
