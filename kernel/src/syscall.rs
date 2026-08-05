//! The SYSCALL/SYSRET system call interface.
//!
//! `syscall` is fast because it does almost nothing: it loads CS/SS from
//! fixed MSRs, stashes the return address in RCX and the flags in R11, and
//! jumps. Notably it does *not* switch stacks, so the entry stub has to do
//! that itself before touching anything.

use x86_64::registers::model_specific::{Efer, EferFlags, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;
use x86_64::VirtAddr;

use crate::gdt;

pub const SYS_WRITE: u64 = 1;
pub const SYS_EXIT: u64 = 2;
pub const SYS_GETPID: u64 = 3;
pub const SYS_YIELD: u64 = 4;

/// Point a core's syscall entry stub at the running task's ring-0 stack.
/// Called on every context switch, alongside the TSS update.
pub fn set_kernel_stack(cpu: usize, top: u64) {
    crate::cpu::set_syscall_stack(cpu, top);
}

/// Entry point for `syscall`.
///
/// Swaps to the kernel stack, preserves everything the System V ABI does not
/// let us clobber (plus RCX and R11, which `sysretq` needs), and calls into
/// Rust. Arguments arrive in RAX (number) and RDI/RSI/RDX.
#[unsafe(naked)]
unsafe extern "C" fn syscall_entry() {
    core::arch::naked_asm!(
        // Swap in this core's GS base. Everything below addresses per-CPU
        // state through it, so two cores in a system call at once each use
        // their own stack instead of fighting over one global.
        "swapgs",

        // Park the user stack and switch to the kernel one.
        "mov gs:[8], rsp",
        "mov rsp, gs:[0]",

        // Save everything the user might care about. `dispatch` is an ordinary
        // Rust function, so it will happily clobber every caller-saved
        // register — including the argument registers and r8-r10, which a
        // user program has no reason to expect are destroyed.
        //
        // The contract this establishes matches Linux: a system call
        // preserves all general-purpose registers except RAX (the result),
        // and RCX/R11, which the `syscall` instruction itself overwrites with
        // the return address and flags.
        "push rcx",
        "push r11",
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "push rdi",
        "push rsi",
        "push rdx",
        "push r8",
        "push r9",
        "push r10",

        // Safe to take interrupts again now that we are off the user stack.
        // Leaving them masked for the whole call would drop keystrokes during
        // anything slow, such as drawing to the framebuffer. An interrupt
        // arriving here does not switch stacks — we are already in ring 0 —
        // so it simply nests on this stack.
        "sti",

        // dispatch(number = rax, a = rdi, b = rsi, c = rdx)
        "mov rcx, rdx",
        "mov rdx, rsi",
        "mov rsi, rdi",
        "mov rdi, rax",
        "call {dispatch}",

        // RAX holds the result and is deliberately not restored.
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "pop r11",
        "pop rcx",

        // Mask again before adopting the user stack pointer: an interrupt in
        // that window would push a ring-0 frame onto user-controlled memory.
        // `sysretq` restores the user's own IF from R11.
        "cli",
        "mov rsp, gs:[8]",
        "swapgs",
        "sysretq",

        dispatch = sym dispatch,
    )
}

/// The Rust side of a system call. Returns the value the user sees in RAX.
extern "C" fn dispatch(number: u64, a: u64, b: u64, c: u64) -> u64 {
    match number {
        SYS_WRITE => sys_write(a, b, c),
        // Terminates the calling task rather than returning to ring 3. The
        // task's stacks and address space stay allocated until something
        // reaps it; nothing does yet.
        SYS_EXIT => crate::task::finish(a),
        SYS_GETPID => x86_64::instructions::interrupts::without_interrupts(|| {
            let cpu = crate::cpu::index();
            crate::task::SCHEDULER.lock().current_id(cpu)
        }),
        SYS_YIELD => {
            crate::task::yield_now();
            0
        }
        _ => u64::MAX, // unknown call
    }
}

/// write(fd, buffer, length). Only stdout and stderr exist.
fn sys_write(fd: u64, buffer: u64, length: u64) -> u64 {
    if fd != 1 && fd != 2 {
        return u64::MAX;
    }

    // A user pointer must never be trusted. This only checks the range is
    // plausibly user memory; a real kernel would also walk the page tables to
    // confirm every page is mapped and user-accessible.
    if buffer == 0 || length > 4096 || !is_user_address(buffer) || !is_user_address(buffer + length)
    {
        return u64::MAX;
    }

    let bytes = unsafe { core::slice::from_raw_parts(buffer as *const u8, length as usize) };

    // One print for the whole buffer, not one per byte: this takes the console
    // lock a single time, so a write from one process cannot be interleaved
    // mid-string with another's.
    match core::str::from_utf8(bytes) {
        Ok(text) => crate::print!("{text}"),
        Err(_) => return u64::MAX,
    }

    length
}

/// User space is the lower half of the address space.
fn is_user_address(address: u64) -> bool {
    address < 0x0000_8000_0000_0000
}

/// Enable `syscall` and point it at our entry stub.
pub fn init() {
    let selectors = gdt::selectors();

    unsafe {
        // SYSCALL/SYSRET derive all four selectors from these two, assuming a
        // fixed GDT order: kernel code, kernel data, then user data, user
        // code. `Star::write` enforces exactly that.
        Star::write(
            selectors.user_code,
            selectors.user_data,
            selectors.kernel_code,
            selectors.kernel_data,
        )
        .expect("GDT selectors are not laid out as SYSRET requires");

        LStar::write(VirtAddr::new(syscall_entry as unsafe extern "C" fn() as u64));

        // Clear IF on entry: the stub runs briefly on the user's stack
        // pointer, and must not be interrupted before it swaps stacks.
        SFMask::write(RFlags::INTERRUPT_FLAG | RFlags::DIRECTION_FLAG);

        Efer::update(|flags| flags.insert(EferFlags::SYSTEM_CALL_EXTENSIONS));
    }
}
