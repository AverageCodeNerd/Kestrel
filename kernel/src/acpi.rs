//! Just enough ACPI to find the APICs.
//!
//! Everything here reads through raw pointers with explicit unaligned loads.
//! ACPI tables are byte-packed and, worse, the XSDT's array of 64-bit table
//! pointers starts at offset 36 — so it is never 8-byte aligned. Naive typed
//! reads would be undefined behaviour.

use crate::hhdm::phys_to_virt;

unsafe fn read_u8(base: *const u8, offset: usize) -> u8 {
    unsafe { base.add(offset).read() }
}

unsafe fn read_u32(base: *const u8, offset: usize) -> u32 {
    unsafe { base.add(offset).cast::<u32>().read_unaligned() }
}

unsafe fn read_u64(base: *const u8, offset: usize) -> u64 {
    unsafe { base.add(offset).cast::<u64>().read_unaligned() }
}

unsafe fn signature(table: *const u8) -> [u8; 4] {
    unsafe { [read_u8(table, 0), read_u8(table, 1), read_u8(table, 2), read_u8(table, 3)] }
}

/// Sum of all bytes must be zero for a valid table.
unsafe fn checksum_ok(table: *const u8, length: usize) -> bool {
    let mut sum: u8 = 0;
    for i in 0..length {
        sum = sum.wrapping_add(unsafe { read_u8(table, i) });
    }
    sum == 0
}

/// An I/O APIC and the global system interrupt it starts at.
#[derive(Clone, Copy)]
pub struct IoApic {
    pub address: u64,
    pub gsi_base: u32,
}

/// Where the firmware has rewired a legacy ISA IRQ.
#[derive(Clone, Copy)]
pub struct Override {
    pub gsi: u32,
    pub flags: u16,
}

pub struct Acpi {
    pub local_apic_address: u64,
    pub io_apic: Option<IoApic>,
    /// Indexed by ISA IRQ number. `None` means the IRQ maps to the identical GSI.
    pub overrides: [Option<Override>; 16],
}

impl Acpi {
    /// Resolve an ISA IRQ to its global system interrupt, honouring any
    /// override the firmware declared. On QEMU, IRQ0 is famously remapped to
    /// GSI 2.
    pub fn resolve_irq(&self, irq: u8) -> (u32, u16) {
        match self.overrides.get(irq as usize).copied().flatten() {
            Some(o) => (o.gsi, o.flags),
            None => (irq as u32, 0),
        }
    }
}

/// # Safety
/// `rsdp` must be the address Limine reported for the RSDP.
pub unsafe fn parse(rsdp: *const u8) -> Option<Acpi> {
    unsafe {
        let mut magic = [0u8; 8];
        for (i, byte) in magic.iter_mut().enumerate() {
            *byte = read_u8(rsdp, i);
        }
        if &magic != b"RSD PTR " {
            return None;
        }

        let revision = read_u8(rsdp, 15);

        // ACPI 1.0 has only a 32-bit RSDT; 2.0+ adds the 64-bit XSDT, which
        // takes precedence when present.
        let (sdt, entry_size) = if revision >= 2 {
            let xsdt = read_u64(rsdp, 24);
            if xsdt != 0 {
                (phys_to_virt(xsdt) as *const u8, 8)
            } else {
                (phys_to_virt(read_u32(rsdp, 16) as u64) as *const u8, 4)
            }
        } else {
            (phys_to_virt(read_u32(rsdp, 16) as u64) as *const u8, 4)
        };

        let madt = find_table(sdt, entry_size, b"APIC")?;
        parse_madt(madt)
    }
}

/// Walk the RSDT/XSDT looking for a table with the given signature.
unsafe fn find_table(sdt: *const u8, entry_size: usize, want: &[u8; 4]) -> Option<*const u8> {
    unsafe {
        let length = read_u32(sdt, 4) as usize;
        if length < 36 || !checksum_ok(sdt, length) {
            return None;
        }

        let count = (length - 36) / entry_size;
        for i in 0..count {
            let offset = 36 + i * entry_size;
            let phys = if entry_size == 8 {
                read_u64(sdt, offset)
            } else {
                read_u32(sdt, offset) as u64
            };

            let table = phys_to_virt(phys) as *const u8;
            if signature(table) == *want {
                let table_len = read_u32(table, 4) as usize;
                if checksum_ok(table, table_len) {
                    return Some(table);
                }
            }
        }
        None
    }
}

/// MADT entry type codes.
const ENTRY_IO_APIC: u8 = 1;
const ENTRY_INTERRUPT_OVERRIDE: u8 = 2;
const ENTRY_LOCAL_APIC_OVERRIDE: u8 = 5;

unsafe fn parse_madt(madt: *const u8) -> Option<Acpi> {
    unsafe {
        let length = read_u32(madt, 4) as usize;

        let mut acpi = Acpi {
            local_apic_address: read_u32(madt, 36) as u64,
            io_apic: None,
            overrides: [None; 16],
        };

        // Variable-length entries start after the 44-byte MADT header.
        let mut offset = 44;
        while offset + 2 <= length {
            let kind = read_u8(madt, offset);
            let entry_len = read_u8(madt, offset + 1) as usize;
            if entry_len < 2 {
                break; // malformed; refuse to spin forever
            }

            match kind {
                ENTRY_IO_APIC if acpi.io_apic.is_none() => {
                    acpi.io_apic = Some(IoApic {
                        address: read_u32(madt, offset + 4) as u64,
                        gsi_base: read_u32(madt, offset + 8),
                    });
                }
                ENTRY_INTERRUPT_OVERRIDE => {
                    let source = read_u8(madt, offset + 3) as usize;
                    let gsi = read_u32(madt, offset + 4);
                    let flags = madt
                        .add(offset + 8)
                        .cast::<u16>()
                        .read_unaligned();
                    if source < 16 {
                        acpi.overrides[source] = Some(Override { gsi, flags });
                    }
                }
                ENTRY_LOCAL_APIC_OVERRIDE => {
                    // 64-bit address supersedes the 32-bit one in the header.
                    acpi.local_apic_address = read_u64(madt, offset + 4);
                }
                _ => {}
            }

            offset += entry_len;
        }

        Some(acpi)
    }
}
