//! PS/2 keyboard, scancode set 1.
//!
//! The interrupt handler does nothing but decode a byte and push it into a
//! ring buffer. It must never print: the console lock may already be held by
//! the code that was interrupted, and blocking on it inside an interrupt
//! handler would deadlock the machine.

use spin::Mutex;
use x86_64::instructions::port::Port;

const DATA_PORT: u16 = 0x60;
const STATUS_PORT: u16 = 0x64;
/// Status bit 0: a byte is waiting in the controller's output buffer.
const OUTPUT_FULL: u8 = 1;

/// Status bit 1: the controller has not yet consumed the last byte we wrote.
const INPUT_FULL: u8 = 1 << 1;

/// Controller commands.
const DISABLE_PORT1: u8 = 0xAD;
const DISABLE_PORT2: u8 = 0xA7;
const ENABLE_PORT1: u8 = 0xAE;
const READ_CONFIG: u8 = 0x20;
const WRITE_CONFIG: u8 = 0x60;

/// Config byte bits.
const CONFIG_PORT1_INTERRUPT: u8 = 1 << 0;
const CONFIG_PORT1_CLOCK_OFF: u8 = 1 << 4;
const CONFIG_TRANSLATION: u8 = 1 << 6;

/// Keyboard command and its acknowledgement.
const ENABLE_SCANNING: u8 = 0xF4;
const ACK: u8 = 0xFA;

fn wait_writable() -> bool {
    let mut status = Port::<u8>::new(STATUS_PORT);
    for _ in 0..100_000 {
        // 0xFF means nothing is answering at all — there is no controller.
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

fn drain() {
    let mut status = Port::<u8>::new(STATUS_PORT);
    let mut data = Port::<u8>::new(DATA_PORT);

    // Bounded: a controller that always reports full is broken, and spinning
    // here would hang the boot.
    for _ in 0..64 {
        unsafe {
            let value = status.read();
            if value == 0xFF || value & OUTPUT_FULL == 0 {
                break;
            }
            let _ = data.read();
        }
    }
}

/// Bring up the PS/2 controller, returning whether a keyboard answered.
///
/// The previous version only drained the output buffer and trusted the
/// firmware to have left scanning enabled and IRQ1 unmasked. That happens to
/// be true under OVMF, and is not guaranteed anywhere else, so the controller
/// is now configured explicitly.
///
/// Some machines have no 8042 at all — Hyper-V Generation 2 among them, where
/// input arrives over VMBus instead — so this reports failure rather than
/// hanging or pretending to have succeeded.
pub fn init() -> bool {
    // Quiet both ports while reconfiguring, then discard whatever the
    // firmware left behind. An unread byte blocks all further interrupts:
    // no interrupt, so no read, so no interrupt.
    command(DISABLE_PORT1);
    command(DISABLE_PORT2);
    drain();

    if !command(READ_CONFIG) {
        return false;
    }
    let Some(config) = read_data() else {
        return false;
    };

    // Unmask the keyboard interrupt, make sure its clock is running, and keep
    // translation on so scancodes arrive in set 1, which is what this driver
    // decodes.
    let updated = (config | CONFIG_PORT1_INTERRUPT | CONFIG_TRANSLATION) & !CONFIG_PORT1_CLOCK_OFF;

    if !command(WRITE_CONFIG) || !write_data(updated) {
        return false;
    }

    if !command(ENABLE_PORT1) {
        return false;
    }

    // Ask the keyboard itself to start sending. Its acknowledgement is the
    // only real evidence that a keyboard is present.
    if !write_data(ENABLE_SCANNING) {
        return false;
    }

    match read_data() {
        Some(ACK) => true,
        // Some controllers answer late or resend; treat any reply as presence.
        Some(_) => true,
        None => false,
    }
}

/// Set 1 make codes, unshifted. Index is the scancode; 0 means "no character".
#[rustfmt::skip]
const UNSHIFTED: [u8; 0x40] = [
    0, 0, b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8', b'9', b'0', b'-', b'=', 0x08, b'\t',
    b'q', b'w', b'e', b'r', b't', b'y', b'u', b'i', b'o', b'p', b'[', b']', b'\n', 0, b'a', b's',
    b'd', b'f', b'g', b'h', b'j', b'k', b'l', b';', b'\'', b'`', 0, b'\\', b'z', b'x', b'c', b'v',
    b'b', b'n', b'm', b',', b'.', b'/', 0, b'*', 0, b' ', 0, 0, 0, 0, 0, 0,
];

#[rustfmt::skip]
const SHIFTED: [u8; 0x40] = [
    0, 0, b'!', b'@', b'#', b'$', b'%', b'^', b'&', b'*', b'(', b')', b'_', b'+', 0x08, b'\t',
    b'Q', b'W', b'E', b'R', b'T', b'Y', b'U', b'I', b'O', b'P', b'{', b'}', b'\n', 0, b'A', b'S',
    b'D', b'F', b'G', b'H', b'J', b'K', b'L', b':', b'"', b'~', 0, b'|', b'Z', b'X', b'C', b'V',
    b'B', b'N', b'M', b'<', b'>', b'?', 0, b'*', 0, b' ', 0, 0, 0, 0, 0, 0,
];

/// Keys with no character of their own, reported above the ASCII range so
/// they still fit the byte-oriented input buffer.
pub const KEY_UP: u8 = 0x80;
pub const KEY_DOWN: u8 = 0x81;
pub const KEY_LEFT: u8 = 0x82;
pub const KEY_RIGHT: u8 = 0x83;
pub const KEY_HOME: u8 = 0x84;
pub const KEY_END: u8 = 0x85;
pub const KEY_DELETE: u8 = 0x86;
/// F1, which the desktop uses to open its launcher.
pub const KEY_F1: u8 = 0x87;
/// Escape, which the tables below cannot carry because it has no character.
pub const KEY_ESCAPE: u8 = 0x1B;

const SCANCODE_LEFT_SHIFT: u8 = 0x2A;
const SCANCODE_RIGHT_SHIFT: u8 = 0x36;
const SCANCODE_CAPS_LOCK: u8 = 0x3A;
/// Prefix byte introducing an extended (two-byte) scancode.
const EXTENDED_PREFIX: u8 = 0xE0;

struct State {
    shift: bool,
    caps: bool,
    /// Set when the previous byte was 0xE0, so the next one is an extended key.
    extended: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    shift: false,
    caps: false,
    extended: false,
});

const BUFFER_CAPACITY: usize = 128;

struct Buffer {
    data: [u8; BUFFER_CAPACITY],
    read: usize,
    write: usize,
}

impl Buffer {
    fn push(&mut self, byte: u8) {
        let next = (self.write + 1) % BUFFER_CAPACITY;
        if next == self.read {
            return; // full; drop the keypress rather than overwrite history
        }
        self.data[self.write] = byte;
        self.write = next;
    }

    fn is_empty(&self) -> bool {
        self.read == self.write
    }

    fn pop(&mut self) -> Option<u8> {
        if self.read == self.write {
            return None;
        }
        let byte = self.data[self.read];
        self.read = (self.read + 1) % BUFFER_CAPACITY;
        Some(byte)
    }
}

static BUFFER: Mutex<Buffer> = Mutex::new(Buffer {
    data: [0; BUFFER_CAPACITY],
    read: 0,
    write: 0,
});

/// Called from the keyboard interrupt handler. Reads exactly one byte from the
/// controller — leaving it unread would block all further keyboard interrupts.
pub fn handle_interrupt() {
    let scancode: u8 = unsafe { Port::new(DATA_PORT).read() };
    let mut state = STATE.lock();

    if scancode == EXTENDED_PREFIX {
        state.extended = true;
        return;
    }

    let extended = core::mem::take(&mut state.extended);
    let released = scancode & 0x80 != 0;
    let code = scancode & 0x7F;

    // Arrows and the navigation cluster arrive as two-byte sequences.
    if extended {
        if released {
            return;
        }

        let key = match code {
            0x48 => KEY_UP,
            0x50 => KEY_DOWN,
            0x4B => KEY_LEFT,
            0x4D => KEY_RIGHT,
            0x47 => KEY_HOME,
            0x4F => KEY_END,
            0x53 => KEY_DELETE,
            // Right ctrl/alt and the rest have no meaning here.
            _ => return,
        };

        BUFFER.lock().push(key);
        return;
    }

    // Escape carries no character, so the tables cannot express it.
    if code == 0x01 {
        if !released {
            BUFFER.lock().push(KEY_ESCAPE);
        }
        return;
    }

    // F1 likewise: it names no character, and the desktop reads it as "open
    // the launcher" so the app menu is reachable without the mouse.
    if code == 0x3B {
        if !released {
            BUFFER.lock().push(KEY_F1);
        }
        return;
    }

    match code {
        SCANCODE_LEFT_SHIFT | SCANCODE_RIGHT_SHIFT => {
            state.shift = !released;
            return;
        }
        SCANCODE_CAPS_LOCK => {
            if !released {
                state.caps = !state.caps;
            }
            return;
        }
        _ => {}
    }

    if released || code as usize >= UNSHIFTED.len() {
        return;
    }

    let table = if state.shift { &SHIFTED } else { &UNSHIFTED };
    let base = table[code as usize];
    if base == 0 {
        return;
    }

    // Caps lock affects letters only, and inverts whatever shift decided.
    let character = if state.caps && base.is_ascii_alphabetic() {
        if state.shift {
            base.to_ascii_lowercase()
        } else {
            base.to_ascii_uppercase()
        }
    } else {
        base
    };

    BUFFER.lock().push(character);
}

/// Take the next typed character, if any.
///
/// Interrupts are masked across the critical section: if a keyboard interrupt
/// landed while this code held the buffer lock, the handler would spin on that
/// lock forever and wedge the machine.
pub fn read() -> Option<u8> {
    x86_64::instructions::interrupts::without_interrupts(|| BUFFER.lock().pop())
}

/// Whether `read` would return something, without taking it.
///
/// Needed by anything that must keep moving while it waits — a game cannot
/// block on a keystroke between frames.
pub fn ready() -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| !BUFFER.lock().is_empty())
}
