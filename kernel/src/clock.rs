//! Wall-clock time, read from the CMOS real-time clock.
//!
//! Kestrel could route a TCP stream across the internet before it could tell
//! you the time, which is the wrong way round. This is the smallest thing that
//! fixes it: the RTC the firmware has already set, read over ports 0x70/0x71.
//!
//! No time zones and no NTP yet. The RTC is read as it stands, which on a PC
//! is whatever the firmware decided - UTC on most virtual machines, local time
//! on a good many real ones. `set clock.offset` shifts it by whole hours,
//! which is honest about the limitation rather than pretending to know better.

use core::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use x86_64::instructions::interrupts;
use x86_64::instructions::port::Port;

const ADDRESS: u16 = 0x70;
const DATA: u16 = 0x71;

const SECONDS: u8 = 0x00;
const MINUTES: u8 = 0x02;
const HOURS: u8 = 0x04;
const DAY: u8 = 0x07;
const MONTH: u8 = 0x08;
const YEAR: u8 = 0x09;
const STATUS_A: u8 = 0x0A;
const STATUS_B: u8 = 0x0B;

/// Set once the RTC has been read successfully at least once.
static PRESENT: AtomicU64 = AtomicU64::new(0);
/// The last reading, as a Unix timestamp, and the tick it was taken on.
static LAST_READ: AtomicI64 = AtomicI64::new(0);
static LAST_TICK: AtomicU64 = AtomicU64::new(0);
/// Whole hours added to whatever the RTC says.
static OFFSET_HOURS: AtomicI64 = AtomicI64::new(0);

/// A moment, broken down the way a clock face wants it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

unsafe fn cmos(register: u8) -> u8 {
    let mut address = Port::<u8>::new(ADDRESS);
    let mut data = Port::<u8>::new(DATA);
    unsafe {
        // Preserve the top bit, which masks the NMI. Clearing it here would
        // re-enable non-maskable interrupts as a side effect of asking what
        // the time is.
        let previous = address.read() & 0x80;
        address.write(previous | (register & 0x7F));
        data.read()
    }
}

unsafe fn updating() -> bool {
    unsafe { cmos(STATUS_A) & 0x80 != 0 }
}

fn from_bcd(value: u8) -> u8 {
    (value & 0x0F) + ((value >> 4) * 10)
}

/// Read the RTC, retrying until two consecutive readings agree.
///
/// The chip updates its registers once a second and does not do it atomically,
/// so a single read can catch 10:59:59 halfway through becoming 11:00:00 and
/// report 10:00:00. Reading twice and comparing is the standard remedy.
fn read_raw() -> Option<DateTime> {
    interrupts::without_interrupts(|| unsafe {
        let mut previous: Option<[u8; 6]> = None;

        // Bounded: a chip that never settles is a chip that is not there, and
        // spinning on it forever would hang the boot.
        for _ in 0..1000 {
            let mut guard = 0;
            while updating() {
                guard += 1;
                if guard > 100_000 {
                    return None;
                }
            }

            let raw = [
                cmos(SECONDS),
                cmos(MINUTES),
                cmos(HOURS),
                cmos(DAY),
                cmos(MONTH),
                cmos(YEAR),
            ];

            if previous == Some(raw) {
                let format = cmos(STATUS_B);
                let binary = format & 0x04 != 0;
                let twelve_hour = format & 0x02 == 0;
                let decode = |value: u8| if binary { value } else { from_bcd(value) };

                // In twelve-hour mode the top bit of the hour register is the
                // PM flag, and has to come off before the digits mean anything.
                let hour_raw = raw[2];
                let pm = twelve_hour && hour_raw & 0x80 != 0;
                let mut hour = decode(hour_raw & 0x7F);
                if twelve_hour {
                    hour %= 12;
                    if pm {
                        hour += 12;
                    }
                }

                return Some(DateTime {
                    // The century register is not reliably present, so a
                    // two-digit year is read as this century. Kestrel did not
                    // exist in the nineties.
                    year: 2000 + decode(raw[5]) as u16,
                    month: decode(raw[4]),
                    day: decode(raw[3]),
                    hour,
                    minute: decode(raw[1]),
                    second: decode(raw[0]),
                });
            }
            previous = Some(raw);
        }
        None
    })
}

fn leap(year: u16) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in(year: u16, month: u8) -> u16 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap(year) => 29,
        2 => 28,
        // Nothing should ask, but a bad month from a broken chip must not
        // become an infinite loop in `from_unix`.
        _ => 30,
    }
}

/// Seconds since 1970, so that time can be compared and added to.
pub fn to_unix(time: &DateTime) -> i64 {
    let mut days: i64 = 0;
    for year in 1970..time.year {
        days += if leap(year) { 366 } else { 365 };
    }
    for month in 1..time.month.max(1) {
        days += days_in(time.year, month) as i64;
    }
    days += time.day.saturating_sub(1) as i64;

    days * 86_400 + time.hour as i64 * 3600 + time.minute as i64 * 60 + time.second as i64
}

pub fn from_unix(mut stamp: i64) -> DateTime {
    if stamp < 0 {
        stamp = 0;
    }

    let mut year = 1970u16;
    loop {
        let length = if leap(year) { 366 } else { 365 } * 86_400;
        if stamp < length {
            break;
        }
        stamp -= length;
        year += 1;
    }

    let mut month = 1u8;
    loop {
        let length = days_in(year, month) as i64 * 86_400;
        if stamp < length {
            break;
        }
        stamp -= length;
        month += 1;
    }

    DateTime {
        year,
        month,
        day: (stamp / 86_400) as u8 + 1,
        hour: ((stamp % 86_400) / 3600) as u8,
        minute: ((stamp % 3600) / 60) as u8,
        second: (stamp % 60) as u8,
    }
}

/// Read the clock once at boot, so a machine without one says so there rather
/// than at the first place something wanted a timestamp.
pub fn init() -> Option<DateTime> {
    let now = read_raw()?;
    PRESENT.store(1, Ordering::Relaxed);
    LAST_READ.store(to_unix(&now), Ordering::Relaxed);
    LAST_TICK.store(crate::apic::ticks(), Ordering::Relaxed);
    Some(now)
}

pub fn present() -> bool {
    PRESENT.load(Ordering::Relaxed) != 0
}

/// The current time, as a Unix timestamp.
///
/// Between readings the timer's own ticks carry it forward: the panel asks the
/// time on every frame it draws, and the RTC is a slow device behind two port
/// accesses per register. Re-reading it once a second keeps the answer honest
/// without making the clock expensive to look at.
pub fn unix_now() -> i64 {
    if !present() {
        return 0;
    }

    let offset = OFFSET_HOURS.load(Ordering::Relaxed) * 3600;
    let ticks = crate::apic::ticks();
    let last = LAST_TICK.load(Ordering::Relaxed);
    let elapsed = ticks.saturating_sub(last) / crate::apic::TIMER_FREQUENCY as u64;

    if elapsed >= 1 {
        if let Some(fresh) = read_raw() {
            let stamp = to_unix(&fresh);
            LAST_READ.store(stamp, Ordering::Relaxed);
            LAST_TICK.store(ticks, Ordering::Relaxed);
            return stamp + offset;
        }
    }

    LAST_READ.load(Ordering::Relaxed) + offset
}

pub fn now() -> Option<DateTime> {
    present().then(|| from_unix(unix_now()))
}

pub fn offset_hours() -> i64 {
    OFFSET_HOURS.load(Ordering::Relaxed)
}

pub fn set_offset_hours(hours: i64) {
    OFFSET_HOURS.store(hours.clamp(-12, 14), Ordering::Relaxed);
}

pub const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// The day of the week, by counting days from a date whose weekday is known:
/// 1 January 1970 was a Thursday. Zeller's congruence does it in one
/// expression, but not in one anybody can check by eye.
pub fn weekday(time: &DateTime) -> &'static str {
    const NAMES: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    let days = to_unix(time).div_euclid(86_400);
    NAMES[days.rem_euclid(7) as usize]
}
