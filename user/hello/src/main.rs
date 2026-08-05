//! A Kestrel user program. Runs in ring 3, in its own address space, and can
//! only reach the kernel through `syscall`.

#![no_std]
#![no_main]

use core::arch::asm;

const SYS_WRITE: u64 = 1;
const SYS_EXIT: u64 = 2;
const SYS_GETPID: u64 = 3;
const SYS_YIELD: u64 = 4;

/// The kernel's calling convention: number in RAX, arguments in RDI/RSI/RDX,
/// result in RAX. `syscall` itself clobbers RCX and R11.
unsafe fn syscall(number: u64, a: u64, b: u64, c: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number => result,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    result
}

fn write(text: &str) {
    unsafe { syscall(SYS_WRITE, 1, text.as_ptr() as u64, text.len() as u64) };
}

fn getpid() -> u64 {
    unsafe { syscall(SYS_GETPID, 0, 0, 0) }
}

fn yield_now() {
    unsafe { syscall(SYS_YIELD, 0, 0, 0) };
}

fn exit(code: u64) -> ! {
    unsafe { syscall(SYS_EXIT, code, 0, 0) };
    // The kernel never returns from exit.
    unreachable!()
}

/// Print a small number without any formatting machinery.
fn write_number(mut value: u64) {
    let mut digits = [0u8; 20];
    let mut count = 0;

    if value == 0 {
        write("0");
        return;
    }

    while value > 0 {
        digits[count] = b'0' + (value % 10) as u8;
        value /= 10;
        count += 1;
    }

    let mut buffer = [0u8; 20];
    for i in 0..count {
        buffer[i] = digits[count - 1 - i];
    }

    write(unsafe { core::str::from_utf8_unchecked(&buffer[..count]) });
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    write("hello from a real ELF binary in ring 3\n");

    write("  my pid is ");
    write_number(getpid());
    write("\n");

    // Yield between lines so that two copies running at once visibly
    // interleave rather than each finishing in one go.
    for step in 1..=3 {
        write("  ");
        write_number(getpid());
        write(": step ");
        write_number(step);
        write("\n");
        yield_now();
    }

    write("  done\n");
    exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(1)
}
