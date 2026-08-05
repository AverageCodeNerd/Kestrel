//! A user program that deliberately misbehaves, to show that the kernel
//! survives it.
//!
//! It reaches for a kernel address. The CPU refuses — user pages and kernel
//! pages are distinguished by a bit in the page tables — and the kernel kills
//! this process alone, leaving the shell running.

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
    write("about to read kernel memory from ring 3...\n");

    // The kernel is mapped in this address space — it has to be, for system
    // calls to work — but without the user-accessible bit.
    let value = unsafe { core::ptr::read_volatile(0xffff_ffff_8000_0000u64 as *const u8) };

    // Only reached if the isolation is broken.
    write("READ SUCCEEDED - the kernel is not protected!\n");
    unsafe { syscall(SYS_EXIT, value as u64, 0, 0) };
    unreachable!()
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { syscall(SYS_EXIT, 1, 0, 0) };
    unreachable!()
}
