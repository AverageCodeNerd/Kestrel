//! Notifications: how the system says something without interrupting anyone.
//!
//! A queue, not a call into the compositor. Anything in the kernel may post
//! here - a syscall, an interrupt handler, a shell command - and the desktop
//! drains it when it next draws. This is the same shape as terminal output and
//! the surface mailbox, and for the same reason: `desktop::with` disables
//! interrupts and spins for the DESKTOP lock, so posting from a task that the
//! compositor is waiting on would wedge the machine.
//!
//! When there is no desktop the queue simply drains into the console instead,
//! so a notification is never lost, only shown differently.

use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use spin::Mutex;

/// What a notification is about, which decides its colour and nothing else.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Info,
    Success,
    Warning,
    Error,
}

impl Kind {
    /// The stripe down the left of the card.
    ///
    /// Fixed colours rather than theme entries: these mean something, and a
    /// theme that made "error" the same green as "success" would be a theme
    /// that lied. They are the same values `theme`'s named colours use, so
    /// they sit correctly beside anything else on screen.
    pub fn colour(self) -> u32 {
        match self {
            Kind::Info => 0x3C8DBC,
            Kind::Success => 0x4CAF50,
            Kind::Warning => 0xE08A3C,
            Kind::Error => 0xE05252,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Info => "note",
            Kind::Success => "ok",
            Kind::Warning => "warning",
            Kind::Error => "error",
        }
    }
}

#[derive(Clone)]
pub struct Notification {
    /// Who is speaking: "Software", "Network", "Kestrel".
    pub source: String,
    pub body: String,
    pub kind: Kind,
    /// The tick it was posted on, which is what makes it expire.
    pub posted: u64,
}

/// Long enough to read a line, short enough not to be in the way.
pub const LIFETIME_TICKS: u64 = 6 * crate::apic::TIMER_FREQUENCY as u64;

/// Anything still queued after this long was posted to a desktop that did not
/// exist yet - during boot, most likely - and showing it minutes later would
/// be a lie about when it happened.
const STALE_TICKS: u64 = 10 * crate::apic::TIMER_FREQUENCY as u64;

/// Bounded: a fault that posts in a loop must not eat the heap.
const MAX_QUEUED: usize = 16;

static QUEUE: Mutex<VecDeque<Notification>> = Mutex::new(VecDeque::new());

/// Post a notification. Safe from anywhere, including interrupt context.
pub fn post(source: &str, body: &str, kind: Kind) {
    let notification = Notification {
        source: source.to_string(),
        body: body.to_string(),
        kind,
        posted: crate::apic::ticks(),
    };

    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut queue = QUEUE.lock();
        if queue.len() >= MAX_QUEUED {
            queue.pop_front();
        }
        queue.push_back(notification);
    });
}

pub fn info(source: &str, body: &str) {
    post(source, body, Kind::Info);
}

pub fn success(source: &str, body: &str) {
    post(source, body, Kind::Success);
}

pub fn warning(source: &str, body: &str) {
    post(source, body, Kind::Warning);
}

pub fn error(source: &str, body: &str) {
    post(source, body, Kind::Error);
}

/// Take everything worth showing, dropping whatever waited too long.
pub fn drain() -> alloc::vec::Vec<Notification> {
    let now = crate::apic::ticks();

    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut queue = QUEUE.lock();
        queue
            .drain(..)
            .filter(|item| now.saturating_sub(item.posted) < STALE_TICKS)
            .collect()
    })
}

pub fn pending() -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| !QUEUE.lock().is_empty())
}
