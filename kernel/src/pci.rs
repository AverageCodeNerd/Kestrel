//! PCI configuration space over the legacy 0xCF8/0xCFC port pair.
//!
//! Memory-mapped configuration (ECAM) would be faster and reach more devices,
//! but it has to be discovered through ACPI's MCFG table. The port pair works
//! everywhere and is enough to find a disk controller.

use x86_64::instructions::port::Port;

const ADDRESS_PORT: u16 = 0xCF8;
const DATA_PORT: u16 = 0xCFC;

/// Command register bits, at offset 0x04.
const COMMAND_MEMORY_SPACE: u16 = 1 << 1;
const COMMAND_BUS_MASTER: u16 = 1 << 2;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Address {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl Address {
    fn encode(&self, offset: u8) -> u32 {
        // Bit 31 enables the transfer; the low two bits of the offset must be
        // zero because config space is addressed in dwords.
        (1 << 31)
            | ((self.bus as u32) << 16)
            | ((self.device as u32) << 11)
            | ((self.function as u32) << 8)
            | ((offset as u32) & 0xFC)
    }

    pub fn read_u32(&self, offset: u8) -> u32 {
        unsafe {
            Port::<u32>::new(ADDRESS_PORT).write(self.encode(offset));
            Port::<u32>::new(DATA_PORT).read()
        }
    }

    pub fn write_u32(&self, offset: u8, value: u32) {
        unsafe {
            Port::<u32>::new(ADDRESS_PORT).write(self.encode(offset));
            Port::<u32>::new(DATA_PORT).write(value);
        }
    }

    pub fn read_u16(&self, offset: u8) -> u16 {
        // Config reads are always 32 bits wide; pick the half we want.
        let value = self.read_u32(offset & 0xFC);
        (value >> ((offset & 2) * 8)) as u16
    }

    pub fn vendor_id(&self) -> u16 {
        self.read_u16(0x00)
    }

    pub fn device_id(&self) -> u16 {
        self.read_u16(0x02)
    }

    pub fn class_code(&self) -> u8 {
        (self.read_u32(0x08) >> 24) as u8
    }

    pub fn subclass(&self) -> u8 {
        (self.read_u32(0x08) >> 16) as u8
    }

    pub fn prog_if(&self) -> u8 {
        (self.read_u32(0x08) >> 8) as u8
    }

    fn header_type(&self) -> u8 {
        (self.read_u32(0x0C) >> 16) as u8
    }

    /// Base address register `index` (0..=5), with its flag bits stripped.
    pub fn bar(&self, index: u8) -> u64 {
        let offset = 0x10 + index * 4;
        let low = self.read_u32(offset);

        // Bit 0 clear means a memory BAR; bits 2:1 == 0b10 means 64-bit, in
        // which case the next BAR holds the upper half.
        if low & 1 == 0 && (low >> 1) & 0b11 == 0b10 {
            let high = self.read_u32(offset + 4);
            ((high as u64) << 32) | (low & 0xFFFF_FFF0) as u64
        } else {
            (low & 0xFFFF_FFF0) as u64
        }
    }

    /// Let the device decode memory accesses and act as a DMA master. A
    /// controller that cannot bus-master cannot transfer anything.
    pub fn enable_memory_and_bus_master(&self) {
        let value = self.read_u32(0x04);
        let command = (value as u16) | COMMAND_MEMORY_SPACE | COMMAND_BUS_MASTER;
        self.write_u32(0x04, (value & 0xFFFF_0000) | command as u32);
    }
}

/// Visit every PCI function present, stopping when `matches` accepts one.
fn scan(mut matches: impl FnMut(&Address) -> bool) -> Option<Address> {
    for bus in 0..=255u16 {
        for device in 0..32u8 {
            let base = Address {
                bus: bus as u8,
                device,
                function: 0,
            };

            if base.vendor_id() == 0xFFFF {
                continue;
            }

            // Bit 7 of the header type marks a multi-function device; without
            // it, only function 0 exists.
            let functions = if base.header_type() & 0x80 != 0 { 8 } else { 1 };

            for function in 0..functions {
                let address = Address {
                    bus: bus as u8,
                    device,
                    function,
                };

                if address.vendor_id() != 0xFFFF && matches(&address) {
                    return Some(address);
                }
            }
        }
    }

    None
}

/// Find a device by its vendor and device identifiers.
pub fn find_by_id(vendor: u16, device: u16) -> Option<Address> {
    scan(|address| address.vendor_id() == vendor && address.device_id() == device)
}

/// Find the first device matching a class/subclass/interface triple.
pub fn find(class: u8, subclass: u8, prog_if: u8) -> Option<Address> {
    for bus in 0..=255u16 {
        for device in 0..32u8 {
            let base = Address {
                bus: bus as u8,
                device,
                function: 0,
            };

            if base.vendor_id() == 0xFFFF {
                continue;
            }

            // Bit 7 of the header type marks a multi-function device; without
            // it, only function 0 exists.
            let functions = if base.header_type() & 0x80 != 0 { 8 } else { 1 };

            for function in 0..functions {
                let address = Address {
                    bus: bus as u8,
                    device,
                    function,
                };

                if address.vendor_id() == 0xFFFF {
                    continue;
                }

                if address.class_code() == class
                    && address.subclass() == subclass
                    && address.prog_if() == prog_if
                {
                    return Some(address);
                }
            }
        }
    }

    None
}
