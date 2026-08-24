//! What the context menus contain, and what choosing an entry does.
//!
//! Kept out of the compositor for the same reason the Settings window is: the
//! compositor's job is to draw rectangles and decide which one was clicked,
//! not to know what "New folder" means.
//!
//! As with `settings`, the list returned here is the only description of a
//! menu. Drawing walks it and so does hit testing, so an entry cannot be shown
//! in one place and acted on from another.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::desktop::Kind;

/// What choosing an entry does.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Repaint everything. Mostly a comfort, but every desktop has one.
    Refresh,
    Open(Kind),
    NextWallpaper,
    NewFolder,
    PanelSide,
    Restart,
    ShutDown,
}

pub struct Item {
    pub label: String,
    /// `None` for a separator, which is drawn as a rule and cannot be chosen.
    pub action: Option<Action>,
}

impl Item {
    fn entry(label: &str, action: Action) -> Self {
        Self {
            label: label.to_string(),
            action: Some(action),
        }
    }

    fn separator() -> Self {
        Self {
            label: String::new(),
            action: None,
        }
    }

    pub fn is_separator(&self) -> bool {
        self.action.is_none()
    }
}

/// The menu for a right-click on the wallpaper.
pub fn desktop() -> Vec<Item> {
    alloc::vec![
        Item::entry("Open Terminal", Action::Open(Kind::Terminal)),
        Item::entry("New Folder", Action::NewFolder),
        Item::separator(),
        Item::entry("Change Wallpaper", Action::NextWallpaper),
        Item::entry("Refresh", Action::Refresh),
        Item::separator(),
        Item::entry("Settings", Action::Open(Kind::Settings)),
    ]
}

/// The menu for a right-click on the panel.
pub fn panel() -> Vec<Item> {
    alloc::vec![
        Item::entry("System Monitor", Action::Open(Kind::Monitor)),
        Item::entry("Software", Action::Open(Kind::Software)),
        Item::separator(),
        Item::entry("Move Panel", Action::PanelSide),
        Item::entry("Settings", Action::Open(Kind::Settings)),
        Item::separator(),
        Item::entry("Restart", Action::Restart),
        Item::entry("Shut Down", Action::ShutDown),
    ]
}

/// Somewhere a new folder can actually be kept.
///
/// The real disk when there is one, and the RAM filesystem otherwise - the
/// same rule the software store follows, and the notification says which it
/// was rather than leaving the user to guess where their folder went.
fn folder_root() -> &'static str {
    if crate::vfs::is_directory("/disk") {
        "/disk"
    } else {
        ""
    }
}

/// Create `New Folder`, or `New Folder 2`, or the first number after that
/// which is not taken. Naming it after the fact is what a rename dialog is
/// for; until there is one, this at least never fails on a second click.
fn new_folder() {
    let root = folder_root();

    for attempt in 1..100 {
        let path = match attempt {
            1 => format!("{root}/New Folder"),
            n => format!("{root}/New Folder {n}"),
        };

        if crate::vfs::exists(&path) {
            continue;
        }

        match crate::vfs::mkdir(&path) {
            Ok(()) => crate::notify::success("Files", &format!("created {path}")),
            Err(e) => crate::notify::error("Files", &format!("{path}: {}", e.as_str())),
        }
        return;
    }

    crate::notify::warning("Files", "there are already a hundred new folders");
}

/// Carry out a menu choice.
///
/// Returns true when the theme changed, because that is the one outcome the
/// compositor has to react to rather than simply repaint.
pub fn apply(action: Action) -> bool {
    match action {
        // Both handled by the caller, which owns the windows and the screen.
        Action::Refresh | Action::Open(_) => false,

        Action::NextWallpaper => {
            crate::settings::apply(crate::settings::Action::NextWallpaper);
            true
        }

        Action::PanelSide => {
            crate::theme::with(|theme| {
                let top = theme.panel_at_top;
                theme.set("panel.top", if top { "off" } else { "on" }).ok();
            });
            true
        }

        Action::NewFolder => {
            new_folder();
            false
        }

        // Neither returns.
        Action::Restart => crate::power::reboot(),
        Action::ShutDown => crate::power::shutdown(),
    }
}
