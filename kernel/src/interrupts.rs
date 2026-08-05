//! Interrupt Descriptor Table and CPU exception handlers.

use spin::Lazy;
use x86_64::PrivilegeLevel;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use crate::gdt;

/// Hardware interrupt vectors. Anything below 32 belongs to the CPU.
pub const TIMER_VECTOR: u8 = 32;
pub const KEYBOARD_VECTOR: u8 = 33;
pub const MOUSE_VECTOR: u8 = 44;
/// The APIC requires a spurious vector with the low four bits set.
pub const SPURIOUS_VECTOR: u8 = 0xFF;

/// Did this fault happen in ring 3?
///
/// A user program's mistake should kill only that program; the same fault in
/// kernel code is unrecoverable.
fn from_user(frame: &InterruptStackFrame) -> bool {
    frame.code_segment.rpl() == PrivilegeLevel::Ring3
}

/// Exit status for a task killed by a CPU exception, following the Unix
/// convention of 128 plus a signal number.
const KILLED: u64 = 139;

/// Defines an exception handler: fatal in the kernel, but survivable for a
/// user process, which is simply terminated.
///
/// The `code` form is for the exceptions that push an error code; the CPU's
/// stack layout differs between the two, so they can't share a signature.
macro_rules! fatal {
    ($name:ident, $description:literal) => {
        extern "x86-interrupt" fn $name(frame: InterruptStackFrame) {
            if from_user(&frame) {
                crate::println!(
                    "\n[{} at {:#x}: killed]",
                    $description,
                    frame.instruction_pointer.as_u64()
                );
                crate::task::finish(KILLED);
            }
            panic!("EXCEPTION: {}\n{:#?}", $description, frame);
        }
    };
    ($name:ident, $description:literal, code) => {
        extern "x86-interrupt" fn $name(frame: InterruptStackFrame, error_code: u64) {
            if from_user(&frame) {
                crate::println!(
                    "\n[{} at {:#x}: killed]",
                    $description,
                    frame.instruction_pointer.as_u64()
                );
                crate::task::finish(KILLED);
            }
            panic!(
                "EXCEPTION: {} (error code {:#x})\n{:#?}",
                $description, error_code, frame
            );
        }
    };
}

fatal!(divide_error, "divide error");
fatal!(debug, "debug");
fatal!(non_maskable_interrupt, "non-maskable interrupt");
fatal!(overflow, "overflow");
fatal!(bound_range_exceeded, "bound range exceeded");
fatal!(invalid_opcode, "invalid opcode");
fatal!(device_not_available, "device not available");
fatal!(x87_floating_point, "x87 floating point");
fatal!(simd_floating_point, "SIMD floating point");
fatal!(virtualization, "virtualization");

fatal!(invalid_tss, "invalid TSS", code);
fatal!(segment_not_present, "segment not present", code);
fatal!(stack_segment_fault, "stack segment fault", code);
fatal!(general_protection_fault, "general protection fault", code);
fatal!(alignment_check, "alignment check", code);

/// `int3`. Returns normally, so a breakpoint is a usable debugging tool
/// rather than a crash.
extern "x86-interrupt" fn breakpoint(frame: InterruptStackFrame) {
    crate::println!("breakpoint at {:#x}", frame.instruction_pointer.as_u64());
}

/// CR2 holds the address that faulted; it must be read before anything else
/// can trigger a second page fault and overwrite it.
extern "x86-interrupt" fn page_fault(frame: InterruptStackFrame, error_code: PageFaultErrorCode) {
    let accessed = Cr2::read();

    if from_user(&frame) {
        crate::println!(
            "\n[segmentation fault at {:#x}, accessing {:#x}: killed]",
            frame.instruction_pointer.as_u64(),
            accessed.map_or(0, |address| address.as_u64())
        );
        crate::task::finish(KILLED);
    }

    panic!(
        "EXCEPTION: page fault\naccessed: {:?}\nerror: {:?}\n{:#?}",
        accessed, error_code, frame
    );
}

/// Reached when a fault handler itself faults. Runs on its own IST stack, so
/// it still works when the kernel stack has overflowed. Cannot return: there
/// is no sensible state to return to.
extern "x86-interrupt" fn double_fault(frame: InterruptStackFrame, error_code: u64) -> ! {
    panic!("EXCEPTION: double fault (code {error_code})\n{frame:#?}");
}

extern "x86-interrupt" fn machine_check(frame: InterruptStackFrame) -> ! {
    panic!("EXCEPTION: machine check\n{frame:#?}");
}

/// Hardware interrupt handlers must not print or allocate: they can preempt
/// code that already holds those locks, and blocking here would deadlock the
/// machine. They only touch lock-free counters and ring buffers.
extern "x86-interrupt" fn timer(_frame: InterruptStackFrame) {
    let cpu = crate::cpu::index();
    crate::cpu::tick(cpu);

    // End-of-interrupt must be signalled *before* switching away, or the APIC
    // stays blocked at this priority for as long as the next task runs.
    crate::apic::eoi();

    // Uptime is counted once, by the boot processor.
    if cpu == 0 {
        crate::apic::tick();
    }

    // Every core preempts whatever it is running, from its own queue.
    crate::task::schedule();
}

extern "x86-interrupt" fn keyboard(_frame: InterruptStackFrame) {
    crate::keyboard::handle_interrupt();
    crate::apic::eoi();
}

extern "x86-interrupt" fn mouse(_frame: InterruptStackFrame) {
    crate::mouse::handle_interrupt();
    crate::apic::eoi();
}

/// Delivered when an interrupt is withdrawn before it can be dispatched.
/// Deliberately does *not* signal end-of-interrupt.
extern "x86-interrupt" fn spurious(_frame: InterruptStackFrame) {}

static IDT: Lazy<InterruptDescriptorTable> = Lazy::new(|| {
    let mut idt = InterruptDescriptorTable::new();

    idt.divide_error.set_handler_fn(divide_error);
    idt.debug.set_handler_fn(debug);
    idt.non_maskable_interrupt.set_handler_fn(non_maskable_interrupt);
    idt.breakpoint.set_handler_fn(breakpoint);
    idt.overflow.set_handler_fn(overflow);
    idt.bound_range_exceeded.set_handler_fn(bound_range_exceeded);
    idt.invalid_opcode.set_handler_fn(invalid_opcode);
    idt.device_not_available.set_handler_fn(device_not_available);
    idt.invalid_tss.set_handler_fn(invalid_tss);
    idt.segment_not_present.set_handler_fn(segment_not_present);
    idt.stack_segment_fault.set_handler_fn(stack_segment_fault);
    idt.general_protection_fault.set_handler_fn(general_protection_fault);
    idt.x87_floating_point.set_handler_fn(x87_floating_point);
    idt.alignment_check.set_handler_fn(alignment_check);
    idt.machine_check.set_handler_fn(machine_check);
    idt.simd_floating_point.set_handler_fn(simd_floating_point);
    idt.virtualization.set_handler_fn(virtualization);

    // These two get dedicated stacks from the TSS, because the condition that
    // triggered them may be exactly a broken kernel stack.
    unsafe {
        idt.double_fault
            .set_handler_fn(double_fault)
            .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
        idt.page_fault
            .set_handler_fn(page_fault)
            .set_stack_index(gdt::PAGE_FAULT_IST_INDEX);
    }

    idt[TIMER_VECTOR].set_handler_fn(timer);
    idt[KEYBOARD_VECTOR].set_handler_fn(keyboard);
    idt[MOUSE_VECTOR].set_handler_fn(mouse);
    idt[SPURIOUS_VECTOR].set_handler_fn(spurious);

    idt
});

pub fn init() {
    IDT.load();
}

/// Unmask interrupts on this CPU. Only safe once the APIC is programmed.
pub fn enable() {
    x86_64::instructions::interrupts::enable();
}
