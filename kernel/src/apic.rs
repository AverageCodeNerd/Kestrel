//! Local APIC and I/O APIC.
//!
//! The local APIC is per-CPU and owns the timer and the end-of-interrupt
//! handshake. The I/O APIC is the chip that turns a device's wire into a
//! vector, and there is generally one for the whole system.

use core::sync::atomic::{AtomicU64, AtomicU32, Ordering};

use x86_64::registers::model_specific::Msr;

use crate::acpi::Acpi;
use crate::hhdm::phys_to_virt;
use crate::interrupts::{KEYBOARD_VECTOR, MOUSE_VECTOR, SPURIOUS_VECTOR, TIMER_VECTOR};
use crate::memory;
use crate::pit;

/// Ticks the timer interrupt has counted since it was started.
static TICKS: AtomicU64 = AtomicU64::new(0);
/// APIC timer counts per tick, recorded so the boot log can show the
/// calibration result.
static TIMER_HZ: AtomicU32 = AtomicU32::new(0);

/// How often the timer fires. 100 Hz gives a 10 ms scheduling quantum.
pub const TIMER_FREQUENCY: u32 = 100;

// Local APIC register offsets.
const LAPIC_ID: u32 = 0x020;
const LAPIC_EOI: u32 = 0x0B0;
const LAPIC_SPURIOUS: u32 = 0x0F0;
const LAPIC_LVT_TIMER: u32 = 0x320;
const LAPIC_TIMER_INITIAL: u32 = 0x380;
const LAPIC_TIMER_CURRENT: u32 = 0x390;
const LAPIC_TIMER_DIVIDE: u32 = 0x3E0;

/// Bit 8 of the spurious register is the APIC's master enable.
const LAPIC_SOFTWARE_ENABLE: u32 = 1 << 8;
/// Bit 17 of an LVT entry selects periodic mode.
const LVT_PERIODIC: u32 = 1 << 17;
/// Bit 16 masks an LVT entry.
const LVT_MASKED: u32 = 1 << 16;

static LAPIC_BASE: AtomicU64 = AtomicU64::new(0);

fn lapic() -> *mut u32 {
    LAPIC_BASE.load(Ordering::Relaxed) as *mut u32
}

fn lapic_read(register: u32) -> u32 {
    unsafe { lapic().byte_add(register as usize).read_volatile() }
}

fn lapic_write(register: u32, value: u32) {
    unsafe { lapic().byte_add(register as usize).write_volatile(value) }
}

/// Signal end-of-interrupt. Every hardware interrupt handler must call this,
/// or the APIC will never deliver that priority level again.
pub fn eoi() {
    lapic_write(LAPIC_EOI, 0);
}

pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Measured APIC timer counts per second.
pub fn timer_counts_per_second() -> u32 {
    TIMER_HZ.load(Ordering::Relaxed)
}

pub fn local_apic_id() -> u8 {
    (lapic_read(LAPIC_ID) >> 24) as u8
}

/// Bring up the local APIC, calibrate and start its timer, and route the
/// keyboard IRQ through the I/O APIC.
///
/// The APIC registers are device MMIO, which the bootloader's direct map does
/// not cover, so they have to be mapped before the first access.
pub fn init(acpi: &Acpi) -> Result<(), &'static str> {
    memory::map_mmio(acpi.local_apic_address, 0x1000).map_err(|_| "could not map the local APIC")?;
    if let Some(io_apic) = acpi.io_apic {
        memory::map_mmio(io_apic.address, 0x1000).map_err(|_| "could not map the I/O APIC")?;
    }

    LAPIC_BASE.store(phys_to_virt(acpi.local_apic_address) as u64, Ordering::Relaxed);

    unsafe {
        // Bit 11 of IA32_APIC_BASE is the hardware enable. Firmware usually
        // sets it already, but a VM may not.
        let mut apic_base = Msr::new(0x1B);
        let value = apic_base.read();
        apic_base.write(value | (1 << 11));
    }

    // Accept spurious interrupts on a dedicated vector and switch the APIC on.
    lapic_write(
        LAPIC_SPURIOUS,
        SPURIOUS_VECTOR as u32 | LAPIC_SOFTWARE_ENABLE,
    );

    start_timer();

    if let Some(io_apic) = acpi.io_apic {
        let identifier = local_apic_id();

        let (gsi, flags) = acpi.resolve_irq(1); // PS/2 keyboard
        unsafe { route(&io_apic, gsi, flags, KEYBOARD_VECTOR, identifier) };

        let (gsi, flags) = acpi.resolve_irq(12); // PS/2 mouse
        unsafe { route(&io_apic, gsi, flags, MOUSE_VECTOR, identifier) };
    }

    Ok(())
}

/// The APIC timer runs off the CPU's bus clock, whose rate is not
/// discoverable, so measure it against the PIT.
fn start_timer() {
    const CALIBRATION_MICROS: u32 = 10_000;

    lapic_write(LAPIC_TIMER_DIVIDE, DIVIDE_BY_16);
    // Mask the timer while calibrating so it cannot fire mid-measurement.
    lapic_write(LAPIC_LVT_TIMER, LVT_MASKED);

    lapic_write(LAPIC_TIMER_INITIAL, u32::MAX);
    pit::wait_micros(CALIBRATION_MICROS);
    let remaining = lapic_read(LAPIC_TIMER_CURRENT);

    let elapsed = u32::MAX - remaining;
    let per_second = elapsed.saturating_mul(1_000_000 / CALIBRATION_MICROS);
    TIMER_HZ.store(per_second, Ordering::Relaxed);

    arm_timer(per_second);
}

const DIVIDE_BY_16: u32 = 0b0011;

/// Start this core's timer at the already-measured rate.
fn arm_timer(per_second: u32) {
    // Never program 0, or the timer silently stops.
    let count = (per_second / TIMER_FREQUENCY).max(1);

    lapic_write(LAPIC_TIMER_DIVIDE, DIVIDE_BY_16);
    // Unmask in periodic mode, then arm it: writing the initial count is what
    // (re)starts the countdown.
    lapic_write(LAPIC_LVT_TIMER, TIMER_VECTOR as u32 | LVT_PERIODIC);
    lapic_write(LAPIC_TIMER_INITIAL, count);
}

/// Bring up an application processor's local APIC.
///
/// Each core has its own LAPIC behind the same physical address, so the
/// mapping and calibration done by the boot processor are reused; only the
/// per-core registers need writing.
pub fn init_ap() {
    unsafe {
        let mut apic_base = Msr::new(0x1B);
        let value = apic_base.read();
        apic_base.write(value | (1 << 11));
    }

    lapic_write(
        LAPIC_SPURIOUS,
        SPURIOUS_VECTOR as u32 | LAPIC_SOFTWARE_ENABLE,
    );

    // Calibrating again here would fight the boot processor over the PIT.
    arm_timer(TIMER_HZ.load(Ordering::Relaxed));
}

// I/O APIC registers are reached through a select/window pair rather than
// being directly memory mapped.
const IOAPIC_REGSEL: usize = 0x00;
const IOAPIC_WINDOW: usize = 0x10;
/// Redirection entries start here, two 32-bit registers each.
const IOAPIC_REDIRECTION_BASE: u32 = 0x10;

unsafe fn io_apic_read(base: *mut u8, register: u32) -> u32 {
    unsafe {
        base.add(IOAPIC_REGSEL).cast::<u32>().write_volatile(register);
        base.add(IOAPIC_WINDOW).cast::<u32>().read_volatile()
    }
}

unsafe fn io_apic_write(base: *mut u8, register: u32, value: u32) {
    unsafe {
        base.add(IOAPIC_REGSEL).cast::<u32>().write_volatile(register);
        base.add(IOAPIC_WINDOW).cast::<u32>().write_volatile(value);
    }
}

/// Point a global system interrupt at `vector` on the given local APIC.
///
/// `flags` comes from the ACPI interrupt source override: bits 0..1 encode
/// polarity and bits 2..3 encode trigger mode, with 0 meaning "bus default"
/// (active high, edge triggered for ISA).
unsafe fn route(io_apic: &crate::acpi::IoApic, gsi: u32, flags: u16, vector: u8, destination: u8) {
    let base = phys_to_virt(io_apic.address);
    let index = gsi - io_apic.gsi_base;
    let register = IOAPIC_REDIRECTION_BASE + index * 2;

    let active_low = (flags & 0b11) == 0b11;
    let level_triggered = ((flags >> 2) & 0b11) == 0b11;

    let mut low = vector as u32;
    if active_low {
        low |= 1 << 13;
    }
    if level_triggered {
        low |= 1 << 15;
    }
    // Leave delivery mode 0 (fixed), destination mode 0 (physical), unmasked.

    unsafe {
        // Program the high word (destination) first, while the entry is still
        // masked by its power-on default.
        io_apic_write(base, register + 1, (destination as u32) << 24);
        io_apic_write(base, register, low);
    }
}

/// Read back an I/O APIC redirection entry, for diagnostics.
pub unsafe fn redirection_entry(io_apic: &crate::acpi::IoApic, gsi: u32) -> u32 {
    let base = phys_to_virt(io_apic.address);
    let register = IOAPIC_REDIRECTION_BASE + (gsi - io_apic.gsi_base) * 2;
    unsafe { io_apic_read(base, register) }
}
