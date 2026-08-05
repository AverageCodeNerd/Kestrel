//! PS/2 mouse.
//!
//! The mouse hangs off the same 8042 controller as the keyboard, on its second
//! port. Commands for it have to be prefixed with 0xD4, or the controller
//! answers them itself instead of passing them along.
//!
//! Each movement produces a three-byte packet. The packets are not framed, so
//! the only way to stay in step is bit 3 of the first byte, which is always
//! set — a stream that loses sync is resynchronised on that.

use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use x86_64::instructions::port::Port;

const DATA_PORT: u16 = 0x60;
const STATUS_PORT: u16 = 0x64;

const OUTPUT_FULL: u8 = 1 << 0;
const INPUT_FULL: u8 = 1 << 1;

/// Controller commands.
const ENABLE_PORT2: u8 = 0xA8;
const READ_CONFIG: u8 = 0x20;
const WRITE_CONFIG: u8 = 0x60;
/// Send the next byte to the mouse rather than the controller.
const ADDRESS_PORT2: u8 = 0xD4;

/// Config byte bits.
const CONFIG_PORT2_INTERRUPT: u8 = 1 << 1;
const CONFIG_PORT2_CLOCK_OFF: u8 = 1 << 5;

/// Mouse commands.
const SET_DEFAULTS: u8 = 0xF6;
const ENABLE_REPORTING: u8 = 0xF4;
const ACK: u8 = 0xFA;

/// Packet byte 0.
const FLAG_LEFT: u8 = 1 << 0;
const FLAG_RIGHT: u8 = 1 << 1;
const FLAG_MIDDLE: u8 = 1 << 2;
/// Always set, so it is how a desynchronised stream is recognised.
const FLAG_ALWAYS_SET: u8 = 1 << 3;
const FLAG_X_SIGN: u8 = 1 << 4;
const FLAG_Y_SIGN: u8 = 1 << 5;
const FLAG_X_OVERFLOW: u8 = 1 << 6;
const FLAG_Y_OVERFLOW: u8 = 1 << 7;

/// Cursor position, in pixels. Clamped to the screen by `set_bounds`.
static X: AtomicI32 = AtomicI32::new(0);
static Y: AtomicI32 = AtomicI32::new(0);
static MAX_X: AtomicI32 = AtomicI32::new(0);
static MAX_Y: AtomicI32 = AtomicI32::new(0);

static LEFT: AtomicBool = AtomicBool::new(false);
static RIGHT: AtomicBool = AtomicBool::new(false);
static MIDDLE: AtomicBool = AtomicBool::new(false);
static MOVED: AtomicBool = AtomicBool::new(false);

/// Partially assembled packet.
static PACKET: spin::Mutex<[u8; 3]> = spin::Mutex::new([0; 3]);
static PACKET_INDEX: AtomicI32 = AtomicI32::new(0);

fn wait_writable() -> bool {
    let mut status = Port::<u8>::new(STATUS_PORT);
    for _ in 0..100_000 {
        let value = unsafe { status.read() };
        if value == 0xFF {
            return false;
        }
        if value & INPUT_FULL == 0 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

fn wait_readable() -> bool {
    let mut status = Port::<u8>::new(STATUS_PORT);
    for _ in 0..100_000 {
        let value = unsafe { status.read() };
        if value == 0xFF {
            return false;
        }
        if value & OUTPUT_FULL != 0 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

fn command(byte: u8) -> bool {
    if !wait_writable() {
        return false;
    }
    unsafe { Port::<u8>::new(STATUS_PORT).write(byte) };
    true
}

fn write_data(byte: u8) -> bool {
    if !wait_writable() {
        return false;
    }
    unsafe { Port::<u8>::new(DATA_PORT).write(byte) };
    true
}

fn read_data() -> Option<u8> {
    if !wait_readable() {
        return None;
    }
    Some(unsafe { Port::<u8>::new(DATA_PORT).read() })
}

/// Send a command to the mouse itself and wait for its acknowledgement.
fn mouse_command(byte: u8) -> bool {
    if !command(ADDRESS_PORT2) || !write_data(byte) {
        return false;
    }
    matches!(read_data(), Some(ACK))
}

/// Where the cursor may go. Called once the framebuffer size is known.
pub fn set_bounds(width: usize, height: usize) {
    MAX_X.store(width as i32 - 1, Ordering::Relaxed);
    MAX_Y.store(height as i32 - 1, Ordering::Relaxed);
    X.store(width as i32 / 2, Ordering::Relaxed);
    Y.store(height as i32 / 2, Ordering::Relaxed);
}

pub fn position() -> (i32, i32) {
    (X.load(Ordering::Relaxed), Y.load(Ordering::Relaxed))
}

pub fn buttons() -> (bool, bool, bool) {
    (
        LEFT.load(Ordering::Relaxed),
        RIGHT.load(Ordering::Relaxed),
        MIDDLE.load(Ordering::Relaxed),
    )
}

/// Has anything changed since the last call? Clears the flag.
pub fn take_movement() -> bool {
    MOVED.swap(false, Ordering::Relaxed)
}

/// Called from the IRQ12 handler. Reads exactly one byte.
pub fn handle_interrupt() {
    let byte = unsafe { Port::<u8>::new(DATA_PORT).read() };

    let index = PACKET_INDEX.load(Ordering::Relaxed);

    // The first byte of a packet always has bit 3 set. If it does not, the
    // stream is out of step and this byte is skipped rather than shifting the
    // whole packet.
    if index == 0 && byte & FLAG_ALWAYS_SET == 0 {
        return;
    }

    {
        let mut packet = PACKET.lock();
        packet[index as usize] = byte;
    }

    if index < 2 {
        PACKET_INDEX.store(index + 1, Ordering::Relaxed);
        return;
    }
    PACKET_INDEX.store(0, Ordering::Relaxed);

    let packet = *PACKET.lock();
    let flags = packet[0];

    // An overflowed axis carries no usable magnitude; drop the packet rather
    // than lurch the cursor across the screen.
    if flags & (FLAG_X_OVERFLOW | FLAG_Y_OVERFLOW) != 0 {
        return;
    }

    LEFT.store(flags & FLAG_LEFT != 0, Ordering::Relaxed);
    RIGHT.store(flags & FLAG_RIGHT != 0, Ordering::Relaxed);
    MIDDLE.store(flags & FLAG_MIDDLE != 0, Ordering::Relaxed);

    // The deltas are 9-bit two's complement: 8 bits of magnitude plus a sign
    // bit living in the flags byte.
    let mut dx = packet[1] as i32;
    let mut dy = packet[2] as i32;
    if flags & FLAG_X_SIGN != 0 {
        dx -= 256;
    }
    if flags & FLAG_Y_SIGN != 0 {
        dy -= 256;
    }

    let max_x = MAX_X.load(Ordering::Relaxed);
    let max_y = MAX_Y.load(Ordering::Relaxed);

    let x = (X.load(Ordering::Relaxed) + dx).clamp(0, max_x);
    // The mouse reports Y increasing upwards; the screen counts downwards.
    let y = (Y.load(Ordering::Relaxed) - dy).clamp(0, max_y);

    X.store(x, Ordering::Relaxed);
    Y.store(y, Ordering::Relaxed);
    MOVED.store(true, Ordering::Relaxed);
}

/// Bring the mouse up, returning whether one answered.
pub fn init() -> bool {
    if !command(ENABLE_PORT2) {
        return false;
    }

    // Unmask the mouse interrupt and make sure its clock is running.
    if !command(READ_CONFIG) {
        return false;
    }
    let Some(config) = read_data() else {
        return false;
    };
    let updated = (config | CONFIG_PORT2_INTERRUPT) & !CONFIG_PORT2_CLOCK_OFF;
    if !command(WRITE_CONFIG) || !write_data(updated) {
        return false;
    }

    // Defaults first, then start reporting. A mouse that acknowledges both is
    // definitely present.
    if !mouse_command(SET_DEFAULTS) {
        return false;
    }
    mouse_command(ENABLE_REPORTING)
}
