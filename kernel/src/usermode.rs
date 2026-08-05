//! Dropping to ring 3.

use x86_64::VirtAddr;

use crate::gdt;

/// Enter ring 3 at `entry` with `stack`, and never come back.
///
/// `iretq` is the general way to change privilege level: it pops RIP, CS,
/// RFLAGS, RSP and SS in that order, so the frame is pushed in reverse. The
/// only way out of the resulting user program is a system call.
///
/// # Safety
/// Both addresses must be mapped and user-accessible in the address space that
/// is currently active.
pub unsafe fn enter(entry: VirtAddr, stack: VirtAddr) -> ! {
    let selectors = gdt::selectors();

    unsafe {
        core::arch::asm!(
            "push {ss}",
            "push {rsp}",
            "push 0x202",       // RFLAGS: interrupts on, bit 1 always set
            "push {cs}",
            "push {rip}",
            "iretq",
            ss = in(reg) selectors.user_data.0 as u64,
            rsp = in(reg) stack.as_u64(),
            cs = in(reg) selectors.user_code.0 as u64,
            rip = in(reg) entry.as_u64(),
            options(noreturn),
        )
    }
}

/// Set up the syscall interface. Must run after the GDT is loaded.
pub fn init() {
    crate::syscall::init();
}
