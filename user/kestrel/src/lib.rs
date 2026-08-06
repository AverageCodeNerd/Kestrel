//! The standard library for Kestrel programs.
//!
//! Everything a ring-3 program can do goes through `syscall`, and every program
//! was writing the same inline assembly and the same integer formatting by
//! hand. This is that, once — the thing an application is written against
//! rather than against the raw instruction.
//!
//! There is no allocator here. Programs are small and static, and giving them
//! a heap would mean the kernel growing a way to hand out pages first.

#![no_std]

use core::arch::asm;

pub const SYS_WRITE: u64 = 1;
pub const SYS_EXIT: u64 = 2;
pub const SYS_GETPID: u64 = 3;
pub const SYS_YIELD: u64 = 4;
pub const SYS_READ: u64 = 5;
pub const SYS_LOAD: u64 = 6;
pub const SYS_STORE: u64 = 7;
pub const SYS_SLEEP: u64 = 8;
pub const SYS_POLL: u64 = 9;
pub const SYS_CLEAR: u64 = 10;
pub const SYS_SURFACE: u64 = 11;
pub const SYS_BLIT: u64 = 12;
pub const SYS_POINTER: u64 = 13;

/// Where the mouse is inside a program's window, and what is held down.
pub struct Pointer {
    pub x: usize,
    pub y: usize,
    pub left: bool,
    pub right: bool,
    pub middle: bool,
}

/// Every call returns this on failure. It is `-1` read as unsigned, which is
/// how the kernel reports an error without a second return value.
pub const FAILED: u64 = u64::MAX;

/// The kernel's calling convention: number in RAX, arguments in RDI/RSI/RDX
/// and R10, result in RAX. `syscall` itself clobbers RCX and R11.
///
/// The fourth argument is in R10 rather than RCX because the instruction has
/// already put the return address there — the same reason Linux does it.
///
/// # Safety
/// The kernel validates what it can, but a pointer that does not refer to
/// memory this program owns is still the caller's mistake to avoid.
pub unsafe fn syscall(number: u64, a: u64, b: u64, c: u64, d: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number => result,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            in("r10") d,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    result
}

// ---------------------------------------------------------------- output ---

pub fn write(text: &str) {
    unsafe { syscall(SYS_WRITE, 1, text.as_ptr() as u64, text.len() as u64, 0) };
}

/// Write a number in base ten, without any formatting machinery.
pub fn write_number(value: u64) {
    let mut digits = [0u8; 20];
    let mut count = 0;
    let mut rest = value;

    if rest == 0 {
        write("0");
        return;
    }

    while rest > 0 {
        digits[count] = b'0' + (rest % 10) as u8;
        rest /= 10;
        count += 1;
    }

    // Written out backwards, so reverse into a second buffer before printing.
    let mut buffer = [0u8; 20];
    for index in 0..count {
        buffer[index] = digits[count - 1 - index];
    }
    write(unsafe { core::str::from_utf8_unchecked(&buffer[..count]) });
}

pub fn write_line(text: &str) {
    write(text);
    write("\n");
}

// ----------------------------------------------------------------- input ---

/// Read whatever input is waiting, blocking until there is some.
///
/// Returns the number of bytes placed in `buffer`, or `FAILED`.
pub fn read(buffer: &mut [u8]) -> u64 {
    unsafe {
        syscall(
            SYS_READ,
            0,
            buffer.as_mut_ptr() as u64,
            buffer.len() as u64,
            0,
        )
    }
}

/// Read one byte, blocking.
pub fn read_byte() -> Option<u8> {
    let mut one = [0u8; 1];
    match read(&mut one) {
        1 => Some(one[0]),
        _ => None,
    }
}

/// Read until Enter, with backspace handled, echoing as it goes.
///
/// Returns how many bytes of `buffer` hold the line. Input past the end of the
/// buffer is dropped rather than wrapping, which would silently corrupt what
/// the user thought they typed.
pub fn read_line(buffer: &mut [u8]) -> usize {
    let mut length = 0;

    loop {
        let Some(byte) = read_byte() else { continue };

        match byte {
            b'\n' | b'\r' => {
                write("\n");
                return length;
            }
            0x08 | 0x7F => {
                if length > 0 {
                    length -= 1;
                    // Rub the character out: back up, overwrite, back up again.
                    write("\u{8} \u{8}");
                }
            }
            byte if byte.is_ascii_graphic() || byte == b' ' => {
                if length < buffer.len() {
                    buffer[length] = byte;
                    length += 1;
                    let echo = [byte];
                    write(unsafe { core::str::from_utf8_unchecked(&echo) });
                }
            }
            _ => {}
        }
    }
}

// ------------------------------------------------------------------ files ---

/// Read a whole file. Returns how much of `buffer` was filled, or `FAILED`.
pub fn load(path: &str, buffer: &mut [u8]) -> u64 {
    unsafe {
        syscall(
            SYS_LOAD,
            path.as_ptr() as u64,
            path.len() as u64,
            buffer.as_mut_ptr() as u64,
            buffer.len() as u64,
        )
    }
}

/// Replace a whole file. Returns the number of bytes written, or `FAILED`.
pub fn store(path: &str, data: &[u8]) -> u64 {
    unsafe {
        syscall(
            SYS_STORE,
            path.as_ptr() as u64,
            path.len() as u64,
            data.as_ptr() as u64,
            data.len() as u64,
        )
    }
}

// ------------------------------------------------------------------ misc ---

/// Is a keystroke waiting? Lets a program keep moving instead of blocking.
pub fn key_ready() -> bool {
    unsafe { syscall(SYS_POLL, 0, 0, 0, 0) == 1 }
}

/// Take a keystroke only if one is waiting.
pub fn read_key() -> Option<u8> {
    if key_ready() { read_byte() } else { None }
}

/// Wipe the screen and start again at the top.
pub fn clear() {
    unsafe { syscall(SYS_CLEAR, 0, 0, 0, 0) };
}

// --------------------------------------------------------------- drawing ---

/// Ask for a window of this size to draw in. False if there is no desktop.
///
/// The kernel keeps the pixels; this program never sees the framebuffer, only
/// its own rectangle.
pub fn surface(width: usize, height: usize, title: &str) -> bool {
    unsafe {
        syscall(
            SYS_SURFACE,
            width as u64,
            height as u64,
            title.as_ptr() as u64,
            title.len() as u64,
        ) == 0
    }
}

/// Show a frame. Must be exactly the size asked for, row by row.
pub fn blit(pixels: &[u32]) -> bool {
    unsafe { syscall(SYS_BLIT, pixels.as_ptr() as u64, pixels.len() as u64, 0, 0) == 0 }
}

/// Where the pointer is inside the window, or `None` when it is elsewhere.
pub fn pointer() -> Option<Pointer> {
    let packed = unsafe { syscall(SYS_POINTER, 0, 0, 0, 0) };
    if packed == FAILED {
        return None;
    }

    let buttons = packed >> 32;
    Some(Pointer {
        x: (packed & 0xFFFF) as usize,
        y: ((packed >> 16) & 0xFFFF) as usize,
        left: buttons & 1 != 0,
        right: buttons & 2 != 0,
        middle: buttons & 4 != 0,
    })
}

pub fn getpid() -> u64 {
    unsafe { syscall(SYS_GETPID, 0, 0, 0, 0) }
}

pub fn yield_now() {
    unsafe { syscall(SYS_YIELD, 0, 0, 0, 0) };
}

/// Wait, rounded up to the kernel's 10 ms tick.
pub fn sleep(milliseconds: u64) {
    unsafe { syscall(SYS_SLEEP, milliseconds, 0, 0, 0) };
}

pub fn exit(code: u64) -> ! {
    unsafe { syscall(SYS_EXIT, code, 0, 0, 0) };
    // The kernel does not return from this.
    unreachable!()
}

/// The panic handler every program needs, so none of them has to write one.
///
/// Declared here as a macro rather than in this crate directly: a `no_std`
/// binary must define its own, and two definitions in one program is an error.
#[macro_export]
macro_rules! main {
    ($body:expr) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn _start() -> ! {
            let body: fn() = $body;
            body();
            $crate::exit(0)
        }

        #[panic_handler]
        fn panic(_info: &core::panic::PanicInfo) -> ! {
            $crate::write_line("panic");
            $crate::exit(1)
        }
    };
}
