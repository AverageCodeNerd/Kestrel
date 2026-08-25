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
use crate::settings;
use crate::theme::{Theme, Wallpaper};

/// One line in the launcher: a window it can open, or a program it can run.
#[derive(Clone, PartialEq, Eq)]
pub enum Target {
    Window(Kind),
    /// An installed package, run by name the way `exec` runs it.
    Program(String),
}

#[derive(Clone)]
pub struct LauncherEntry {
    pub name: String,
    pub detail: String,
    pub target: Target,
}

/// The gap that separates everything from everything else, in unscaled pixels.
/// One number so the desktop has one rhythm rather than a dozen near misses.
const GAP: usize = 6;

/// The pointer bitmap is this many pixels square before magnification. The
/// damage region and the drawing both derive from it, or the arrow leaves a
/// trail of its own top-left corner behind.
const CURSOR_SIZE: usize = 12;

/// Layout derived from the theme.
///
/// Everything that measures itself in characters has to be recomputed when the
/// text scale changes, so it is worked out once per theme change rather than
/// being const. Keeping it in one struct is what stops the drawing code and
/// the hit tests drifting apart — the bug this file has produced most often.
#[derive(Clone, Copy)]
struct Metrics {
    /// How many screen pixels one font pixel occupies. Every measurement below
    /// is a multiple of it, which is what makes the desktop the same size on a
    /// 4K panel as on a 1280x800 one instead of a quarter of it.
    scale: usize,
    cell_w: usize,
    cell_h: usize,
    /// The standard gap, scaled.
    gap: isize,
    title_height: usize,
    panel_height: usize,
    panel_at_top: bool,
    border: usize,
    /// Corner rounding in real pixels, and zero for a flat theme.
    corner: usize,
    /// How far inside a window's edge a press counts as grabbing it to resize.
    grab: usize,
    /// Gradients and rounding, or solid fills and square edges.
    soft: bool,
    /// How far a window's shadow reaches, and zero when it casts none. Damage
    /// rectangles have to allow for it or a dragged window smears its own
    /// shadow across the wallpaper.
    shadow: usize,

    task_width: usize,
    task_step: isize,
    task_strip_x: isize,
    launcher_x: isize,
    launcher_button_width: usize,

    launcher_width: usize,
    launcher_row_height: usize,
    launcher_row_step: isize,
    launcher_header: isize,

    /// Desktop shortcuts: a square icon with two lines of text beside it.
    shortcut_x: isize,
    shortcut_top: isize,
    shortcut_step: isize,
    shortcut_icon: usize,
    shortcut_label_x: isize,
    shortcut_label_width: usize,
}

/// The longest line the launcher menu has to show, in characters.
fn longest_entry() -> usize {
    Kind::ALL
        .iter()
        .map(|kind| kind.title().len().max(kind.description().len()))
        .max()
        .unwrap_or(16)
}

impl Metrics {
    fn from(theme: &Theme) -> Self {
        let scale = theme.scale;
        let cell_w = theme.cell_width();
        let cell_h = theme.cell_height();
        let gap = (GAP * scale) as isize;

        // Wide enough for an eight-character name plus padding, so the common
        // window titles are not abbreviated in the panel.
        let task_width = 8 * cell_w + 14 * scale;
        // Sized to actually fit "Kestrel" rather than clipping the name.
        let launcher_button_width = 7 * cell_w + 12 * scale;

        let launcher_row_height = 2 * cell_h + 4 * scale;
        let launcher_row_step = launcher_row_height as isize + gap;
        let launcher_header = cell_h as isize + 16 * scale as isize;

        let launcher_x = gap;
        let shortcut_icon = 40 * scale;

        Self {
            scale,
            cell_w,
            cell_h,
            gap,
            title_height: theme.title_height,
            panel_height: theme.panel_height,
            panel_at_top: theme.panel_at_top,
            border: theme.window_border_width,
            corner: theme.corner(),
            soft: theme.soft(),
            // Deliberately generous: a shadow that fades to nothing needs room
            // to fade in, and the cost is paid only where a window overlaps
            // what is behind it.
            shadow: if theme.shadow && theme.soft() { 6 * scale } else { 0 },

            // Wide enough to hit with a mouse that moves in whole pixels,
            // narrow enough that the title bar is still mostly draggable.
            // Scaled like everything else, or it would be a hair's breadth on
            // a 4K panel and half the title bar on a small one.
            grab: 4 * scale,

            task_width,
            task_step: task_width as isize + gap,
            task_strip_x: launcher_x + launcher_button_width as isize + gap + gap / 2,
            launcher_x,
            launcher_button_width,

            // Wide enough for the longest thing it has to say, measured from
            // the entries themselves. A round twenty columns fitted the names
            // but clipped every description to "Change how this~".
            launcher_width: (longest_entry() + 4) * cell_w,
            launcher_row_height,
            launcher_row_step,
            launcher_header,

            shortcut_x: 18 * scale as isize,
            shortcut_top: 20 * scale as isize,
            // Icon plus two lines of label plus breathing room.
            shortcut_step: (shortcut_icon + 26 * scale) as isize,
            shortcut_icon,
            shortcut_label_x: (shortcut_icon + 10 * scale) as isize,
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
    Settings,
    Software,
    /// A window a running program draws into. Not in the launcher: it exists
    /// only while something owns it.
    Surface,
}

impl Kind {
    /// The order the launcher lists them in. `Surface` is absent on purpose:
    /// it exists only while a program owns it, so there is nothing to open.
    pub const ALL: [Kind; 4] = [Kind::Terminal, Kind::Monitor, Kind::Settings, Kind::Software];

    pub fn title(self) -> &'static str {
        match self {
            Kind::Terminal => "Terminal",
            Kind::Monitor => "System Monitor",
            Kind::Settings => "Settings",
            Kind::Software => "Software",
            Kind::Surface => "Program",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Kind::Terminal => "The Kestrel shell",
            Kind::Monitor => "Hardware overview",
            Kind::Settings => "Change how this looks",
            Kind::Software => "Install and remove programs",
            Kind::Surface => "A running program",
        }
    }
}

/// Which edges of a window a resize is pulling. A corner pulls two.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Edges {
    left: bool,
    right: bool,
    top: bool,
    bottom: bool,
}

impl Edges {
    fn any(self) -> bool {
        self.left || self.right || self.top || self.bottom
    }
}

/// A resize in progress.
///
/// The rectangle and pointer position the drag *started* from are kept, and
/// every frame recomputes the new rectangle from those. Accumulating a delta
/// per frame instead would let the window creep: once a pull hits the minimum
/// size the surplus movement has nowhere to go, and feeding the clamped result
/// back in as the next frame's origin means dragging in and back out again
/// does not return the window to where it was.
#[derive(Clone, Copy)]
struct Resize {
    index: usize,
    edges: Edges,
    /// `(x, y, width, height)` when the drag began.
    rect: (isize, isize, usize, usize),
    /// Where the pointer was when the drag began.
    pointer: (isize, isize),
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
    /// Pixels, for a window a program draws into. One `u32` per pixel, row by
    /// row, `surface_width` wide.
    surface: Vec<u32>,
    surface_width: usize,
    surface_height: usize,
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
            surface: Vec::new(),
            surface_width: 0,
            surface_height: 0,
            title_height: theme.title_height,
            cell_height: theme.cell_height(),
        }
    }

    /// Where the inside of the window starts, below the title bar.
    fn body_origin(&self) -> (isize, isize) {
        (self.x, self.y + self.title_height as isize)
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

    /// Which edges, if any, a press at this point grabs for a resize.
    ///
    /// The band is inside the window rather than straddling its edge. Straddling
    /// would put half the target on the desktop, where a press is already
    /// spoken for — it opens the desktop menu — and would make two overlapping
    /// windows fight over the pixels between them.
    ///
    /// Corners are deliberately more generous than sides: they are the only
    /// way to change both dimensions at once, and a band just wide enough to
    /// hit a side is nearly impossible to hit at a corner.
    fn resize_edges(&self, x: isize, y: isize, margin: usize) -> Edges {
        let margin = margin as isize;
        let corner = margin * 3;

        let (left, top) = (self.x, self.y);
        let (right, bottom) = (
            self.x + self.width as isize,
            self.y + self.height as isize,
        );

        if x < left || x >= right || y < top || y >= bottom {
            return Edges::default();
        }

        // A window narrower than two bands would have every press count as
        // both edges at once, which reads as the window collapsing.
        let reach = margin.min(self.width as isize / 3).min(self.height as isize / 3);
        let corner = corner.min(self.width as isize / 3).min(self.height as isize / 3);

        let near_left = x < left + reach;
        let near_right = x >= right - reach;
        let near_top = y < top + reach;
        let near_bottom = y >= bottom - reach;

        let corner_left = x < left + corner;
        let corner_right = x >= right - corner;
        let corner_top = y < top + corner;
        let corner_bottom = y >= bottom - corner;

        // Within the corner square, both of its edges are grabbed even when
        // the pointer is only close enough to one of them.
        let in_corner = (corner_left || corner_right) && (corner_top || corner_bottom);
        if in_corner {
            return Edges {
                left: corner_left,
                right: corner_right,
                top: corner_top,
                bottom: corner_bottom,
            };
        }

        Edges {
            left: near_left,
            right: near_right,
            top: near_top,
            bottom: near_bottom,
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
    /// A fraction of the title bar rather than a fixed inset from it, so it
    /// stays the same shape at every text scale: subtracting a constant made
    /// it very nearly as tall as the whole bar once the bar itself scaled.
    fn close_button(&self) -> (isize, isize, usize) {
        let size = (self.title_height * 5 / 9).max(8);
        let margin = (self.title_height as isize - size as isize) / 2;
        let x = self.x + self.width as isize - size as isize - margin;
        let y = self.y + margin;
        (x, y, size)
    }

    fn contains_close(&self, x: isize, y: isize) -> bool {
        let (bx, by, size) = self.close_button();
        x >= bx && x < bx + size as isize && y >= by && y < by + size as isize
    }

    /// Inside the window, below the title bar.
    fn contains_body(&self, x: isize, y: isize) -> bool {
        x >= self.x
            && x < self.x + self.width as isize
            && y >= self.y + self.title_height as isize
            && y < self.y + self.height as isize
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
    /// Window being resized, if any. Never set at the same time as `dragging`:
    /// one press starts one or the other.
    resizing: Option<Resize>,
    /// Set when a resize changed a terminal window, so the shell knows to
    /// refill it for the new row count. The compositor cannot do it itself —
    /// the scrollback belongs to the shell.
    terminal_resized: bool,
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
    /// What the launcher is currently offering, already filtered by `search`.
    ///
    /// Rebuilt when the launcher opens and whenever the query changes, rather
    /// than recomputed per frame: the menu's height depends on how many
    /// entries there are, and a list that changed between drawing and hit
    /// testing would put the rows somewhere other than where they were drawn.
    launcher_items: Vec<LauncherEntry>,
    /// What has been typed into the launcher's search field.
    search: String,
    /// The entry the keyboard is on, as opposed to the one the pointer is on.
    selected: usize,
    /// A program the user picked from the launcher. Run by the shell, outside
    /// the compositor's lock, for the same reason everything else here is.
    run_request: Option<String>,
    /// The open context menu, if any: where it is and what is in it.
    menu: Option<(isize, isize, Vec<crate::menu::Item>)>,
    /// The menu entry the pointer is over.
    menu_hover: Option<usize>,
    /// Kept locally so one held right button produces exactly one menu.
    right_was_down: bool,
    /// Notifications currently on screen, oldest first. Drained from
    /// `notify`'s queue rather than pushed here, so anything in the kernel can
    /// raise one without touching the compositor's lock.
    notifications: Vec<crate::notify::Notification>,
    /// The rectangle the stack occupied when it was last drawn, so a card
    /// that expires erases exactly what it used to cover.
    notification_rect: Option<Damage>,
    /// The hour and minute the panel is currently showing, so the clock can
    /// be redrawn when it changes and only then. Without this the desktop
    /// would either repaint every frame to keep a clock ticking, or show the
    /// time it happened to start at forever.
    clock_shown: Option<(u8, u8)>,
    /// The launcher row the pointer is over, so exactly one row is
    /// highlighted and it is the one a click would choose.
    hovered_entry: Option<usize>,
    /// A window the user clicked for in the launcher that is not open yet.
    open_request: Option<Kind>,
    /// Something the desktop wants said in the terminal. Collected here rather
    /// than printed, because printing from inside the compositor would run
    /// while this task holds the DESKTOP lock.
    notice: Option<String>,
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
            resizing: None,
            terminal_resized: false,
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
            launcher_items: Vec::new(),
            search: String::new(),
            selected: 0,
            run_request: None,
            menu: None,
            menu_hover: None,
            right_was_down: false,
            notifications: Vec::new(),
            notification_rect: None,
            clock_shown: None,
            hovered_entry: None,
            open_request: None,
            notice: None,
            theme,
            m,
        }
    }

    /// The controls a window shows. One description, used to draw them and to
    /// decide what was clicked, so the two cannot disagree.
    fn controls(
        &self,
        kind: Kind,
        x: isize,
        y: isize,
        width: usize,
    ) -> alloc::vec::Vec<settings::Item> {
        match kind {
            Kind::Software => settings::software_layout(x, y, width, self.m.cell_w, self.m.cell_h),
            _ => settings::layout(&self.theme, x, y, width, self.m.cell_w, self.m.cell_h),
        }
    }

    /// Work out which control was clicked, and do it.
    fn click_settings(&mut self, index: usize, x: isize, y: isize) {
        let window = &self.windows[index];
        let body_y = window.y + self.m.title_height as isize;
        let items = self.controls(window.kind, window.x, body_y, window.width);

        let Some(action) = items
            .iter()
            .find(|item| item.action.is_some() && item.contains(x, y))
            .and_then(|item| item.action)
        else {
            // A click on a heading or on empty space: focus, nothing more.
            self.invalidate_all();
            return;
        };

        if let Some(message) = settings::apply(action) {
            self.notice = Some(message);
        }

        // Every action changes the theme, and the window's own controls show
        // the new values, so the whole screen is repainted either way.
        self.apply_theme(crate::theme::current());
    }

    /// Take anything the desktop wants printed, for the caller to say outside
    /// the lock.
    pub fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
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
        // being dragged or resized — simplest and safest is to drop both.
        self.dragging = None;
        self.resizing = None;
        self.focused = self.focused.min(self.windows.len().saturating_sub(1));
        self.invalidate_all();
    }

    /// The smallest a window may be dragged to.
    ///
    /// Enough title bar left to grab and to hold the close button, and enough
    /// body for a couple of rows of text. A window allowed to reach zero could
    /// not be grabbed again, which makes it unrecoverable rather than small.
    fn minimum_size(&self) -> (usize, usize) {
        let width = (self.m.title_height * 4).max(self.m.grab * 8);
        let height = self.m.title_height + self.m.cell_h * 2 + self.m.grab * 2;
        (width, height)
    }

    /// Recompute a resizing window's rectangle from where the drag started.
    fn apply_resize(&mut self, resize: Resize, x: isize, y: isize) {
        let (start_x, start_y, start_w, start_h) = resize.rect;
        let (min_w, min_h) = self.minimum_size();

        let Some(window) = self.windows.get_mut(resize.index) else {
            self.resizing = None;
            return;
        };

        let dx = x - resize.pointer.0;
        let dy = y - resize.pointer.1;

        let mut new_x = start_x;
        let mut new_y = start_y;
        let mut new_w = start_w as isize;
        let mut new_h = start_h as isize;

        // A pulled left or top edge moves the origin as well as the size; a
        // right or bottom edge only changes the size.
        if resize.edges.left {
            new_x = start_x + dx;
            new_w = start_w as isize - dx;
        }
        if resize.edges.right {
            new_w = start_w as isize + dx;
        }
        if resize.edges.top {
            new_y = start_y + dy;
            new_h = start_h as isize - dy;
        }
        if resize.edges.bottom {
            new_h = start_h as isize + dy;
        }

        // Clamping pushes the *moving* edge back, never the anchored one, so
        // the edge the user is not touching stays exactly where it was.
        if new_w < min_w as isize {
            if resize.edges.left {
                new_x -= min_w as isize - new_w;
            }
            new_w = min_w as isize;
        }
        if new_h < min_h as isize {
            if resize.edges.top {
                new_y -= min_h as isize - new_h;
            }
            new_h = min_h as isize;
        }

        let (old_x, old_y, old_w, old_h) = (window.x, window.y, window.width, window.height);
        let (new_w, new_h) = (new_w as usize, new_h as usize);

        if (old_x, old_y, old_w, old_h) == (new_x, new_y, new_w, new_h) {
            return;
        }

        window.x = new_x;
        window.y = new_y;
        window.width = new_w;
        window.height = new_h;

        // Text held for more rows than now fit would otherwise sit in the
        // buffer unseen and reappear on the next growth.
        let rows = window.rows();
        if window.lines.len() > rows {
            let excess = window.lines.len() - rows;
            window.lines.drain(0..excess);
        }

        if window.kind == Kind::Terminal {
            self.terminal_resized = true;
        }

        // Both rectangles: the one vacated and the one now covered. Anything
        // outside them is never repainted, which is how a shrinking window
        // would otherwise leave its old edge printed on the wallpaper.
        self.invalidate_window_rect(old_x, old_y, old_w, old_h);
        self.invalidate_window_rect(new_x, new_y, new_w, new_h);
    }

    /// Did a resize change the terminal's shape since this was last asked?
    pub fn take_terminal_resized(&mut self) -> bool {
        core::mem::take(&mut self.terminal_resized)
    }

    /// A window the user asked for that does not exist yet.
    ///
    /// The compositor knows what was clicked but not how to build a window —
    /// where it goes, how big it is, what is in it — so it records the request
    /// and the shell services it on the next pass.
    pub fn take_open_request(&mut self) -> Option<Kind> {
        self.open_request.take()
    }

    /// Collect anything a drawing program has left for us, and publish where
    /// the pointer is for it to read next time.
    ///
    /// Called by the compositor's own loop, which already holds the desktop,
    /// so nothing here has to lock it.
    pub fn service_surface(&mut self) {
        if let Some((width, height, title)) = mailbox::REQUEST.lock().take() {
            self.open_surface(&title, width, height);
        }

        if mailbox::FRAME_READY.swap(false, Ordering::Relaxed) {
            let frame = mailbox::FRAME.lock();
            self.blit_surface(&frame);
        }

        mailbox::POINTER.store(self.pointer_in_surface(), Ordering::Relaxed);
    }

    /// Give a program a window to draw in, replacing any it already had.
    ///
    /// Placed roughly in the middle and clamped to the screen, because a
    /// program has no idea what else is open or how big the display is.
    pub fn open_surface(&mut self, title: &str, width: usize, height: usize) {
        // The frame adds a title bar and a border on each side.
        let frame_width = width + self.m.border * 2;
        let frame_height = height + self.m.title_height + self.m.border;

        let x = (self.width.saturating_sub(frame_width) / 2) as isize;
        let y = (self.height.saturating_sub(frame_height) / 3) as isize;

        // One surface at a time: a program that asks twice is resizing, not
        // opening a second window.
        if let Some(index) = self.find(Kind::Surface) {
            self.close(index);
        }

        let mut window = Window::new(Kind::Surface, x, y, frame_width, frame_height);
        window.title = String::from(title);
        window.surface = vec![0; width * height];
        window.surface_width = width;
        window.surface_height = height;

        self.open(window);
    }

    /// Take a frame from the program that owns the surface.
    ///
    /// A frame of the wrong size is refused rather than stretched or padded:
    /// silently accepting it would show something the program did not draw.
    pub fn blit_surface(&mut self, pixels: &[u32]) -> bool {
        let Some(index) = self.find(Kind::Surface) else {
            return false;
        };

        let window = &mut self.windows[index];
        if pixels.len() != window.surface.len() {
            return false;
        }

        window.surface.copy_from_slice(pixels);
        self.invalidate_window(index);
        true
    }

    /// The pointer's position inside the surface, packed for a system call.
    ///
    /// Returns `u64::MAX` when the pointer is outside the window, which a
    /// program reads as "not over me" rather than as a position it should
    /// draw at.
    pub fn pointer_in_surface(&self) -> u64 {
        let Some(index) = self.find(Kind::Surface) else {
            return u64::MAX;
        };

        let window = &self.windows[index];
        let (origin_x, origin_y) = window.body_origin();
        let (x, y) = self.last_cursor;

        let inside_x = x >= origin_x + self.m.border as isize
            && x < origin_x + (self.m.border + window.surface_width) as isize;
        let inside_y = y >= origin_y && y < origin_y + window.surface_height as isize;

        if !inside_x || !inside_y {
            return u64::MAX;
        }

        let local_x = (x - origin_x - self.m.border as isize) as u64;
        let local_y = (y - origin_y) as u64;
        let (left, right, middle) = crate::mouse::buttons();
        let buttons = u64::from(left) | (u64::from(right) << 1) | (u64::from(middle) << 2);

        (buttons << 32) | (local_y << 16) | local_x
    }

    /// Ask for a window, as the launcher does. Focuses it if already open.
    pub fn request_open(&mut self, kind: Kind) {
        match self.find(kind) {
            Some(index) => {
                self.focused = index;
                self.invalidate_all();
            }
            None => self.open_request = Some(kind),
        }
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

    /// How tall the launcher is, which depends on how much it is offering.
    ///
    /// Everything that positions or hit-tests the launcher goes through this
    /// and `launcher_width`, so filtering the list moves the menu rather than
    /// leaving rows drawn outside it.
    fn launcher_height(&self) -> usize {
        let rows = self.launcher_items.len().max(1);
        self.m.launcher_header as usize + rows * self.m.launcher_row_step as usize + GAP * self.m.scale
    }

    fn launcher_width(&self) -> usize {
        let longest = self
            .launcher_items
            .iter()
            .map(|entry| entry.name.chars().count().max(entry.detail.chars().count()))
            .chain(core::iter::once(longest_entry()))
            .max()
            .unwrap_or(20);

        ((longest + 4) * self.m.cell_w).max(self.m.launcher_width)
    }

    /// Y coordinate of the launcher's top edge, opening away from the panel.
    fn launcher_y(&self) -> isize {
        if self.m.panel_at_top {
            self.m.panel_height as isize + 4
        } else {
            self.height
                .saturating_sub(self.m.panel_height + 4 + self.launcher_height()) as isize
        }
    }

    /// Everything the launcher can offer, filtered by what has been typed.
    ///
    /// Windows first, since they are always there, then whatever is installed.
    /// Both are matched on their name, so typing "sn" finds Snake and typing
    /// "set" finds Settings.
    fn build_launcher_items(&mut self) {
        let query = self.search.to_ascii_lowercase();
        let matches = |name: &str| {
            query.is_empty() || name.to_ascii_lowercase().contains(query.as_str())
        };

        let mut items = Vec::new();

        for kind in Kind::ALL {
            if matches(kind.title()) {
                items.push(LauncherEntry {
                    name: String::from(kind.title()),
                    detail: String::from(kind.description()),
                    target: Target::Window(kind),
                });
            }
        }

        // Installed packages, which is what makes the search worth having:
        // the four windows fit on screen, the programs will not always.
        for package in crate::store::catalogue() {
            if crate::store::is_installed(&package.name) && matches(&package.name) {
                items.push(LauncherEntry {
                    name: package.name.clone(),
                    detail: package.summary.clone(),
                    target: Target::Program(package.name),
                });
            }
        }

        self.selected = self.selected.min(items.len().saturating_sub(1));
        self.launcher_items = items;
    }

    /// Open the launcher, or close it if it is already open.
    fn toggle_launcher(&mut self) {
        let previous = self.launcher_region();
        self.launcher_open = !self.launcher_open;

        if self.launcher_open {
            self.search.clear();
            self.selected = 0;
            self.build_launcher_items();
        } else {
            self.hovered_entry = None;
        }

        self.invalidate(previous);
        let region = self.launcher_region();
        self.invalidate(region);
    }

    fn close_launcher(&mut self) {
        if !self.launcher_open {
            return;
        }
        let region = self.launcher_region();
        self.launcher_open = false;
        self.hovered_entry = None;
        self.search.clear();
        self.invalidate(region);
    }

    /// Act on a launcher entry: raise a window, or ask for a program to run.
    fn activate_entry(&mut self, index: usize) {
        let Some(entry) = self.launcher_items.get(index).cloned() else {
            return;
        };
        self.close_launcher();

        match entry.target {
            Target::Window(kind) => match self.find(kind) {
                // Focus it if it is open, otherwise ask for it to be opened -
                // which is what makes closing a window recoverable rather
                // than permanent.
                Some(index) => self.focused = index,
                None => self.open_request = Some(kind),
            },
            Target::Program(name) => self.run_request = Some(name),
        }
        self.invalidate_all();
    }

    /// A program the user asked for, taken by the shell to run.
    pub fn take_run_request(&mut self) -> Option<String> {
        self.run_request.take()
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

    /// Mix `colour` into what is already in the buffer, `alpha` out of 255.
    ///
    /// Everything with a soft edge goes through here: shadows, rounded
    /// corners, the cursor's outline. It reads the buffer back, which is only
    /// sound because the compositor draws strictly back to front - whatever is
    /// underneath has already been painted by the time anything blends onto
    /// it.
    fn blend(&mut self, x: isize, y: isize, colour: u32, alpha: u32) {
        if alpha == 0 || x < 0 || y < 0 {
            return;
        }
        let (x, y) = (x as usize, y as usize);
        if x >= self.width || y >= self.height {
            return;
        }
        if let Some(clip) = self.clip {
            if x < clip.x || x >= clip.x + clip.width || y < clip.y || y >= clip.y + clip.height {
                return;
            }
        }

        if alpha >= 255 {
            self.buffer[y * self.width + x] = colour;
            return;
        }

        let dst = self.buffer[y * self.width + x];
        let inverse = 255 - alpha;
        let channel = |shift: u32| {
            let a = (dst >> shift) & 0xFF;
            let b = (colour >> shift) & 0xFF;
            ((a * inverse + b * alpha) / 255) & 0xFF
        };
        self.buffer[y * self.width + x] = channel(16) << 16 | channel(8) << 8 | channel(0);
    }

    /// How much of the pixel at (`dx`, `dy`) from a corner's centre is inside
    /// a circle of radius `radius`, as an alpha.
    ///
    /// Sampled on a 4x4 grid rather than measured, because measuring wants a
    /// square root and the kernel has no floating-point library - only
    /// comparisons of squares, which need none.
    fn corner_alpha(dx: isize, dy: isize, radius: isize) -> u32 {
        // Distances are in quarter-pixels, then doubled, so that a sample
        // centre lands on a whole number and the comparison stays in integers.
        let limit = (8 * radius) as i64 * (8 * radius) as i64;
        let mut inside = 0;

        for sub_y in 0..4 {
            for sub_x in 0..4 {
                let u = (2 * (4 * dx + sub_x) + 1) as i64;
                let v = (2 * (4 * dy + sub_y) + 1) as i64;
                if u * u + v * v <= limit {
                    inside += 1;
                }
            }
        }
        inside * 255 / 16
    }

    /// How far a row is set in from the straight edge, inside a rounded corner.
    ///
    /// Shared by the fill and by anything that has to stay inside one - a
    /// program's window paints its own pixels right up to the frame, and
    /// without this it would square off the corners the frame just rounded.
    fn round_inset(radius: isize, from_edge: isize) -> isize {
        if radius <= 0 || from_edge >= radius {
            return 0;
        }
        let dy = radius - from_edge - 1;
        let mut inset = 0;
        while inset < radius {
            let dx = radius - inset - 1;
            if dx * dx + dy * dy <= radius * radius {
                break;
            }
            inset += 1;
        }
        inset
    }

    /// A filled rectangle whose colour blends from `top` to `bottom` and whose
    /// corners are rounded by `radius`.
    ///
    /// Every surface on the desktop is one of these: pass the same colour
    /// twice for a flat fill, and zero for square corners. Keeping it to one
    /// routine is what stops a flat theme from acquiring a stray gradient in
    /// one place and not another.
    fn fill_surface(
        &mut self,
        x: isize,
        y: isize,
        width: usize,
        height: usize,
        top: u32,
        bottom: u32,
        corners: (usize, usize),
    ) {
        if width == 0 || height == 0 {
            return;
        }
        let limit = (width / 2).min(height / 2);
        // Separate radii because most surfaces here are rounded at one end
        // only: a title bar meets the window body, and rounding that join
        // leaves the frame showing through as two notches.
        let (round_top, round_bottom) = (corners.0.min(limit) as isize, corners.1.min(limit) as isize);

        for row in 0..height {
            let colour = if top == bottom {
                top
            } else {
                logo::blend(top, bottom, row as f32 / height.max(1) as f32)
            };

            // How far this row is into a rounded corner, if at all.
            let upper = row < height / 2;
            let radius = if upper { round_top } else { round_bottom };
            let from_edge = if upper { row } else { height - 1 - row } as isize;
            if radius == 0 || from_edge >= radius {
                self.fill(x, y + row as isize, width, 1, colour);
                continue;
            }

            // The circle's centre for this corner, measured from the outermost
            // row and column of the rectangle.
            let dy = radius - from_edge - 1;
            let inset = Self::round_inset(radius, from_edge);

            let span = width.saturating_sub(2 * inset as usize);
            self.fill(x + inset, y + row as isize, span, 1, colour);

            // The pixel where the arc crosses this row is partly covered; the
            // rest of the edge is either wholly in or wholly out.
            if inset > 0 {
                let alpha = Self::corner_alpha(radius - inset, dy, radius);
                self.blend(x + inset - 1, y + row as isize, colour, alpha);
                self.blend(
                    x + width as isize - inset,
                    y + row as isize,
                    colour,
                    alpha,
                );
            }
        }
    }

    /// A soft shadow cast by the rectangle `(x, y, width, height)`.
    ///
    /// Drawn before the thing casting it, and offset downwards, so the desktop
    /// reads as lit from above. The falloff is squared - a linear one looks
    /// like a grey border rather than a shadow.
    fn draw_shadow(&mut self, x: isize, y: isize, width: usize, height: usize, extent: usize) {
        if extent == 0 || width == 0 || height == 0 {
            return;
        }
        let reach = extent as isize;
        let drop = reach / 2;
        let (left, top) = (x, y + drop);
        let (right, bottom) = (x + width as isize, y + drop + height as isize);

        for py in (top - reach)..(bottom + reach) {
            for px in (left - reach)..(right + reach) {
                // Distance outside the rectangle, on each axis.
                let dx = (left - px).max(px - right + 1).max(0);
                let dy = (top - py).max(py - bottom + 1).max(0);
                if dx == 0 && dy == 0 {
                    continue;
                }

                // An octagonal approximation of the distance: near enough for
                // something whose whole purpose is to be indistinct, and it
                // needs no square root.
                let far = dx.max(dy) + dx.min(dy) / 2;
                if far >= reach {
                    continue;
                }

                let fade = (reach - far) as u32;
                let alpha = 70 * fade * fade / (reach * reach) as u32;
                self.blend(px, py, 0x000000, alpha);
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
        self.invalidate_window_rect(x, y, width, height);
    }

    /// A window's rectangle plus the shadow it casts around it.
    ///
    /// Windows paint outside themselves now. A region that stopped at the
    /// frame would leave the shadow of a dragged window printed on the
    /// wallpaper behind it, which is exactly the class of smear this file's
    /// damage tracking exists to avoid.
    fn invalidate_window_rect(&mut self, x: isize, y: isize, width: usize, height: usize) {
        let reach = self.m.shadow as isize * 2;
        self.invalidate_rect(
            x - reach,
            y - reach,
            width + 2 * reach as usize,
            height + 2 * reach as usize,
        );
    }

    fn invalidate_all(&mut self) {
        self.damage = Some(Damage {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        });
    }

    /// Offer a keystroke to the desktop before the shell sees it.
    ///
    /// Returns true when the desktop used it. Escape is the whole reason this
    /// exists: it has always meant "leave the desktop", and now it has to mean
    /// "close whatever is open" first, or a menu would be impossible to
    /// dismiss without also throwing away the desktop behind it.
    pub fn take_key(&mut self, byte: u8) -> bool {
        use crate::keyboard::{KEY_DOWN, KEY_ESCAPE, KEY_UP};

        if byte == KEY_ESCAPE && self.menu.is_some() {
            self.close_menu();
            return true;
        }

        if !self.launcher_open {
            return false;
        }

        // With the launcher open the keyboard belongs to it. Anything it does
        // not want falls through to the shell, so a keystroke is never lost.
        let previous = self.launcher_region();
        match byte {
            KEY_ESCAPE => {
                self.close_launcher();
                return true;
            }
            b'\n' => {
                let selected = self.selected;
                self.activate_entry(selected);
                return true;
            }
            KEY_UP => self.selected = self.selected.saturating_sub(1),
            KEY_DOWN => {
                let last = self.launcher_items.len().saturating_sub(1);
                self.selected = (self.selected + 1).min(last);
            }
            0x08 => {
                if self.search.pop().is_none() {
                    return true;
                }
                self.selected = 0;
                self.build_launcher_items();
            }
            byte if (0x20..=0x7E).contains(&byte) => {
                self.search.push(byte as char);
                self.selected = 0;
                self.build_launcher_items();
            }
            _ => return false,
        }

        // The list may have grown or shrunk, so both the old rectangle and the
        // new one need repainting.
        self.invalidate(previous);
        let region = self.launcher_region();
        self.invalidate(region);
        true
    }

    /// Carry out a menu choice.
    ///
    /// Opening a window and repainting are the compositor's own business; the
    /// rest belongs to `menu`, which reports back whether the theme changed.
    fn run_menu_action(&mut self, action: crate::menu::Action) {
        use crate::menu::Action;

        match action {
            Action::Refresh => self.invalidate_all(),
            Action::Open(kind) => match self.find(kind) {
                Some(index) => {
                    self.focused = index;
                    self.invalidate_all();
                }
                None => self.open_request = Some(kind),
            },
            other => {
                if crate::menu::apply(other) {
                    self.apply_theme(crate::theme::current());
                }
            }
        }
    }

    /// Open a context menu at `(x, y)`, nudged so it always fits on screen.
    fn open_menu(&mut self, x: isize, y: isize, items: Vec<crate::menu::Item>) {
        self.close_menu();

        let (width, height) = self.menu_size(&items);
        // A menu opened near the right or bottom edge grows the other way,
        // which is what every desktop does and what stops the entries running
        // off the screen where they cannot be clicked.
        let x = x.min(self.width as isize - width as isize - self.m.gap).max(0);
        let y = y.min(self.height as isize - height as isize - self.m.gap).max(0);

        self.menu = Some((x, y, items));
        self.menu_hover = None;
        let region = self.menu_region();
        self.invalidate(region);
    }

    fn close_menu(&mut self) {
        if self.menu.is_none() {
            return;
        }
        let region = self.menu_region();
        self.menu = None;
        self.menu_hover = None;
        self.invalidate(region);
    }

    pub fn menu_open(&self) -> bool {
        self.menu.is_some()
    }

    fn menu_row_height(&self) -> usize {
        self.m.cell_h + 8 * self.m.scale
    }

    fn menu_separator_height(&self) -> usize {
        6 * self.m.scale
    }

    fn menu_size(&self, items: &[crate::menu::Item]) -> (usize, usize) {
        let longest = items
            .iter()
            .map(|item| item.label.chars().count())
            .max()
            .unwrap_or(8);

        let mut height = 2 * self.m.scale;
        for item in items {
            height += if item.is_separator() {
                self.menu_separator_height()
            } else {
                self.menu_row_height()
            };
        }
        height += 2 * self.m.scale;

        ((longest + 4) * self.m.cell_w, height)
    }

    /// Where each entry sits. The one description of the menu's geometry.
    fn menu_rows(&self) -> Vec<(usize, Damage)> {
        let Some((x, y, items)) = self.menu.as_ref() else {
            return Vec::new();
        };
        let (width, _) = self.menu_size(items);

        let mut out = Vec::new();
        let mut cursor = *y + 2 * self.m.scale as isize;

        for (index, item) in items.iter().enumerate() {
            let height = if item.is_separator() {
                self.menu_separator_height()
            } else {
                self.menu_row_height()
            };

            if let Some(rect) = self.region(*x, cursor, width, height) {
                out.push((index, rect));
            }
            cursor += height as isize;
        }
        out
    }

    /// The menu's rectangle, widened for its shadow.
    fn menu_region(&self) -> Option<Damage> {
        let (x, y, items) = self.menu.as_ref()?;
        let (width, height) = self.menu_size(items);
        let reach = self.m.shadow as isize * 2;
        self.region(
            *x - reach,
            *y - reach,
            width + 2 * reach as usize,
            height + 2 * reach as usize,
        )
    }

    /// Which entry `(x, y)` falls on, skipping separators.
    fn menu_entry_at(&self, x: isize, y: isize) -> Option<usize> {
        let items = self.menu.as_ref().map(|(_, _, items)| items)?;

        self.menu_rows()
            .into_iter()
            .find(|(index, rect)| {
                !items[*index].is_separator()
                    && x >= rect.x as isize
                    && x < (rect.x + rect.width) as isize
                    && y >= rect.y as isize
                    && y < (rect.y + rect.height) as isize
            })
            .map(|(index, _)| index)
    }

    /// Whether `(x, y)` is anywhere over the menu, separators included.
    fn in_menu(&self, x: isize, y: isize) -> bool {
        let Some((left, top, items)) = self.menu.as_ref() else {
            return false;
        };
        let (width, height) = self.menu_size(items);
        x >= *left && x < *left + width as isize && y >= *top && y < *top + height as isize
    }

    fn draw_menu(&mut self) {
        let Some((x, y, items)) = self.menu.as_ref() else {
            return;
        };
        let (x, y) = (*x, *y);
        let (width, height) = self.menu_size(items);

        let base = self.theme.launcher;
        let light = self.theme.light_text;
        let selected = self.theme.launcher_selected;
        let edge = self.theme.panel_edge;
        let corner = self.m.corner;
        let soft = self.m.soft;
        let hover = self.menu_hover;

        // Collected first: drawing borrows self mutably, and the menu is
        // borrowed from it.
        let rows: Vec<(usize, Damage, bool, String)> = self
            .menu_rows()
            .into_iter()
            .filter_map(|(index, rect)| {
                let item = items.get(index)?;
                Some((index, rect, item.is_separator(), item.label.clone()))
            })
            .collect();

        if self.m.shadow > 0 {
            self.draw_shadow(x, y, width, height, self.m.shadow);
        }
        let bottom = if soft { logo::blend(base, 0x000000, 0.10) } else { base };
        self.fill_surface(x, y, width, height, base, bottom, (corner, corner));

        let pad = 10 * self.m.scale as isize;
        for (index, rect, separator, label) in rows {
            if separator {
                // A hairline the width of the menu, inset so it reads as a
                // divider rather than a border.
                self.fill(
                    x + pad,
                    rect.y as isize + (rect.height / 2) as isize,
                    width - 2 * pad as usize,
                    self.m.scale,
                    edge,
                );
                continue;
            }

            let hovered = hover == Some(index);
            if hovered {
                let (top, bottom) = if soft {
                    (
                        logo::blend(selected, 0xFFFFFF, 0.10),
                        logo::blend(selected, 0x000000, 0.10),
                    )
                } else {
                    (selected, selected)
                };
                self.fill_surface(
                    x + 2 * self.m.scale as isize,
                    rect.y as isize,
                    width - 4 * self.m.scale,
                    rect.height,
                    top,
                    bottom,
                    (corner / 2, corner / 2),
                );
            }

            let text_y = rect.y as isize + (rect.height as isize - self.m.cell_h as isize) / 2;
            self.draw_text_within(x + pad, text_y, &label, light, width - 2 * pad as usize);
        }
    }

    /// Take anything newly posted, and retire anything that has had its time.
    ///
    /// Called at the top of every frame. Both halves are damage: a card
    /// arriving has to be painted, and a card leaving has to be erased from
    /// exactly where it was, which is what `notification_rect` remembers.
    fn service_notifications(&mut self) {
        let now = crate::apic::ticks();
        let before = self.notifications.len();

        if crate::notify::pending() {
            self.notifications.extend(crate::notify::drain());
        }

        // Oldest first, so this is a prefix and the newest card never jumps.
        self.notifications
            .retain(|item| now.saturating_sub(item.posted) < crate::notify::LIFETIME_TICKS);

        // Three at a time. More than that and they are a wall rather than a
        // message, and the oldest is the one nobody is still reading.
        while self.notifications.len() > 3 {
            self.notifications.remove(0);
        }

        if self.notifications.len() != before {
            let previous = self.notification_rect;
            let current = self.notification_bounds();
            self.invalidate(previous);
            self.invalidate(current);
            self.notification_rect = current;
        }
    }

    /// Where each card sits, newest nearest the panel.
    ///
    /// The one description of the stack's geometry: drawing walks it and so
    /// does dismissal, so a card cannot be shown in one place and closed from
    /// another.
    fn notification_cards(&self) -> Vec<(usize, Damage)> {
        let pad = 10 * self.m.scale;
        let card_width = (32 * self.m.cell_w).min(self.width / 3).max(self.m.cell_w * 12);
        let columns = ((card_width - 2 * pad) / self.m.cell_w.max(1)).max(8);
        let left = self.width.saturating_sub(card_width + 2 * self.m.gap as usize);

        // Cards grow away from the panel, whichever edge it is on.
        let downwards = self.m.panel_at_top;
        let mut edge = if downwards {
            self.panel_y() + self.m.panel_height as isize + self.m.gap
        } else {
            self.panel_y() - self.m.gap
        };

        let mut out = Vec::new();
        for (index, item) in self.notifications.iter().enumerate().rev() {
            let lines = wrap(&item.body, columns).len().max(1);
            let height = (lines + 1) * self.m.cell_h + 2 * pad;

            let top = if downwards { edge } else { edge - height as isize };
            let Some(rect) = self.region(left as isize, top, card_width, height) else {
                continue;
            };
            out.push((index, rect));

            edge = if downwards {
                edge + height as isize + self.m.gap
            } else {
                top - self.m.gap
            };
        }
        out
    }

    /// The whole stack's rectangle, widened for the shadow it casts.
    fn notification_bounds(&self) -> Option<Damage> {
        let cards = self.notification_cards();
        let first = cards.first()?.1;

        let mut left = first.x;
        let mut top = first.y;
        let mut right = first.x + first.width;
        let mut bottom = first.y + first.height;

        for (_, rect) in cards.iter().skip(1) {
            left = left.min(rect.x);
            top = top.min(rect.y);
            right = right.max(rect.x + rect.width);
            bottom = bottom.max(rect.y + rect.height);
        }

        let reach = self.m.shadow as isize * 2;
        self.region(
            left as isize - reach,
            top as isize - reach,
            (right - left) + 2 * reach as usize,
            (bottom - top) + 2 * reach as usize,
        )
    }

    fn draw_notifications(&mut self) {
        if self.notifications.is_empty() {
            return;
        }

        let pad = 10 * self.m.scale;
        let corner = self.m.corner;
        let stripe = 4 * self.m.scale;
        let body_colour = self.theme.text;
        let card = self.theme.window_body;
        let card_bottom = if self.m.soft {
            logo::blend(card, 0x000000, 0.04)
        } else {
            card
        };

        for (index, rect) in self.notification_cards() {
            let Some(item) = self.notifications.get(index).cloned() else {
                continue;
            };

            let (x, y) = (rect.x as isize, rect.y as isize);
            if self.m.shadow > 0 {
                self.draw_shadow(x, y, rect.width, rect.height, self.m.shadow);
            }

            self.fill_surface(
                x,
                y,
                rect.width,
                rect.height,
                card,
                card_bottom,
                (corner, corner),
            );
            // A coloured edge says what kind of news this is without needing a
            // word for it, and without any theme having a say in the matter.
            self.fill_surface(x, y, stripe, rect.height, item.kind.colour(), item.kind.colour(), (corner, corner));

            let text_x = x + stripe as isize + pad as isize;
            let columns = ((rect.width - 2 * pad) / self.m.cell_w.max(1)).max(8);
            let interior = rect.width.saturating_sub(stripe + 2 * pad);

            // Who is speaking, in their colour; then what they said.
            self.draw_text_within(text_x, y + pad as isize, &item.source, item.kind.colour(), interior);

            let mut row = 1;
            for line in wrap(&item.body, columns) {
                self.draw_text_within(
                    text_x,
                    y + pad as isize + (row * self.m.cell_h) as isize,
                    line,
                    body_colour,
                    interior,
                );
                row += 1;
            }
        }
    }

    /// Which launcher entry `(x, y)` falls on, if any.
    ///
    /// The one description of where the rows are: the highlight and the click
    /// both come through here, so the row that lights up is always the row
    /// that opens.
    fn entry_at(&self, x: isize, y: isize) -> Option<usize> {
        if !self.launcher_open {
            return None;
        }
        let launcher_y = self.launcher_y();
        if x < self.m.launcher_x || x >= self.m.launcher_x + self.launcher_width() as isize {
            return None;
        }
        if y < launcher_y + self.m.launcher_header {
            return None;
        }

        let offset = y - launcher_y - self.m.launcher_header;
        // Rows are shorter than their spacing; a point in the gap between them
        // is on neither.
        if offset % self.m.launcher_row_step >= self.m.launcher_row_height as isize {
            return None;
        }
        let row = (offset / self.m.launcher_row_step) as usize;
        (row < self.launcher_items.len()).then_some(row)
    }

    fn launcher_region(&self) -> Option<Damage> {
        // Widened by the shadow: the menu paints outside its own rectangle
        // now, and a region that stopped at the edge would leave the shadow
        // behind when the menu closed.
        let reach = self.m.shadow as isize * 2;
        self.region(
            self.m.launcher_x - reach,
            self.launcher_y() - reach,
            self.launcher_width() + 2 * reach as usize,
            self.launcher_height() + 2 * reach as usize,
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

        self.service_notifications();

        // A minute passing is damage like any other. Checked before the damage
        // is taken, so a desktop with nothing else happening still ticks.
        if self.theme.show_clock {
            let showing = crate::clock::now().map(|now| (now.hour, now.minute));
            if showing != self.clock_shown {
                self.clock_shown = showing;
                let (panel_y, height) = (self.panel_y(), self.m.panel_height);
                self.invalidate_rect(0, panel_y, self.width, height);
            }
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
            let x = self.m.shortcut_x.max(0) as usize;
            let top = self.m.shortcut_top;
            let step = self.m.shortcut_step;
            self.draw_shortcut(x, top.max(0) as usize, "Terminal", "Shell");
            self.draw_shortcut(x, (top + step).max(0) as usize, "System", "Monitor");
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
        // Above the windows and the panel, below the cursor: a notification is
        // the system speaking over whatever else is on screen.
        self.draw_notifications();
        self.draw_menu();
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

        let icon = self.m.shortcut_icon;
        let scale = self.m.scale;
        let corner = if self.m.soft { icon / 5 } else { 0 };
        let rim = 2 * scale;

        if self.m.shadow > 0 {
            self.draw_shadow(x as isize, y as isize, icon, icon, self.m.shadow / 2);
        }

        // A pale rim around a coloured tile, so the icon reads against both a
        // dark wallpaper and a light one without either being chosen for it.
        self.fill_surface(
            x as isize,
            y as isize,
            icon,
            icon,
            light,
            light,
            (corner, corner),
        );
        let (top, bottom) = if self.m.soft {
            (
                logo::blend(accent, 0xFFFFFF, 0.12),
                logo::blend(accent, 0x000000, 0.12),
            )
        } else {
            (accent, accent)
        };
        self.fill_surface(
            x as isize + rim as isize,
            y as isize + rim as isize,
            icon - 2 * rim,
            icon - 2 * rim,
            top,
            bottom,
            (corner.saturating_sub(rim), corner.saturating_sub(rim)),
        );
        self.draw_logo(x + icon / 6, y + icon / 8, icon - icon / 3);

        // The two labels straddle the icon's middle, so the pair reads as one
        // block against the icon however tall the text is.
        let label_x = x as isize + self.m.shortcut_label_x;
        let cell_h = self.m.cell_h as isize;
        let first = y as isize + (icon as isize - 2 * cell_h) / 2;
        self.draw_text_within(label_x, first, name, label, label_width);
        self.draw_text_within(
            label_x,
            first + cell_h,
            detail,
            logo::blend(label, self.theme.desktop_top, 0.35),
            label_width,
        );
    }

    /// Whether `(x, y)` is inside the shortcut whose icon starts at `top`.
    ///
    /// The label is part of the shortcut as far as a user is concerned, so the
    /// target covers the text too — measured from where the text is actually
    /// drawn rather than guessed, which is how it came to stop short of it.
    fn in_shortcut(&self, x: isize, y: isize, top: isize) -> bool {
        x >= self.m.shortcut_x
            && x < self.m.shortcut_x
                + self.m.shortcut_label_x
                + self.m.shortcut_label_width as isize
            && y >= top
            && y < top + self.m.shortcut_icon as isize
    }

    fn draw_panel(&mut self) {
        let y = self.panel_y();
        let (panel, edge) = (self.theme.panel, self.theme.panel_edge);
        let (light, accent) = (self.theme.panel_text, self.theme.title_active);
        let height = self.m.panel_height;
        // Text sits one line in from the top of the bar, centred by eye.
        let text_y = y + (height as isize - self.m.cell_h as isize) / 2;
        let inset = self.m.gap;
        let box_height = height.saturating_sub(2 * inset as usize);
        let box_y = y + inset;
        let corner = if self.m.soft { self.m.corner } else { 0 };
        let soft = self.m.soft;

        let (top_shade, bottom_shade) = if soft {
            (
                logo::blend(panel, 0xFFFFFF, 0.05),
                logo::blend(panel, 0x000000, 0.12),
            )
        } else {
            (panel, panel)
        };
        self.fill_surface(0, y, self.width, height, top_shade, bottom_shade, (0, 0));

        // The rule goes along whichever edge faces the rest of the screen, and
        // is a scaled pixel thick so it does not vanish on a dense display.
        let thickness = self.m.scale;
        let rule_y = if self.m.panel_at_top {
            y + height as isize - thickness as isize
        } else {
            y
        };
        self.fill(0, rule_y, self.width, thickness, edge);

        // Launcher button, task strip, and a small status area.  A clock needs
        // a time service; until that exists this says whatever the user wants.
        let (accent_top, accent_bottom) = if soft {
            (
                logo::blend(accent, 0xFFFFFF, 0.12),
                logo::blend(accent, 0x000000, 0.12),
            )
        } else {
            (accent, accent)
        };
        self.fill_surface(
            self.m.launcher_x,
            box_y,
            self.m.launcher_button_width,
            box_height,
            accent_top,
            accent_bottom,
            (corner, corner),
        );
        self.draw_text_within(
            self.m.launcher_x + 8 * thickness as isize,
            text_y,
            "Kestrel",
            light,
            self.m.launcher_button_width - 12 * thickness,
        );

        let mut x = self.m.task_strip_x;
        let titles: Vec<String> = self.windows.iter().map(|window| window.title.clone()).collect();
        for (index, title) in titles.iter().enumerate() {
            let colour = if index == self.focused {
                self.theme.launcher_selected
            } else {
                self.theme.launcher
            };
            let (button_top, button_bottom) = if soft {
                (
                    logo::blend(colour, 0xFFFFFF, 0.10),
                    logo::blend(colour, 0x000000, 0.10),
                )
            } else {
                (colour, colour)
            };
            self.fill_surface(
                x,
                box_y,
                self.m.task_width,
                box_height,
                button_top,
                button_bottom,
                (corner, corner),
            );
            // The panel is intentionally compact.  A window's first word is
            // readable here while its full title remains in the title bar.
            let label = title.split_whitespace().next().unwrap_or(title);
            let ink = if index == self.focused {
                light
            } else {
                logo::blend(light, colour, 0.25)
            };
            self.draw_text_within(
                x + 7 * thickness as isize,
                text_y,
                label,
                ink,
                self.m.task_width - 14 * thickness,
            );
            x += self.m.task_step;
        }

        // ---- the status area, at the far end of the panel ----------------
        //
        // Right-aligned from its own measured width, so it cannot run off the
        // edge, and never allowed to collide with the last task button.
        let mut fields: Vec<String> = Vec::new();

        if self.theme.show_clock {
            fields.push(match crate::net::config(|config| config.ip) {
                Some(ip) => alloc::format!("{ip}"),
                None => String::from("offline"),
            });

            let used = crate::memory::allocated_frames();
            let total = (crate::memory::usable_bytes() / 4096).max(1);
            fields.push(alloc::format!("mem {}%", used * 100 / total));

            if let Some(now) = crate::clock::now() {
                fields.push(alloc::format!("{:02}:{:02}", now.hour, now.minute));
            }
        }

        if self.theme.show_status && !self.theme.status_text.is_empty() {
            fields.insert(0, self.theme.status_text.clone());
        }

        if fields.is_empty() {
            return;
        }

        // Three characters of separator between fields: " | ".
        let characters: usize =
            fields.iter().map(|field| field.chars().count()).sum::<usize>() + 3 * (fields.len() - 1);
        let text_width = characters * self.m.cell_w;
        let pad = 10 * thickness;

        if self.width < text_width + pad * 4 {
            return;
        }

        let start = (self.width - text_width - pad * 2) as isize;
        if start <= x + self.m.gap {
            return;
        }

        // One tray, rather than loose text floating at the end of the bar.
        let tray = self.theme.launcher;
        let (tray_top, tray_bottom) = if soft {
            (
                logo::blend(tray, 0xFFFFFF, 0.08),
                logo::blend(tray, 0x000000, 0.08),
            )
        } else {
            (tray, tray)
        };
        self.fill_surface(
            start,
            box_y,
            text_width + pad * 2,
            box_height,
            tray_top,
            tray_bottom,
            (corner, corner),
        );

        // Drawn field by field so the separators can be quieter than the
        // values and the clock brighter than either.
        let quiet = logo::blend(light, tray, 0.30);
        let faint = logo::blend(light, tray, 0.62);
        let last = fields.len() - 1;
        let mut cursor = start + pad as isize;

        for (index, field) in fields.iter().enumerate() {
            let ink = if index == last && self.theme.show_clock { light } else { quiet };
            self.draw_text(cursor, text_y, field, ink);
            cursor += (field.chars().count() * self.m.cell_w) as isize;

            if index != last {
                self.draw_text(cursor, text_y, " | ", faint);
                cursor += (3 * self.m.cell_w) as isize;
            }
        }
    }

    fn draw_launcher(&mut self) {
        let y = self.launcher_y();
        let (width, height) = (self.launcher_width(), self.launcher_height());
        let light = self.theme.light_text;
        let x = self.m.launcher_x;
        let scale = self.m.scale;
        let corner = self.m.corner;
        let soft = self.m.soft;
        let base = self.theme.launcher;

        // Closing the launcher only invalidates the launcher's own rectangle
        // (plus its shadow), so nothing here may be drawn beyond that.
        if self.m.shadow > 0 {
            self.draw_shadow(x, y, width, height, self.m.shadow);
        }
        let bottom = if soft { logo::blend(base, 0x000000, 0.10) } else { base };
        self.fill_surface(x, y, width, height, base, bottom, (corner, corner));
        self.fill(x, y, width, scale, self.theme.panel_edge);

        // The search field, which doubles as the heading: it says what this
        // menu is for, and shows what has been typed at it.
        let query = self.search.clone();
        let prompt = if query.is_empty() {
            String::from("Search applications")
        } else {
            alloc::format!("{query}_")
        };
        let quiet = logo::blend(light, base, 0.45);
        self.draw_text_within(
            x + 12 * scale as isize,
            y + 10 * scale as isize,
            &prompt,
            if query.is_empty() { quiet } else { light },
            width - 24 * scale,
        );

        // Walks the list the hit testing walks, so the row that lights up is
        // the row that opens.
        let entries: Vec<(String, String)> = self
            .launcher_items
            .iter()
            .map(|entry| (entry.name.clone(), entry.detail.clone()))
            .collect();
        let row_x = x + self.m.gap;
        let row_width = width - 2 * self.m.gap as usize;
        let text_width = row_width - 20 * scale;

        if entries.is_empty() {
            self.draw_text_within(
                row_x + 10 * scale as isize,
                y + self.m.launcher_header,
                "Nothing matches",
                quiet,
                text_width,
            );
            return;
        }

        for (index, (name, description)) in entries.iter().enumerate() {
            let row_y = y + self.m.launcher_header + index as isize * self.m.launcher_row_step;

            // Only the row under the pointer is highlighted. Painting every
            // row in the selection colour, which is what this used to do, made
            // a menu of four identical buttons and hid which one was about to
            // be chosen.
            // The pointer wins while it is over a row; otherwise the
            // keyboard's own selection is what is highlighted, so typing and
            // pressing Enter is a complete way to use this.
            let hovered = match self.hovered_entry {
                Some(row) => row == index,
                None => self.selected == index,
            };
            let selected = self.theme.launcher_selected;
            if hovered {
                let (top, bottom) = if soft {
                    (
                        logo::blend(selected, 0xFFFFFF, 0.10),
                        logo::blend(selected, 0x000000, 0.10),
                    )
                } else {
                    (selected, selected)
                };
                self.fill_surface(
                    row_x,
                    row_y,
                    row_width,
                    self.m.launcher_row_height,
                    top,
                    bottom,
                    (corner, corner),
                );
            }

            let backdrop = if hovered { selected } else { base };
            // One cell apart, so the description clears the name.
            self.draw_text_within(row_x + 10 * scale as isize, row_y + 4 * scale as isize, name, light, text_width);
            self.draw_text_within(
                row_x + 10 * scale as isize,
                row_y + 4 * scale as isize + self.m.cell_h as isize,
                description,
                logo::blend(light, backdrop, 0.40),
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
        let corner = self.m.corner;
        let inner_corner = corner.saturating_sub(border);
        let soft = self.m.soft;

        // The shadow goes down first, so the window sits on top of its own.
        // Only the focused window casts a full one; the others are further
        // back and say so by casting less.
        if self.m.shadow > 0 {
            let extent = if focused { self.m.shadow } else { self.m.shadow / 2 };
            self.draw_shadow(x, y, width, height, extent);
        }

        // Frame, then body inset by the border width.
        let frame = self.theme.window_border;
        self.fill_surface(x, y, width, height, frame, frame, (corner, corner));

        let body = self.theme.window_body;
        // A body that is very slightly darker at the bottom reads as a surface
        // rather than a hole. Two percent: any more and it looks dirty.
        let body_bottom = if soft { logo::blend(body, 0x000000, 0.02) } else { body };
        self.fill_surface(
            x + inset,
            y + title_height as isize,
            width.saturating_sub(border * 2),
            height.saturating_sub(title_height + border),
            body,
            body_bottom,
            (0, inner_corner),
        );

        let title_colour = if focused {
            self.theme.title_active
        } else {
            self.theme.title_inactive
        };
        // Lit from above, like everything else: lighter at the top edge,
        // deeper at the join with the body.
        let (title_top, title_bottom) = if soft {
            (
                logo::blend(title_colour, 0xFFFFFF, 0.10),
                logo::blend(title_colour, 0x000000, 0.10),
            )
        } else {
            (title_colour, title_colour)
        };
        self.fill_surface(
            x + inset,
            y + inset,
            width.saturating_sub(border * 2),
            title_height.saturating_sub(border),
            title_top,
            title_bottom,
            (inner_corner, 0),
        );

        // Centred in the title bar, so it stays put as the bar is resized.
        let title_y = y + (title_height as isize - self.m.cell_h as isize) / 2;
        let title = self.windows[index].title.clone();
        let (close_x, close_y, close_size) = self.windows[index].close_button();

        // The title stops before the close button rather than running under it.
        let pad = 6 * self.m.scale as isize;
        let title_room = (close_x - (x + pad)).max(0) as usize;
        self.draw_text_within(x + pad, title_y, &title, self.theme.light_text, title_room);

        // A cross, drawn as two diagonals rather than a glyph so it stays
        // square and centred at any title-bar height. The strokes thicken with
        // the scale, or the button would be a hairline on a 4K panel.
        let mark = self.theme.light_text;
        let button = if soft {
            logo::blend(title_colour, 0x000000, 0.22)
        } else {
            self.theme.window_border
        };
        let button_corner = if soft { close_size / 3 } else { 0 };
        self.fill_surface(
            close_x,
            close_y,
            close_size,
            close_size,
            button,
            button,
            (button_corner, button_corner),
        );

        let thickness = self.m.scale.max(1);
        let margin = close_size / 4;
        for step in margin..close_size.saturating_sub(margin) {
            let far = close_size - 1 - step;
            for offset in 0..thickness {
                let nudge = offset as isize;
                self.blend(close_x + step as isize, close_y + step as isize + nudge, mark, 255);
                self.blend(close_x + step as isize, close_y + far as isize + nudge, mark, 255);
            }
        }

        // A program's window shows whatever it last drew.
        if self.windows[index].kind == Kind::Surface {
            let (origin_x, origin_y) = self.windows[index].body_origin();
            self.draw_surface(index, origin_x + self.m.border as isize, origin_y);
            return;
        }

        // Some windows draw controls rather than text.
        if matches!(self.windows[index].kind, Kind::Settings | Kind::Software) {
            let kind = self.windows[index].kind;
            self.draw_controls(kind, x, y + title_height as isize, width);
            return;
        }

        // Wrapped to the window's interior, then clipped to it as a backstop.
        //
        // Nothing may be drawn outside the window's own rectangle: a drag
        // invalidates the rectangle it left and the one it moved to, so pixels
        // beyond that are never erased and smear across the desktop.
        let colour = if focused { self.theme.text } else { self.theme.text_dim };
        let interior = width.saturating_sub(2 * pad as usize);
        let columns = interior / self.m.cell_w.max(1);
        let cell_h = self.m.cell_h;
        let lines = self.windows[index].lines.clone();

        let mut row = 0;
        'lines: for line in lines.iter() {
            for piece in wrap(line, columns) {
                let py = y + (title_height + 4 * self.m.scale + row * cell_h) as isize;
                if py + cell_h as isize > y + height as isize {
                    break 'lines;
                }
                self.draw_text_within(x + pad, py, piece, colour, interior);
                row += 1;
            }
        }
    }

    /// The Settings window's controls.
    ///
    /// Walks the same list `handle_mouse` hit-tests against, so a control
    /// cannot be drawn somewhere it cannot be clicked.
    /// Copy a program's pixels into the compositor's buffer.
    ///
    /// Through `plot`, so the damage clip applies exactly as it does to
    /// everything else — a program's window is composited like any other and
    /// cannot paint outside its own rectangle.
    fn draw_surface(&mut self, index: usize, x: isize, y: isize) {
        let (mut width, mut height) = (
            self.windows[index].surface_width,
            self.windows[index].surface_height,
        );

        // A surface is whatever size the program asked for, and the window can
        // now be dragged smaller than that. `plot` clips to the damage region,
        // not to the window, so without this a shrunken window would let the
        // program keep painting over the desktop beyond its own frame — the
        // same class of bug as text escaping a window, and just as invisible
        // until something moves.
        let window = &self.windows[index];
        let interior_w = window.width.saturating_sub(2 * self.m.border);
        let interior_h = window
            .height
            .saturating_sub(window.title_height + self.m.border);
        width = width.min(interior_w);
        height = height.min(interior_h);

        // The rows are copied out of a buffer `surface_width` wide, so a
        // narrowed view still has to step by the original stride.
        let stride = self.windows[index].surface_width;
        if stride == 0 || width == 0 || height == 0 {
            return;
        }
        // Cloned because plot borrows self mutably. A frame is copied on blit
        // anyway, so this is the same cost the design already accepted.
        let pixels = self.windows[index].surface.clone();

        // A program's surface reaches the bottom edge of its window, so it has
        // to respect the same rounding the body was drawn with or it squares
        // the two bottom corners off again.
        let corner = self.m.corner.saturating_sub(self.m.border) as isize;

        for row in 0..height {
            let py = y + row as isize;
            if py < 0 {
                continue;
            }
            let inset = Self::round_inset(corner, (height - 1 - row) as isize) as usize;

            for column in inset..width.saturating_sub(inset) {
                let px = x + column as isize;
                if px < 0 {
                    continue;
                }
                self.plot(px as usize, py as usize, pixels[row * stride + column]);
            }
        }
    }

    fn draw_controls(&mut self, kind: Kind, x: isize, y: isize, width: usize) {
        let items = self.controls(kind, x, y, width);

        for item in items {
            let (fill, ink) = match item.style {
                settings::Style::Heading => (None, self.theme.text_dim),
                settings::Style::Label | settings::Style::Value => (None, self.theme.text),
                settings::Style::Button => (Some(self.theme.launcher), self.theme.light_text),
                settings::Style::Active => {
                    (Some(self.theme.launcher_selected), self.theme.light_text)
                }
            };

            if let Some(colour) = fill {
                self.fill(item.x, item.y, item.width, item.height, colour);
            }

            // Anything sitting in a box — a button, or a value between two of
            // them — is centred in it. Headings and row labels start at the
            // left, where the eye scans for them.
            let boxed = fill.is_some() || item.style == settings::Style::Value;
            let text_x = if boxed {
                let text_width = item.label.chars().count() * self.m.cell_w;
                item.x + (item.width as isize - text_width as isize).max(0) / 2
            } else {
                item.x
            };
            let text_y = if boxed {
                item.y + (item.height as isize - self.m.cell_h as isize) / 2
            } else {
                item.y
            };

            self.draw_text_within(text_x, text_y, &item.label, ink, item.width);
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
        let scale = self.m.scale as isize;

        for (row, line) in ARROW.iter().enumerate() {
            for (column, glyph) in line.bytes().enumerate() {
                let colour = match glyph {
                    b'#' => self.theme.cursor_edge,
                    b'*' => self.theme.cursor,
                    _ => continue,
                };
                // Magnified with the rest of the desktop: a 12-pixel arrow is
                // a speck on the display that needed scale 3 in the first
                // place.
                for down in 0..scale {
                    for across in 0..scale {
                        let x = mx + column as isize * scale + across;
                        let y = my + row as isize * scale + down;
                        if x >= 0 && y >= 0 {
                            self.plot(x as usize, y as usize, colour);
                        }
                    }
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

        // The cursor itself is damage: erase its old arrow and draw it at the
        // new location.  When it has not moved, an idle desktop costs no
        // framebuffer work at all. The arrow is a 12x12 bitmap magnified by
        // the text scale, so the region has to be scaled with it.
        let arrow = CURSOR_SIZE * self.m.scale;
        if (x, y) != self.last_cursor {
            let (old_x, old_y) = self.last_cursor;
            self.invalidate_rect(old_x, old_y, arrow, arrow);
            self.invalidate_rect(x, y, arrow, arrow);
            self.last_cursor = (x, y);
        }

        // Which launcher row the pointer is over. Tracked on every move, not
        // just on a click, because the highlight is what tells the user what
        // the click will do.
        let hovered = self.entry_at(x, y);
        if hovered != self.hovered_entry {
            self.hovered_entry = hovered;
            self.invalidate(self.launcher_region());
        }

        // The same, for the context menu.
        let over = self.menu_entry_at(x, y);
        if over != self.menu_hover {
            self.menu_hover = over;
            let region = self.menu_region();
            self.invalidate(region);
        }

        // The right button opens a menu, on its own press edge. Handled before
        // the left button's early return, or a menu could only be opened
        // while something else was already held down.
        let (_, right, _) = crate::mouse::buttons();
        if right {
            if !self.right_was_down {
                self.right_was_down = true;

                let panel_y = self.panel_y();
                let on_panel = y >= panel_y && y < panel_y + self.m.panel_height as isize;
                let on_window = self.windows.iter().any(|window| {
                    x >= window.x
                        && x < window.x + window.width as isize
                        && y >= window.y
                        && y < window.y + window.height as isize
                });

                if on_panel {
                    self.open_menu(x, y - self.menu_size(&crate::menu::panel()).1 as isize, crate::menu::panel());
                } else if !on_window && !self.launcher_open {
                    self.open_menu(x, y, crate::menu::desktop());
                }
            }
        } else {
            self.right_was_down = false;
        }

        if !left {
            self.dragging = None;
            self.resizing = None;
            self.left_was_down = false;
            return;
        }

        if let Some(resize) = self.resizing {
            self.apply_resize(resize, x, y);
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
                    self.invalidate_window_rect(old_x, old_y, width, height);
                    self.invalidate_window_rect(new_x, new_y, width, height);
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

        // An open menu takes the next click, wherever it lands: choosing an
        // entry, or dismissing the menu without also acting on whatever was
        // underneath it.
        if self.menu.is_some() {
            let chosen = self
                .menu_entry_at(x, y)
                .and_then(|index| self.menu.as_ref()?.2.get(index)?.action);
            let inside = self.in_menu(x, y);
            self.close_menu();

            if let Some(action) = chosen {
                self.run_menu_action(action);
            }
            if inside || chosen.is_some() {
                return;
            }
            return;
        }

        // A notification is dismissed by clicking it, and takes the click with
        // it: the window underneath was covered when the user aimed.
        if !self.notifications.is_empty() {
            let hit = self
                .notification_cards()
                .into_iter()
                .find(|(_, rect)| {
                    x >= rect.x as isize
                        && x < (rect.x + rect.width) as isize
                        && y >= rect.y as isize
                        && y < (rect.y + rect.height) as isize
                })
                .map(|(index, _)| index);

            if let Some(index) = hit {
                let previous = self.notification_bounds();
                self.notifications.remove(index);
                let current = self.notification_bounds();
                self.invalidate(previous);
                self.invalidate(current);
                self.notification_rect = current;
                return;
            }
        }

        let panel_y = self.panel_y();
        let in_panel = y >= panel_y && y < panel_y + self.m.panel_height as isize;
        if in_panel {
            if x >= self.m.launcher_x
                && x < self.m.launcher_x + self.m.launcher_button_width as isize
            {
                self.toggle_launcher();
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
            let inside_x =
                x >= self.m.launcher_x && x < self.m.launcher_x + self.launcher_width() as isize;

            if inside_x && y >= launcher_y + self.m.launcher_header {
                match self.entry_at(x, y) {
                    Some(row) => self.activate_entry(row),
                    None => {
                        // Inside the menu but between rows: not a choice, but
                        // not a dismissal either.
                        self.close_launcher();
                        self.invalidate_all();
                    }
                }
                return;
            }
            self.close_launcher();
        }

        // The desktop shortcuts mirror the launcher entries, and behave the
        // same way: focus what is open, open what is not. By kind rather than
        // by index — a closed window shifts every index after it, so "shortcut
        // two means window one" stops being true the moment anything closes.
        if self.theme.show_shortcuts {
            let tops = [self.m.shortcut_top, self.m.shortcut_top + self.m.shortcut_step];
            for (row, top) in tops.iter().enumerate() {
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
            // Before the title bar, so the top edge resizes rather than
            // moves. The band is a few pixels and the bar is tens of them, so
            // there is still plenty of bar left to drag by.
            let edges = self.windows[index].resize_edges(x, y, self.m.grab);
            if edges.any() {
                self.focused = index;
                self.resizing = Some(Resize {
                    index,
                    edges,
                    rect: (
                        self.windows[index].x,
                        self.windows[index].y,
                        self.windows[index].width,
                        self.windows[index].height,
                    ),
                    pointer: (x, y),
                });
                self.invalidate_all();
                return;
            }

            if self.windows[index].contains_title(x, y) {
                self.focused = index;
                self.dragging = Some((index, x - self.windows[index].x, y - self.windows[index].y));
                self.invalidate_all();
                return;
            }

            // A click in the body of the Settings window works a control.
            // Checked after the title bar so dragging still wins there.
            if matches!(self.windows[index].kind, Kind::Settings | Kind::Software)
                && self.windows[index].contains_body(x, y)
            {
                self.focused = index;
                self.click_settings(index, x, y);
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

/// Whether a desktop is running, readable without taking any lock.
pub static ACTIVE: AtomicBool = AtomicBool::new(false);

/// What a drawing program has asked for, and what it has drawn.
///
/// A system call must never take the `DESKTOP` lock. `desktop::with` disables
/// interrupts and then spins for it, so a program blocking there on the same
/// core the compositor runs on stops that core from ever running the
/// compositor again — the lock is never released and the machine is wedged.
/// Requests are left here instead and collected by the compositor on its own
/// terms, the same way terminal output and Settings notices already work.
mod mailbox {
    use super::*;

    pub static REQUEST: Mutex<Option<(usize, usize, String)>> = Mutex::new(None);
    pub static FRAME: Mutex<Vec<u32>> = Mutex::new(Vec::new());
    pub static FRAME_READY: AtomicBool = AtomicBool::new(false);
    /// Width and height of the surface in force, packed, so `blit` can check a
    /// frame's size without reaching into the compositor.
    pub static SIZE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    /// The pointer's position inside the surface, published each frame.
    pub static POINTER: core::sync::atomic::AtomicU64 =
        core::sync::atomic::AtomicU64::new(u64::MAX);
}

/// Ask for a window to draw in. Called from a system call, so it only records
/// the request.
pub fn request_surface(title: &str, width: usize, height: usize) -> bool {
    if !ACTIVE.load(Ordering::Relaxed) {
        return false;
    }

    *mailbox::REQUEST.lock() = Some((width, height, String::from(title)));
    mailbox::SIZE.store(((width as u64) << 32) | height as u64, Ordering::Relaxed);
    // Anything half-drawn for the previous size would be the wrong shape now.
    mailbox::FRAME_READY.store(false, Ordering::Relaxed);
    true
}

/// Hand over a frame. Refused if it is not the size that was asked for.
pub fn submit_frame(pixels: &[u32]) -> bool {
    let packed = mailbox::SIZE.load(Ordering::Relaxed);
    let expected = (packed >> 32) as usize * (packed & 0xFFFF_FFFF) as usize;
    if expected == 0 || pixels.len() != expected {
        return false;
    }

    let mut frame = mailbox::FRAME.lock();
    frame.clear();
    frame.extend_from_slice(pixels);
    mailbox::FRAME_READY.store(true, Ordering::Relaxed);
    true
}

/// Where the pointer was inside the surface, as of the last frame drawn.
pub fn surface_pointer() -> u64 {
    mailbox::POINTER.load(Ordering::Relaxed)
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
/// Offer a keystroke to the desktop. See `Desktop::take_key`.
pub fn take_key(byte: u8) -> bool {
    with(|desktop| desktop.take_key(byte)).unwrap_or(false)
}

pub fn note_screen_disturbed() {
    SCREEN_DISTURBED.store(true, Ordering::Relaxed);
}

pub fn with<T>(body: impl FnOnce(&mut Desktop) -> T) -> Option<T> {
    x86_64::instructions::interrupts::without_interrupts(|| DESKTOP.lock().as_mut().map(body))
}

pub fn active() -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| DESKTOP.lock().is_some())
}




