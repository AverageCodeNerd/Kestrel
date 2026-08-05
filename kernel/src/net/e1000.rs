//! Intel 8254x ("e1000") gigabit Ethernet driver.
//!
//! The card is a DMA master with two rings of descriptors: one for receive,
//! one for transmit. Each descriptor points at a buffer by *physical* address,
//! so everything here comes from the frame allocator rather than the heap.
//!
//! Both rings are polled. Interrupt-driven receive would be better under load,
//! but polling keeps the driver honest about ordering and is plenty for a
//! machine that talks to the network on demand.

use alloc::vec::Vec;
use spin::Mutex;

use crate::hhdm::phys_to_virt;
use crate::{memory, pci};

/// QEMU's default e1000, an 82540EM.
const VENDOR_INTEL: u16 = 0x8086;
const DEVICE_82540EM: u16 = 0x100E;

// Registers, as byte offsets into BAR0.
const REG_CTRL: usize = 0x0000;
const REG_STATUS: usize = 0x0008;
const REG_ICR: usize = 0x00C0;
const REG_IMC: usize = 0x00D8;
const REG_RCTL: usize = 0x0100;
const REG_TCTL: usize = 0x0400;
const REG_TIPG: usize = 0x0410;
const REG_RDBAL: usize = 0x2800;
const REG_RDBAH: usize = 0x2804;
const REG_RDLEN: usize = 0x2808;
const REG_RDH: usize = 0x2810;
const REG_RDT: usize = 0x2818;
const REG_TDBAL: usize = 0x3800;
const REG_TDBAH: usize = 0x3804;
const REG_TDLEN: usize = 0x3808;
const REG_TDH: usize = 0x3810;
const REG_TDT: usize = 0x3818;
const REG_MTA: usize = 0x5200;
const REG_RAL: usize = 0x5400;
const REG_RAH: usize = 0x5404;

/// CTRL bits.
const CTRL_SLU: u32 = 1 << 6; // set link up
const CTRL_ASDE: u32 = 1 << 5; // auto speed detect
const CTRL_RST: u32 = 1 << 26;

/// STATUS bits.
const STATUS_LINK_UP: u32 = 1 << 1;

/// RCTL bits.
const RCTL_EN: u32 = 1 << 1;
const RCTL_BAM: u32 = 1 << 15; // accept broadcast
const RCTL_SECRC: u32 = 1 << 26; // strip the Ethernet CRC

/// TCTL bits.
const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3; // pad short packets
/// Collision threshold and distance. QEMU transmits happily without these,
/// but VirtualBox's emulation follows the manual and never completes a
/// descriptor if they are left zero.
const TCTL_CT: u32 = 0x0F << 4;
const TCTL_COLD_FULL_DUPLEX: u32 = 0x40 << 12;

/// Transmit descriptor command bits.
const TX_CMD_EOP: u8 = 1 << 0; // end of packet
const TX_CMD_IFCS: u8 = 1 << 1; // insert the frame checksum
const TX_CMD_RS: u8 = 1 << 3; // report status

/// Descriptor status bits.
const STATUS_DD: u8 = 1 << 0; // descriptor done
const RX_STATUS_EOP: u8 = 1 << 1;

const RING_SIZE: usize = 32;
const BUFFER_SIZE: usize = 2048;
pub const MTU: usize = 1500;

/// Receive descriptor, exactly as the hardware defines it.
#[repr(C)]
#[derive(Clone, Copy)]
struct RxDescriptor {
    address: u64,
    length: u16,
    checksum: u16,
    status: u8,
    errors: u8,
    special: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TxDescriptor {
    address: u64,
    length: u16,
    checksum_offset: u8,
    command: u8,
    status: u8,
    checksum_start: u8,
    special: u16,
}

pub struct E1000 {
    registers: *mut u8,
    rx_ring: *mut RxDescriptor,
    tx_ring: *mut TxDescriptor,
    /// Virtual addresses of the buffers, for copying in and out.
    rx_buffers: Vec<*mut u8>,
    tx_buffers: Vec<*mut u8>,
    rx_next: usize,
    tx_next: usize,
    pub mac: [u8; 6],
}

// The card is owned exclusively by the Mutex below.
unsafe impl Send for E1000 {}

impl E1000 {
    fn read(&self, register: usize) -> u32 {
        unsafe { self.registers.add(register).cast::<u32>().read_volatile() }
    }

    fn write(&self, register: usize, value: u32) {
        unsafe {
            self.registers
                .add(register)
                .cast::<u32>()
                .write_volatile(value)
        }
    }

    pub fn link_up(&self) -> bool {
        self.read(REG_STATUS) & STATUS_LINK_UP != 0
    }

    /// Send one frame. Blocks until the card reports the descriptor done.
    pub fn transmit(&mut self, frame: &[u8]) -> Result<(), &'static str> {
        if frame.len() > BUFFER_SIZE {
            return Err("frame is too large");
        }

        let index = self.tx_next;

        unsafe {
            core::ptr::copy_nonoverlapping(frame.as_ptr(), self.tx_buffers[index], frame.len());

            let descriptor = self.tx_ring.add(index);
            (*descriptor).length = frame.len() as u16;
            // Ask the card to append the CRC and tell us when it is done.
            (*descriptor).command = TX_CMD_EOP | TX_CMD_IFCS | TX_CMD_RS;
            (*descriptor).status = 0;
        }

        self.tx_next = (index + 1) % RING_SIZE;
        // Advancing the tail is what hands the descriptor to the card.
        self.write(REG_TDT, self.tx_next as u32);

        for _ in 0..1_000_000 {
            if unsafe { (*self.tx_ring.add(index)).status } & STATUS_DD != 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }

        Err("timed out transmitting")
    }

    /// Take the next received frame, if one is waiting.
    pub fn receive(&mut self) -> Option<Vec<u8>> {
        let index = self.rx_next;
        let descriptor = unsafe { &mut *self.rx_ring.add(index) };

        if descriptor.status & STATUS_DD == 0 {
            return None;
        }

        let frame = if descriptor.status & RX_STATUS_EOP != 0 && descriptor.errors == 0 {
            let length = descriptor.length as usize;
            let mut data = alloc::vec![0u8; length];
            unsafe {
                core::ptr::copy_nonoverlapping(self.rx_buffers[index], data.as_mut_ptr(), length);
            }
            Some(data)
        } else {
            // Split or damaged frames are dropped; nothing here reassembles.
            None
        };

        // Hand the descriptor back to the card.
        descriptor.status = 0;
        self.rx_next = (index + 1) % RING_SIZE;
        self.write(REG_RDT, index as u32);

        frame
    }
}

pub static NIC: Mutex<Option<E1000>> = Mutex::new(None);

fn frame_address() -> Result<(u64, *mut u8), &'static str> {
    let frame = memory::allocate_zeroed_frame().ok_or("out of memory for network buffers")?;
    let physical = frame.start_address().as_u64();
    Ok((physical, phys_to_virt(physical)))
}

/// Find and initialise the card, returning its MAC address.
pub fn init() -> Result<[u8; 6], &'static str> {
    let device = pci::find_by_id(VENDOR_INTEL, DEVICE_82540EM).ok_or("no e1000 found")?;
    device.enable_memory_and_bus_master();

    let bar0 = device.bar(0);
    if bar0 == 0 {
        return Err("e1000 has no register window");
    }
    // The register file is 128 KiB of device memory, outside the direct map.
    memory::map_mmio(bar0, 0x20000).map_err(|_| "could not map the e1000 registers")?;

    let (rx_ring_phys, rx_ring_virt) = frame_address()?;
    let (tx_ring_phys, tx_ring_virt) = frame_address()?;

    let mut nic = E1000 {
        registers: phys_to_virt(bar0),
        rx_ring: rx_ring_virt.cast(),
        tx_ring: tx_ring_virt.cast(),
        rx_buffers: Vec::new(),
        tx_buffers: Vec::new(),
        rx_next: 0,
        tx_next: 0,
        mac: [0; 6],
    };

    // Reset, then wait for the card to come back.
    nic.write(REG_CTRL, nic.read(REG_CTRL) | CTRL_RST);
    for _ in 0..1_000_000 {
        if nic.read(REG_CTRL) & CTRL_RST == 0 {
            break;
        }
        core::hint::spin_loop();
    }

    // Silence interrupts: this driver polls.
    nic.write(REG_IMC, 0xFFFF_FFFF);
    let _ = nic.read(REG_ICR);

    nic.write(REG_CTRL, nic.read(REG_CTRL) | CTRL_SLU | CTRL_ASDE);

    // The firmware leaves the MAC in the first receive address register.
    let low = nic.read(REG_RAL);
    let high = nic.read(REG_RAH);
    nic.mac = [
        low as u8,
        (low >> 8) as u8,
        (low >> 16) as u8,
        (low >> 24) as u8,
        high as u8,
        (high >> 8) as u8,
    ];

    // Clear the multicast table filter, or stale entries drop traffic.
    for index in 0..128 {
        nic.write(REG_MTA + index * 4, 0);
    }

    // Two 2 KiB buffers fit in each 4 KiB frame.
    for index in 0..RING_SIZE {
        if index % 2 == 0 {
            let (physical, virtual_address) = frame_address()?;
            nic.rx_buffers.push(virtual_address);
            unsafe {
                (*nic.rx_ring.add(index)).address = physical;
                (*nic.rx_ring.add(index)).status = 0;
            }
        } else {
            let previous = nic.rx_buffers[index - 1];
            let virtual_address = unsafe { previous.add(BUFFER_SIZE) };
            nic.rx_buffers.push(virtual_address);
            unsafe {
                let physical = (*nic.rx_ring.add(index - 1)).address + BUFFER_SIZE as u64;
                (*nic.rx_ring.add(index)).address = physical;
                (*nic.rx_ring.add(index)).status = 0;
            }
        }
    }

    for index in 0..RING_SIZE {
        if index % 2 == 0 {
            let (physical, virtual_address) = frame_address()?;
            nic.tx_buffers.push(virtual_address);
            unsafe {
                (*nic.tx_ring.add(index)).address = physical;
                (*nic.tx_ring.add(index)).status = STATUS_DD;
                (*nic.tx_ring.add(index)).command = 0;
            }
        } else {
            let previous = nic.tx_buffers[index - 1];
            nic.tx_buffers.push(unsafe { previous.add(BUFFER_SIZE) });
            unsafe {
                let physical = (*nic.tx_ring.add(index - 1)).address + BUFFER_SIZE as u64;
                (*nic.tx_ring.add(index)).address = physical;
                (*nic.tx_ring.add(index)).status = STATUS_DD;
                (*nic.tx_ring.add(index)).command = 0;
            }
        }
    }

    // Point the card at the receive ring. The tail sits one behind the head,
    // marking every descriptor as available to the card.
    nic.write(REG_RDBAL, rx_ring_phys as u32);
    nic.write(REG_RDBAH, (rx_ring_phys >> 32) as u32);
    nic.write(REG_RDLEN, (RING_SIZE * 16) as u32);
    nic.write(REG_RDH, 0);
    nic.write(REG_RDT, (RING_SIZE - 1) as u32);
    nic.write(REG_RCTL, RCTL_EN | RCTL_BAM | RCTL_SECRC);

    nic.write(REG_TDBAL, tx_ring_phys as u32);
    nic.write(REG_TDBAH, (tx_ring_phys >> 32) as u32);
    nic.write(REG_TDLEN, (RING_SIZE * 16) as u32);
    nic.write(REG_TDH, 0);
    nic.write(REG_TDT, 0);
    nic.write(
        REG_TCTL,
        TCTL_EN | TCTL_PSP | TCTL_CT | TCTL_COLD_FULL_DUPLEX,
    );
    // Inter-packet gap, as the manual specifies for copper.
    nic.write(REG_TIPG, 10 | (8 << 10) | (6 << 20));

    let mac = nic.mac;
    *NIC.lock() = Some(nic);
    Ok(mac)
}

/// Run `body` against the card.
pub fn with<T>(body: impl FnOnce(&mut E1000) -> T) -> Option<T> {
    x86_64::instructions::interrupts::without_interrupts(|| NIC.lock().as_mut().map(body))
}
