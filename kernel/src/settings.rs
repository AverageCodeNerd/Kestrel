//! The Settings window's contents: what it shows, and what clicking does.
//!
//! Kept apart from the compositor because the compositor's job is to draw
//! rectangles, not to know what "panel height" means. It is also kept apart
//! from `theme` — that module owns what a setting *is*, and this one owns how
//! it is presented.
//!
//! The important rule: `layout` is the only description of where anything is.
//! Drawing walks the list it returns, and so does hit testing, so a control
//! cannot be drawn in one place and clicked in another. Every layout bug this
//! desktop has had came from those two being written out separately.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::theme::{self, Theme};

/// What clicking a control does.
#[derive(Clone, Copy)]
pub enum Action {
    Preset(&'static str),
    /// Flip an on/off setting.
    Toggle(&'static str),
    /// Add to a numeric setting, clamped by `theme` itself.
    Step(&'static str, isize),
    NextWallpaper,
    Save,
    Reset,
    /// Install or remove the catalogue entry at this position.
    ///
    /// An index rather than a name so the action stays `Copy`, and safe to use
    /// because both the layout and this act on the same catalogue in the same
    /// order.
    Install(usize),
    Remove(usize),
}

/// How an item is drawn.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// A section title. Not clickable.
    Heading,
    /// The name of a setting, on the left of its row. Not clickable.
    Label,
    /// A number between its two stepper buttons. Centred, but no box — it is
    /// showing a value, not offering to be pressed.
    Value,
    /// A clickable box.
    Button,
    /// A clickable box that is currently the active choice.
    Active,
}

pub struct Item {
    pub x: isize,
    pub y: isize,
    pub width: usize,
    pub height: usize,
    pub label: String,
    pub style: Style,
    pub action: Option<Action>,
}

impl Item {
    pub fn contains(&self, x: isize, y: isize) -> bool {
        x >= self.x
            && x < self.x + self.width as isize
            && y >= self.y
            && y < self.y + self.height as isize
    }
}

/// Lay the window out.
///
/// `x`/`y` are the window's interior origin — below the title bar — and
/// `width`/`height` the space available inside it. Everything is measured in
/// character cells so the window stays usable at any text scale.
pub fn layout(
    theme: &Theme,
    x: isize,
    y: isize,
    width: usize,
    cell_w: usize,
    cell_h: usize,
) -> Vec<Item> {
    let mut items = Vec::new();
    let row_height = cell_h + 10;
    let step = (row_height + 6) as isize;
    let mut cursor = y + 6;

    let interior = width.saturating_sub(16);
    let left = x + 8;

    let mut heading = |items: &mut Vec<Item>, cursor: &mut isize, text: &str| {
        items.push(Item {
            x: left,
            y: *cursor,
            width: interior,
            height: cell_h,
            label: text.to_string(),
            style: Style::Heading,
            action: None,
        });
        *cursor += cell_h as isize + 8;
    };

    // ---- presets, side by side ----------------------------------------
    heading(&mut items, &mut cursor, "Presets");

    let gap = 6;
    let current = current_preset(theme);

    // Wide enough for the longest name plus a little air. Wrapping to as many
    // columns as actually fit beats squeezing them all onto one row, which
    // clipped "midnight" to "midnig~" at every window size worth using.
    let longest = theme::PRESETS
        .iter()
        .map(|name| name.chars().count())
        .max()
        .unwrap_or(8);
    let button_width = (longest + 2) * cell_w;
    let columns = (interior / (button_width + gap)).max(1);

    for (index, name) in theme::PRESETS.iter().enumerate() {
        let (column, row_index) = (index % columns, index / columns);
        items.push(Item {
            x: left + (column * (button_width + gap)) as isize,
            y: cursor + row_index as isize * step,
            width: button_width,
            height: row_height,
            label: (*name).to_string(),
            style: if current == Some(*name) { Style::Active } else { Style::Button },
            action: Some(Action::Preset(name)),
        });
    }
    cursor += theme::PRESETS.len().div_ceil(columns) as isize * step;

    // ---- the rest, one setting per row --------------------------------
    heading(&mut items, &mut cursor, "Panel");
    row(&mut items, left, &mut cursor, interior, cell_w, row_height, step,
        "At the top", Control::Toggle("panel.top", theme));
    row(&mut items, left, &mut cursor, interior, cell_w, row_height, step,
        "Height", Control::Step("panel.height", theme, 2));

    heading(&mut items, &mut cursor, "Desktop");
    row(&mut items, left, &mut cursor, interior, cell_w, row_height, step,
        "Wallpaper", Control::Cycle(theme));
    row(&mut items, left, &mut cursor, interior, cell_w, row_height, step,
        "Text size", Control::Step("scale", theme, 1));
    row(&mut items, left, &mut cursor, interior, cell_w, row_height, step,
        "Shortcuts", Control::Toggle("shortcuts", theme));
    row(&mut items, left, &mut cursor, interior, cell_w, row_height, step,
        "Status text", Control::Toggle("status", theme));

    // ---- save and reset ------------------------------------------------
    cursor += 4;
    let half = (interior - gap) / 2;
    items.push(Item {
        x: left,
        y: cursor,
        width: half,
        height: row_height,
        label: "Save".to_string(),
        style: Style::Button,
        action: Some(Action::Save),
    });
    items.push(Item {
        x: left + (half + gap) as isize,
        y: cursor,
        width: half,
        height: row_height,
        label: "Reset".to_string(),
        style: Style::Button,
        action: Some(Action::Reset),
    });

    items
}

/// The control on the right of a labelled row.
enum Control<'a> {
    Toggle(&'static str, &'a Theme),
    /// Key, theme, and how much each press changes it by.
    Step(&'static str, &'a Theme, isize),
    Cycle(&'a Theme),
}

#[allow(clippy::too_many_arguments)]
fn row(
    items: &mut Vec<Item>,
    left: isize,
    cursor: &mut isize,
    interior: usize,
    cell_w: usize,
    row_height: usize,
    step: isize,
    label: &str,
    control: Control,
) {
    items.push(Item {
        x: left,
        y: *cursor + 5,
        width: interior / 2,
        height: row_height,
        label: label.to_string(),
        style: Style::Label,
        action: None,
    });

    // Controls are right-aligned so their edges line up down the column,
    // however long the labels are.
    let right = left + interior as isize;

    match control {
        Control::Toggle(key, theme) => {
            let on = theme.get(key).as_deref() == Some("on");
            let width = 5 * cell_w;
            items.push(Item {
                x: right - width as isize,
                y: *cursor,
                width,
                height: row_height,
                label: if on { "on" } else { "off" }.to_string(),
                style: if on { Style::Active } else { Style::Button },
                action: Some(Action::Toggle(key)),
            });
        }

        Control::Step(key, theme, amount) => {
            let value = theme.get(key).unwrap_or_default();
            let button = 3 * cell_w;
            let value_width = 5 * cell_w;

            items.push(Item {
                x: right - (button * 2 + value_width) as isize,
                y: *cursor,
                width: button,
                height: row_height,
                label: "-".to_string(),
                style: Style::Button,
                action: Some(Action::Step(key, -amount)),
            });
            items.push(Item {
                x: right - (button + value_width) as isize,
                y: *cursor,
                width: value_width,
                height: row_height,
                label: value,
                style: Style::Value,
                action: None,
            });
            items.push(Item {
                x: right - button as isize,
                y: *cursor,
                width: button,
                height: row_height,
                label: "+".to_string(),
                style: Style::Button,
                action: Some(Action::Step(key, amount)),
            });
        }

        Control::Cycle(theme) => {
            let value = theme.get("wallpaper").unwrap_or_default();
            let width = 11 * cell_w;
            items.push(Item {
                x: right - width as isize,
                y: *cursor,
                width,
                height: row_height,
                label: value,
                style: Style::Button,
                action: Some(Action::NextWallpaper),
            });
        }
    }

    *cursor += step;
}

/// Which preset the current theme matches, if any.
///
/// Compared by the values themselves rather than by remembering what was last
/// clicked, so a theme loaded from disk or built up with `set` highlights the
/// preset it happens to equal, and stops highlighting it the moment it does not.
fn current_preset(theme: &Theme) -> Option<&'static str> {
    theme::PRESETS.iter().copied().find(|name| {
        theme::preset(name).is_some_and(|candidate| {
            theme::KEYS
                .iter()
                .all(|key| candidate.get(key) == theme.get(key))
        })
    })
}

/// Carry out a click. Returns a line to show in the terminal, if any.
pub fn apply(action: Action) -> Option<String> {
    match action {
        Action::Preset(name) => {
            let preset = theme::preset(name)?;
            theme::replace(preset);
            None
        }

        Action::Toggle(key) => {
            theme::with(|theme| {
                let on = theme.get(key).as_deref() == Some("on");
                theme.set(key, if on { "off" } else { "on" }).ok();
            });
            None
        }

        Action::Step(key, amount) => {
            theme::with(|theme| {
                let current: isize = theme.get(key)?.parse().ok()?;
                // Out-of-range presses are ignored rather than reported: the
                // button is being held against a limit, which is not an error.
                theme.set(key, &format!("{}", current + amount)).ok();
                Some(())
            });
            None
        }

        Action::NextWallpaper => {
            const ORDER: [&str; 4] = ["gradient", "horizontal", "solid", "grid"];
            theme::with(|theme| {
                let current = theme.get("wallpaper").unwrap_or_default();
                let index = ORDER.iter().position(|name| *name == current).unwrap_or(0);
                theme.set("wallpaper", ORDER[(index + 1) % ORDER.len()]).ok();
            });
            None
        }

        Action::Save => Some(match theme::save_to_disk() {
            Ok(()) => format!("settings saved to {}", theme::CONFIG_PATH),
            Err(e) => format!("settings: could not save: {e}"),
        }),

        Action::Reset => {
            theme::replace(Theme::default());
            None
        }

        Action::Install(index) => {
            let packages = crate::store::catalogue();
            let package = packages.get(index)?;
            Some(match crate::store::install(&package.name) {
                Ok(size) => {
                    if crate::store::persistent() {
                        format!("installed {} ({size} bytes)", package.name)
                    } else {
                        format!(
                            "installed {} ({size} bytes) - no writable disk, so until reboot",
                            package.name
                        )
                    }
                }
                Err(e) => format!("store: {e}"),
            })
        }

        Action::Remove(index) => {
            let packages = crate::store::catalogue();
            let package = packages.get(index)?;
            Some(match crate::store::remove(&package.name) {
                Ok(()) => format!("removed {}", package.name),
                Err(e) => format!("store: {e}"),
            })
        }
    }
}

/// The Software window's contents.
///
/// One row per package: its name, what it is, its size, and a button that
/// either installs it or takes it off again. Built from the same catalogue the
/// `store` command reads, so the two cannot disagree about what exists.
pub fn software_layout(
    x: isize,
    y: isize,
    width: usize,
    cell_w: usize,
    cell_h: usize,
) -> Vec<Item> {
    let mut items = Vec::new();
    let row_height = cell_h + 10;
    let step = (row_height + 8) as isize;

    let interior = width.saturating_sub(16);
    let left = x + 8;
    let right = left + interior as isize;
    let mut cursor = y + 6;

    let packages = crate::store::catalogue();

    if packages.is_empty() {
        items.push(Item {
            x: left,
            y: cursor,
            width: interior,
            height: cell_h,
            label: "Nothing available on this medium".to_string(),
            style: Style::Label,
            action: None,
        });
        return items;
    }

    items.push(Item {
        x: left,
        y: cursor,
        width: interior,
        height: cell_h,
        label: if crate::store::persistent() {
            format!("Installing to {}", crate::store::install_root())
        } else {
            "No writable disk - installs last until reboot".to_string()
        },
        style: Style::Heading,
        action: None,
    });
    cursor += cell_h as isize + 10;

    let button_width = 9 * cell_w;

    for (index, package) in packages.iter().enumerate() {
        // Name on the first line, description under it, button to the right.
        items.push(Item {
            x: left,
            y: cursor,
            width: interior.saturating_sub(button_width + 10),
            height: cell_h,
            label: package.name.clone(),
            style: Style::Label,
            action: None,
        });

        items.push(Item {
            x: left,
            y: cursor + cell_h as isize,
            width: interior.saturating_sub(button_width + 10),
            height: cell_h,
            label: package.summary.clone(),
            style: Style::Heading,
            action: None,
        });

        items.push(Item {
            x: right - button_width as isize,
            y: cursor,
            width: button_width,
            height: row_height,
            label: if package.installed { "remove" } else { "install" }.to_string(),
            style: if package.installed { Style::Active } else { Style::Button },
            action: Some(if package.installed {
                Action::Remove(index)
            } else {
                Action::Install(index)
            }),
        });

        cursor += step + cell_h as isize;
    }

    items
}
