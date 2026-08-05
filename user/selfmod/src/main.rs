//! A user program that tries to rewrite its own code.
//!
//! Its text segment is mapped read-execute, so the write faults and the kernel
//! kills the process. Before segment permissions were enforced this silently
//! succeeded.

#![no_std]
#![no_main]

use core::arch::asm;

const SYS_WRITE: u64 = 1;
const SYS_EXIT: u64 = 2;

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

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    write("about to overwrite my own code...\n");

    // `_start` lives in the text segment, which the loader mapped without
    // write permission.
    let code = _start as *const () as *mut u8;
    unsafe { core::ptr::write_volatile(code, 0x90) };

    // Only reached if the text segment was writable after all.
    write("WRITE SUCCEEDED - code is writable!\n");
    unsafe { syscall(SYS_EXIT, 0, 0, 0) };
    unreachable!()
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { syscall(SYS_EXIT, 1, 0, 0) };
    unreachable!()
}
