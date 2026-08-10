//! Everything about the desktop's appearance that a user may change.
//!
//! The compositor used to carry these as constants. They live here instead so
//! there is exactly one place that knows what a setting is called, what it may
//! hold, and what it means — the shell's `set` command, the file written to
//! disk and the renderer all go through this one table rather than each
//! keeping its own list to fall out of step with the others.
//!
//! Nothing here can fail in a way that stops the desktop starting. An
//! unreadable file, an unknown key or a nonsense value leaves the default in
//! place; a desktop that refuses to draw because one line of configuration is
//! wrong would be a much worse bug than a wrong colour.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use spin::Mutex;

/// How the desktop background is painted.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Wallpaper {
    /// A vertical blend from `desktop_top` to `desktop_bottom`.
    Gradient,
    /// A horizontal blend, left to right.
    Horizontal,
    /// `desktop_top` alone.
    Solid,
    /// The gradient with a faint grid over it, like graph paper.
    Grid,
}

impl Wallpaper {
    fn name(self) -> &'static str {
        match self {
            Wallpaper::Gradient => "gradient",
            Wallpaper::Horizontal => "horizontal",
            Wallpaper::Solid => "solid",
            Wallpaper::Grid => "grid",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "gradient" => Some(Wallpaper::Gradient),
            "horizontal" => Some(Wallpaper::Horizontal),
            "solid" => Some(Wallpaper::Solid),
            "grid" => Some(Wallpaper::Grid),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct Theme {
    // ---- colours ----
    pub desktop_top: u32,
    pub desktop_bottom: u32,
    pub window_body: u32,
    pub window_border: u32,
    pub title_active: u32,
    pub title_inactive: u32,
    pub panel: u32,
    pub panel_edge: u32,
    pub launcher: u32,
    pub launcher_selected: u32,
    pub text: u32,
    pub text_dim: u32,
    /// Text on a coloured surface: title bars, the launcher menu.
    pub light_text: u32,
    /// Text on the panel. Separate from `light_text` because a light theme has
    /// a light panel, and reusing one colour for both makes the labels
    /// invisible on exactly the themes people reach for first.
    pub panel_text: u32,
    /// Shortcut labels, which sit directly on the wallpaper.
    pub desktop_text: u32,
    pub cursor: u32,
    pub cursor_edge: u32,

    // ---- layout ----
    /// Text magnification. The font is 8x8; everything that measures itself in
    /// characters scales with this, which is why it is not merely cosmetic.
    pub scale: usize,
    pub panel_height: usize,
    /// Put the panel along the top edge instead of the bottom.
    pub panel_at_top: bool,
    pub title_height: usize,
    pub window_border_width: usize,

    // ---- content ----
    pub show_shortcuts: bool,
    pub show_status: bool,
    /// The text in the far corner of the panel. There is no clock yet, so this
    /// is whatever the user wants to say.
    pub status_text: String,
    pub wallpaper: Wallpaper,
}

impl Default for Theme {
    /// The look Kestrel has always shipped with, so an empty config file and no
    /// config file at all produce the same desktop.
    fn default() -> Self {
        Self {
            desktop_top: 0x2B6B9A,
            desktop_bottom: 0x15324B,
            window_body: 0xF4F6F8,
            window_border: 0x6A7785,
            title_active: 0x3C8DBC,
            title_inactive: 0x6B7886,
            panel: 0x18232E,
            panel_edge: 0x405363,
            launcher: 0x263746,
            launcher_selected: 0x3C8DBC,
            text: 0x18232E,
            text_dim: 0x65717D,
            light_text: 0xF4F6F8,
            panel_text: 0xF4F6F8,
            desktop_text: 0xF4F6F8,
            cursor: 0xFFFFFF,
            cursor_edge: 0x101828,

            scale: 2,
            panel_height: 30,
            panel_at_top: false,
            title_height: 20,
            window_border_width: 1,

            show_shortcuts: true,
            show_status: true,
            status_text: String::from("Experimental OS"),
            wallpaper: Wallpaper::Gradient,
        }
    }
}

/// One character cell at the current scale.
impl Theme {
    pub fn cell_width(&self) -> usize {
        crate::font::GLYPH_WIDTH * self.scale
    }

    pub fn cell_height(&self) -> usize {
        (crate::font::GLYPH_HEIGHT + 1) * self.scale
    }
}

/// Every setting, in the order `theme` lists them.
///
/// Grouped rather than alphabetical: someone changing the look wants the
/// colours together, and someone changing the shape wants the sizes together.
pub const KEYS: &[&str] = &[
    "desktop.top",
    "desktop.bottom",
    "wallpaper",
    "window.body",
    "window.border",
    "window.border-width",
    "window.title-active",
    "window.title-inactive",
    "window.title-height",
    "panel.colour",
    "panel.edge",
    "panel.height",
    "panel.top",
    "launcher.colour",
    "launcher.selected",
    "text.dark",
    "text.dim",
    "text.light",
    "text.panel",
    "text.desktop",
    "cursor.fill",
    "cursor.edge",
    "scale",
    "shortcuts",
    "status",
    "status.text",
];

impl Theme {
    /// Read a setting back as text, in the same form `set` accepts.
    pub fn get(&self, key: &str) -> Option<String> {
        let colour = |value: u32| format!("#{value:06x}");
        let flag = |value: bool| if value { "on" } else { "off" }.to_string();

        Some(match key {
            "desktop.top" => colour(self.desktop_top),
            "desktop.bottom" => colour(self.desktop_bottom),
            "wallpaper" => self.wallpaper.name().to_string(),
            "window.body" => colour(self.window_body),
            "window.border" => colour(self.window_border),
            "window.border-width" => self.window_border_width.to_string(),
            "window.title-active" => colour(self.title_active),
            "window.title-inactive" => colour(self.title_inactive),
            "window.title-height" => self.title_height.to_string(),
            "panel.colour" => colour(self.panel),
            "panel.edge" => colour(self.panel_edge),
            "panel.height" => self.panel_height.to_string(),
            "panel.top" => flag(self.panel_at_top),
            "launcher.colour" => colour(self.launcher),
            "launcher.selected" => colour(self.launcher_selected),
            "text.dark" => colour(self.text),
            "text.dim" => colour(self.text_dim),
            "text.light" => colour(self.light_text),
            "text.panel" => colour(self.panel_text),
            "text.desktop" => colour(self.desktop_text),
            "cursor.fill" => colour(self.cursor),
            "cursor.edge" => colour(self.cursor_edge),
            "scale" => self.scale.to_string(),
            "shortcuts" => flag(self.show_shortcuts),
            "status" => flag(self.show_status),
            "status.text" => self.status_text.clone(),
            _ => return None,
        })
    }

    /// Apply a setting. The error is the message the user sees, so it says what
    /// would have been acceptable rather than merely that this was not.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        // Sizes are clamped rather than rejected at the extremes, but a value
        // that is not a number at all is a mistake worth reporting.
        let number = |what: &str, low: usize, high: usize| -> Result<usize, String> {
            match value.parse::<usize>() {
                Ok(n) if n >= low && n <= high => Ok(n),
                Ok(n) => Err(format!("{what} is {n}, but must be {low} to {high}")),
                Err(_) => Err(format!("{value:?} is not a whole number")),
            }
        };

        match key {
            "desktop.top" => self.desktop_top = parse_colour(value)?,
            "desktop.bottom" => self.desktop_bottom = parse_colour(value)?,
            "wallpaper" => {
                self.wallpaper = Wallpaper::parse(value)
                    .ok_or_else(|| "expected gradient, horizontal, solid or grid".to_string())?
            }
            "window.body" => self.window_body = parse_colour(value)?,
            "window.border" => self.window_border = parse_colour(value)?,
            "window.border-width" => self.window_border_width = number("the border", 0, 8)?,
            "window.title-active" => self.title_active = parse_colour(value)?,
            "window.title-inactive" => self.title_inactive = parse_colour(value)?,
            // Below the height of one line of text the title would be clipped
            // away entirely, taking the drag handle with it.
            "window.title-height" => self.title_height = number("the title bar", 12, 64)?,
            "panel.colour" => self.panel = parse_colour(value)?,
            "panel.edge" => self.panel_edge = parse_colour(value)?,
            "panel.height" => self.panel_height = number("the panel", 16, 96)?,
            "panel.top" => self.panel_at_top = parse_flag(value)?,
            "launcher.colour" => self.launcher = parse_colour(value)?,
            "launcher.selected" => self.launcher_selected = parse_colour(value)?,
            "text.dark" => self.text = parse_colour(value)?,
            "text.dim" => self.text_dim = parse_colour(value)?,
            "text.light" => self.light_text = parse_colour(value)?,
            "text.panel" => self.panel_text = parse_colour(value)?,
            "text.desktop" => self.desktop_text = parse_colour(value)?,
            "cursor.fill" => self.cursor = parse_colour(value)?,
            "cursor.edge" => self.cursor_edge = parse_colour(value)?,
            // Beyond 4 a single character is 32 pixels wide and almost nothing
            // fits on screen; 0 would divide by zero when measuring text.
            "scale" => self.scale = number("the text scale", 1, 4)?,
            "shortcuts" => self.show_shortcuts = parse_flag(value)?,
            "status" => self.show_status = parse_flag(value)?,
            "status.text" => self.status_text = value.to_string(),
            _ => return Err(format!("no such setting: {key}")),
        }
        Ok(())
    }

    /// The whole theme as a config file.
    pub fn serialise(&self) -> String {
        let mut out = String::from("# Kestrel desktop settings.\n");
        out.push_str("# Written by 'theme save'. Edit freely: unknown keys and\n");
        out.push_str("# bad values are ignored, and anything missing keeps its default.\n\n");

        for key in KEYS {
            if let Some(value) = self.get(key) {
                out.push_str(&format!("{key} = {value}\n"));
            }
        }
        out
    }

    /// Read a config file, ignoring anything that does not make sense.
    ///
    /// Returns the names of the lines that were skipped, so `theme load` can
    /// say what it did not understand instead of silently dropping it.
    pub fn apply_config(&mut self, text: &str) -> Vec<String> {
        let mut skipped = Vec::new();

        for (number, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let Some((key, value)) = line.split_once('=') else {
                skipped.push(format!("line {}: no '='", number + 1));
                continue;
            };

            let (key, value) = (key.trim(), value.trim());
            if let Err(e) = self.set(key, value) {
                skipped.push(format!("line {}: {e}", number + 1));
            }
        }
        skipped
    }
}

/// Accepts `#rrggbb`, `0xrrggbb` or bare `rrggbb`, plus a few names so that
/// changing a colour does not require knowing hex.
fn parse_colour(text: &str) -> Result<u32, String> {
    const NAMED: &[(&str, u32)] = &[
        ("black", 0x000000),
        ("white", 0xFFFFFF),
        ("red", 0xE05252),
        ("orange", 0xE08A3C),
        ("yellow", 0xE0C93C),
        ("green", 0x4CAF50),
        ("teal", 0x2BA098),
        ("blue", 0x3C8DBC),
        ("navy", 0x15324B),
        ("purple", 0x9B59B6),
        ("pink", 0xE0709B),
        ("grey", 0x808A94),
        ("gray", 0x808A94),
    ];

    let lowered = text.to_ascii_lowercase();
    if let Some((_, value)) = NAMED.iter().find(|(name, _)| *name == lowered) {
        return Ok(*value);
    }

    let digits = lowered
        .strip_prefix('#')
        .or_else(|| lowered.strip_prefix("0x"))
        .unwrap_or(&lowered);

    if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "{text:?} is not a colour - expected #rrggbb or a name like 'blue'"
        ));
    }

    u32::from_str_radix(digits, 16).map_err(|_| format!("{text:?} is not a colour"))
}

fn parse_flag(text: &str) -> Result<bool, String> {
    match text.to_ascii_lowercase().as_str() {
        "on" | "yes" | "true" | "1" => Ok(true),
        "off" | "no" | "false" | "0" => Ok(false),
        _ => Err(format!("{text:?} is not on or off")),
    }
}

/// Named starting points, so a user can get a very different desktop without
/// setting twenty values by hand.
pub const PRESETS: &[&str] = &["default", "midnight", "paper", "amber", "matrix"];

pub fn preset(name: &str) -> Option<Theme> {
    let mut theme = Theme::default();

    match name {
        "default" => {}

        // Near-black and low contrast, for a dark room.
        "midnight" => {
            theme.desktop_top = 0x141A24;
            theme.desktop_bottom = 0x080B10;
            theme.window_body = 0x1B222E;
            theme.window_border = 0x2E3947;
            theme.title_active = 0x2C4159;
            theme.title_inactive = 0x232C38;
            theme.panel = 0x0B0F15;
            theme.panel_edge = 0x232C38;
            theme.launcher = 0x151C26;
            theme.launcher_selected = 0x2C4159;
            theme.text = 0xC3CEDC;
            theme.text_dim = 0x71808F;
            theme.panel_text = 0xC3CEDC;
            theme.desktop_text = 0xC3CEDC;
            theme.wallpaper = Wallpaper::Solid;
        }

        // Light, flat and quiet — the opposite of the default.
        "paper" => {
            theme.desktop_top = 0xE8E4DA;
            theme.desktop_bottom = 0xD3CEC2;
            theme.window_body = 0xFBFAF7;
            theme.window_border = 0xB3ACA0;
            theme.title_active = 0x5B7C99;
            theme.title_inactive = 0xB3ACA0;
            theme.panel = 0xCFC9BC;
            theme.panel_edge = 0xB3ACA0;
            theme.launcher = 0xE8E4DA;
            theme.launcher_selected = 0x5B7C99;
            theme.text = 0x2C2A26;
            theme.text_dim = 0x6E6961;
            theme.light_text = 0xFBFAF7;
            // The panel and the wallpaper are both light here, so their labels
            // have to be dark or they vanish.
            theme.panel_text = 0x2C2A26;
            theme.desktop_text = 0x2C2A26;
            theme.cursor = 0x2C2A26;
            theme.cursor_edge = 0xFBFAF7;
            theme.wallpaper = Wallpaper::Grid;
        }

        // A warm terminal, in the spirit of an amber phosphor monitor.
        "amber" => {
            theme.desktop_top = 0x2A1B08;
            theme.desktop_bottom = 0x120A02;
            theme.window_body = 0x1A1206;
            theme.window_border = 0x6B4A16;
            theme.title_active = 0xB37A1E;
            theme.title_inactive = 0x4A3410;
            theme.panel = 0x140D03;
            theme.panel_edge = 0x6B4A16;
            theme.launcher = 0x241804;
            theme.launcher_selected = 0xB37A1E;
            theme.text = 0xFFB84D;
            theme.text_dim = 0xA6772E;
            theme.light_text = 0xFFD79A;
            theme.panel_text = 0xFFB84D;
            theme.desktop_text = 0xFFB84D;
            theme.cursor = 0xFFB84D;
            theme.cursor_edge = 0x120A02;
            theme.wallpaper = Wallpaper::Solid;
        }

        "matrix" => {
            theme.desktop_top = 0x04140A;
            theme.desktop_bottom = 0x000000;
            theme.window_body = 0x061A0C;
            theme.window_border = 0x1F6B33;
            theme.title_active = 0x2C8F44;
            theme.title_inactive = 0x14401F;
            theme.panel = 0x020C05;
            theme.panel_edge = 0x1F6B33;
            theme.launcher = 0x061A0C;
            theme.launcher_selected = 0x2C8F44;
            theme.text = 0x5BE07A;
            theme.text_dim = 0x2F8443;
            theme.light_text = 0xA6F5B8;
            theme.panel_text = 0x5BE07A;
            theme.desktop_text = 0x5BE07A;
            theme.cursor = 0x5BE07A;
            theme.cursor_edge = 0x000000;
            theme.wallpaper = Wallpaper::Grid;
        }

        _ => return None,
    }

    Some(theme)
}

/// Where settings live between boots.
///
/// On the real disk rather than the RAM filesystem, which is the whole point —
/// and at the root rather than in a directory, so saving never depends on
/// having created one. Booting the ISO leaves `/disk` absent entirely, and
/// everything here degrades to "keep the defaults" rather than failing.
pub const CONFIG_PATH: &str = "/disk/desktop.conf";

/// Read the saved theme, if there is one.
///
/// Returns the complaints about lines it could not use, so the caller can show
/// them. A missing file is not a complaint — it is the normal first boot.
pub fn load_from_disk() -> Option<Vec<String>> {
    // The free functions, not `vfs::with` — the latter hands back the in-RAM
    // filesystem, which knows nothing about the /disk mount and would happily
    // "save" settings somewhere that vanishes at power off.
    let bytes = crate::vfs::read(CONFIG_PATH).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();

    let mut theme = Theme::default();
    let skipped = theme.apply_config(&text);
    replace(theme);
    Some(skipped)
}

pub fn save_to_disk() -> Result<(), String> {
    let text = current().serialise();

    crate::vfs::write(CONFIG_PATH, text.as_bytes()).map_err(|e| e.as_str().to_string())
}

/// The live theme. The compositor takes a copy when it starts and whenever
/// this changes, rather than locking on every pixel.
pub static THEME: Mutex<Option<Theme>> = Mutex::new(None);

pub fn current() -> Theme {
    THEME.lock().clone().unwrap_or_default()
}

pub fn replace(theme: Theme) {
    *THEME.lock() = Some(theme);
}

/// Run `body` against the live theme, returning what it returns.
pub fn with<T>(body: impl FnOnce(&mut Theme) -> T) -> T {
    let mut guard = THEME.lock();
    let theme = guard.get_or_insert_with(Theme::default);
    body(theme)
}

