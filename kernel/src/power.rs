//! Turning the machine off and restarting it.
//!
//! Reboot is straightforward. A clean ACPI shutdown is not: it needs the
//! `\_S5` object from the DSDT, which is encoded in AML and would mean writing
//! a bytecode interpreter. Instead this tries the shutdown ports that
//! emulators recognise, and says so plainly when none of them answer.

use x86_64::instructions::port::Port;

/// The 8042's command port; pulsing the CPU reset line goes through it.
const PS2_COMMAND: u16 = 0x64;
const PS2_INPUT_FULL: u8 = 1 << 1;
const PS2_RESET_CPU: u8 = 0xFE;

pub fn reboot() -> ! {
    unsafe {
        let mut port = Port::<u8>::new(PS2_COMMAND);

        // The controller ignores a command while its input buffer is full.
        for _ in 0..100_000 {
            if port.read() & PS2_INPUT_FULL == 0 {
                break;
            }
            core::hint::spin_loop();
        }

        port.write(PS2_RESET_CPU);
    }

    // If the keyboard controller did not reset us, force a triple fault: an
    // empty IDT makes the next interrupt unhandleable, which the CPU escalates
    // until it gives up and resets.
    unsafe {
        let empty = x86_64::structures::DescriptorTablePointer {
            limit: 0,
            base: x86_64::VirtAddr::new(0),
        };
        x86_64::instructions::tables::lidt(&empty);
        core::arch::asm!("int3", options(noreturn));
    }
}

pub fn shutdown() -> ! {
    unsafe {
        // QEMU 2.0+ and Bochs, then older QEMU, then VirtualBox.
        Port::<u16>::new(0x604).write(0x2000);
        Port::<u16>::new(0xB004).write(0x2000);
        Port::<u16>::new(0x4004).write(0x3400);
    }

    // Give the machine a moment to actually go. Where one of those ports is
    // honoured, execution stops here and the message below is never printed.
    crate::pit::wait_micros(200_000);

    crate::println!();
    crate::println!("Power off is not supported here - it needs ACPI. Halting instead;");
    crate::println!("it is safe to close the window or stop the VM now.");

    loop {
        x86_64::instructions::interrupts::disable();
        x86_64::instructions::hlt();
    }
}
