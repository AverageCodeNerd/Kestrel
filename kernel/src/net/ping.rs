//! Tracking outstanding ICMP echo requests.

use core::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, Ordering};

use crate::net::Ipv4;

/// Identifier used for our echo requests, so replies to someone else's pings
/// are ignored.
const IDENTIFIER: u16 = 0x4B45; // "KE"

static SEQUENCE: AtomicU16 = AtomicU16::new(1);
static REPLIES: AtomicU32 = AtomicU32::new(0);
/// Tick at which the last reply arrived, for a crude round-trip time.
static LAST_REPLY_TICK: AtomicU64 = AtomicU64::new(0);

pub fn on_reply(_source: Ipv4, identifier: u16, _sequence: u16) {
    if identifier != IDENTIFIER {
        return;
    }
    REPLIES.fetch_add(1, Ordering::Relaxed);
    LAST_REPLY_TICK.store(crate::apic::ticks(), Ordering::Relaxed);
}

/// Send one echo request and wait briefly for its reply.
///
/// Returns the round trip in milliseconds. The timer runs at 100 Hz, so the
/// resolution is 10 ms — enough to tell "answered" from "did not".
pub fn ping(destination: Ipv4, timeout_ms: u64) -> Result<u64, &'static str> {
    let before = REPLIES.load(Ordering::Relaxed);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let start = crate::apic::ticks();

    crate::net::ip::ping(destination, IDENTIFIER, sequence)?;

    let deadline = start + timeout_ms / 10;
    while crate::apic::ticks() < deadline {
        crate::net::poll();

        if REPLIES.load(Ordering::Relaxed) != before {
            let elapsed = LAST_REPLY_TICK.load(Ordering::Relaxed).saturating_sub(start);
            return Ok(elapsed * 10);
        }

        core::hint::spin_loop();
    }

    Err("no reply")
}
