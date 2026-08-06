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
/// read(fd, buffer, length) — from the keyboard or the serial console.
pub const SYS_READ: u64 = 5;
/// load(path, length, buffer, capacity) — a whole file at once.
pub const SYS_LOAD: u64 = 6;
/// store(path, length, buffer, count) — replace a whole file.
pub const SYS_STORE: u64 = 7;
/// sleep(milliseconds).
pub const SYS_SLEEP: u64 = 8;

/// The longest path a program may pass in. Enough for anything the FAT32
/// driver can represent, and small enough to copy onto the kernel stack.
const MAX_PATH: u64 = 255;
/// The largest single transfer. Bigger than a screen of text and small enough
/// that a program cannot ask the kernel to allocate without bound.
const MAX_TRANSFER: u64 = 64 * 1024;

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

        // dispatch(number = rax, a = rdi, b = rsi, c = rdx, d = r10)
        //
        // The fourth argument arrives in R10 rather than RCX, because the
        // `syscall` instruction has already overwritten RCX with the return
        // address. Linux does the same thing for the same reason. R8 is the
        // fifth System V parameter register and was saved above, so it is free
        // to stage the value in.
        "mov r8, r10",
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
extern "C" fn dispatch(number: u64, a: u64, b: u64, c: u64, d: u64) -> u64 {
    match number {
        SYS_WRITE => sys_write(a, b, c),
        SYS_READ => sys_read(a, b, c),
        SYS_LOAD => sys_load(a, b, c, d),
        SYS_STORE => sys_store(a, b, c, d),
        SYS_SLEEP => sys_sleep(a),
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

/// A user buffer, checked before anything is allowed to touch it.
///
/// The range test is the same one `sys_write` uses and has the same limits: it
/// confirms the address looks like user memory, not that every page in it is
/// mapped and writable. A real kernel would walk the page tables. What saves
/// us here is that a fault while writing hits the kernel's own handler and
/// kills the offending task rather than the system.
fn user_slice(buffer: u64, length: u64) -> Option<()> {
    if buffer == 0
        || length > MAX_TRANSFER
        || !is_user_address(buffer)
        || !is_user_address(buffer.checked_add(length)?)
    {
        return None;
    }
    Some(())
}

/// Copy a path out of user memory.
fn user_path(pointer: u64, length: u64) -> Option<alloc::string::String> {
    if length == 0 || length > MAX_PATH {
        return None;
    }
    user_slice(pointer, length)?;

    let bytes = unsafe { core::slice::from_raw_parts(pointer as *const u8, length as usize) };
    core::str::from_utf8(bytes).ok().map(alloc::string::String::from)
}

/// read(fd, buffer, length). Only stdin exists, and it blocks.
///
/// Blocking rather than returning nothing is what lets a program written the
/// obvious way work: an editor asking for a keystroke should wait for one, not
/// spin. The task yields between checks so the rest of the system keeps
/// running, and halts so an idle machine is not burning a core.
fn sys_read(fd: u64, buffer: u64, length: u64) -> u64 {
    if fd != 0 || length == 0 {
        return u64::MAX;
    }
    if user_slice(buffer, length).is_none() {
        return u64::MAX;
    }

    let out = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, length as usize) };
    let mut count = 0;

    loop {
        // Both input paths, so a program works over serial on machines with no
        // PS/2 controller just as it does with a keyboard.
        while count < out.len() {
            let Some(byte) = crate::keyboard::read().or_else(crate::serial::read) else {
                break;
            };
            out[count] = byte;
            count += 1;
        }

        if count > 0 {
            return count as u64;
        }

        crate::task::yield_now();
        x86_64::instructions::hlt();
    }
}

/// load(path, path_length, buffer, capacity) -> bytes read, or -1.
///
/// Whole files rather than file descriptors: no open/seek/close, no per-task
/// table to keep consistent when a program dies. It is the smallest thing that
/// lets programs actually use the disk, and it suits the size of file this
/// system deals in.
fn sys_load(path: u64, path_length: u64, buffer: u64, capacity: u64) -> u64 {
    let Some(path) = user_path(path, path_length) else {
        return u64::MAX;
    };
    if user_slice(buffer, capacity).is_none() {
        return u64::MAX;
    }

    let Ok(data) = crate::vfs::read(&path) else {
        return u64::MAX;
    };

    // A short buffer is not an error worth failing on: the program is told how
    // much it got, and asked for no more than it can hold.
    let count = data.len().min(capacity as usize);
    unsafe {
        core::ptr::copy_nonoverlapping(data.as_ptr(), buffer as *mut u8, count);
    }
    count as u64
}

/// store(path, path_length, buffer, count) -> bytes written, or -1.
fn sys_store(path: u64, path_length: u64, buffer: u64, count: u64) -> u64 {
    let Some(path) = user_path(path, path_length) else {
        return u64::MAX;
    };
    if user_slice(buffer, count).is_none() {
        return u64::MAX;
    }

    let bytes = unsafe { core::slice::from_raw_parts(buffer as *const u8, count as usize) };
    match crate::vfs::write(&path, bytes) {
        Ok(()) => count,
        Err(_) => u64::MAX,
    }
}

/// sleep(milliseconds). Rounded up to the 10 ms timer tick.
fn sys_sleep(milliseconds: u64) -> u64 {
    // A tick is 10 ms, so anything shorter still costs one; asking for zero
    // is a yield, which is what a caller means by it.
    if milliseconds == 0 {
        crate::task::yield_now();
        return 0;
    }

    let ticks = milliseconds.div_ceil(10);
    let until = crate::apic::ticks() + ticks;

    while crate::apic::ticks() < until {
        crate::task::yield_now();
        x86_64::instructions::hlt();
    }
    0
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
