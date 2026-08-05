//! An AHCI (SATA) driver, read-only, polled.
//!
//! AHCI rather than virtio-blk because QEMU, Hyper-V and VirtualBox all
//! present a SATA controller, so the same driver works on all of them.
//!
//! The controller is a DMA master: it reads command structures out of RAM and
//! writes sector data back, so everything it touches must be described by
//! *physical* addresses. That is why the buffers here come from the frame
//! allocator rather than the heap — the heap gives virtual addresses whose
//! physical backing is neither known nor contiguous.

use spin::Mutex;

use crate::hhdm::phys_to_virt;
use crate::{memory, pci};

pub const SECTOR_SIZE: usize = 512;

// Host control registers, at the base of the ABAR.
const HBA_GHC: usize = 0x04;
const HBA_PI: usize = 0x0C;

/// GHC bits.
const GHC_AHCI_ENABLE: u32 = 1 << 31;

// Per-port registers, at 0x100 + port * 0x80.
const PORT_BASE: usize = 0x100;
const PORT_STRIDE: usize = 0x80;

const PORT_CLB: usize = 0x00;
const PORT_CLBU: usize = 0x04;
const PORT_FB: usize = 0x08;
const PORT_FBU: usize = 0x0C;
const PORT_IS: usize = 0x10;
const PORT_CMD: usize = 0x18;
const PORT_TFD: usize = 0x20;
const PORT_SIG: usize = 0x24;
const PORT_SSTS: usize = 0x28;
const PORT_SERR: usize = 0x30;
const PORT_CI: usize = 0x38;

/// CMD bits: start, FIS receive enable, and their running acknowledgements.
const CMD_ST: u32 = 1 << 0;
const CMD_FRE: u32 = 1 << 4;
const CMD_FR: u32 = 1 << 14;
const CMD_CR: u32 = 1 << 15;

/// Task file: busy, and data request.
const TFD_BSY: u32 = 1 << 7;
const TFD_DRQ: u32 = 1 << 3;
const TFD_ERR: u32 = 1 << 0;

/// Interrupt status: task file error.
const IS_TFES: u32 = 1 << 30;

/// A plain SATA disk, as opposed to ATAPI or a port multiplier.
const SIGNATURE_SATA: u32 = 0x0000_0101;

const ATA_READ_DMA_EXT: u8 = 0x25;
const ATA_WRITE_DMA_EXT: u8 = 0x35;
/// Tells the drive to commit its write cache to the platters.
const ATA_FLUSH_CACHE_EXT: u8 = 0xEA;

/// One 4 KiB frame of DMA buffer, so eight sectors move per command.
const SECTORS_PER_TRANSFER: usize = 4096 / SECTOR_SIZE;

pub struct Ahci {
    abar: *mut u8,
    port: usize,
    /// Physical addresses of the structures the controller reads.
    command_list: u64,
    fis_area: u64,
    command_table: u64,
    buffer: u64,
}

// The controller is owned exclusively by the Mutex below.
unsafe impl Send for Ahci {}

impl Ahci {
    fn read_register(&self, offset: usize) -> u32 {
        unsafe { self.abar.add(offset).cast::<u32>().read_volatile() }
    }

    fn write_register(&self, offset: usize, value: u32) {
        unsafe { self.abar.add(offset).cast::<u32>().write_volatile(value) }
    }

    fn port_offset(&self, register: usize) -> usize {
        PORT_BASE + self.port * PORT_STRIDE + register
    }

    fn read_port(&self, register: usize) -> u32 {
        self.read_register(self.port_offset(register))
    }

    fn write_port(&self, register: usize, value: u32) {
        self.write_register(self.port_offset(register), value)
    }

    /// Halt the port's command engine so its pointers can be reprogrammed.
    fn stop(&self) {
        let mut command = self.read_port(PORT_CMD);
        command &= !(CMD_ST | CMD_FRE);
        self.write_port(PORT_CMD, command);

        // Wait for the hardware to acknowledge by clearing its running bits.
        for _ in 0..1_000_000 {
            let status = self.read_port(PORT_CMD);
            if status & (CMD_CR | CMD_FR) == 0 {
                return;
            }
            core::hint::spin_loop();
        }
    }

    fn start(&self) {
        // Never set ST while the command engine is still running.
        for _ in 0..1_000_000 {
            if self.read_port(PORT_CMD) & CMD_CR == 0 {
                break;
            }
            core::hint::spin_loop();
        }

        let command = self.read_port(PORT_CMD) | CMD_FRE | CMD_ST;
        self.write_port(PORT_CMD, command);
    }

    /// Read `count` sectors starting at `lba` into `out`.
    pub fn read(&self, mut lba: u64, mut count: usize, out: &mut [u8]) -> Result<(), &'static str> {
        if out.len() < count * SECTOR_SIZE {
            return Err("output buffer is too small");
        }

        let mut written = 0;

        while count > 0 {
            let chunk = count.min(SECTORS_PER_TRANSFER);
            self.read_chunk(lba, chunk)?;

            let source = phys_to_virt(self.buffer);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    source,
                    out.as_mut_ptr().add(written),
                    chunk * SECTOR_SIZE,
                );
            }

            written += chunk * SECTOR_SIZE;
            lba += chunk as u64;
            count -= chunk;
        }

        Ok(())
    }

    /// Write `count` sectors starting at `lba` from `data`.
    pub fn write(&self, mut lba: u64, mut count: usize, data: &[u8]) -> Result<(), &'static str> {
        if data.len() < count * SECTOR_SIZE {
            return Err("input buffer is too small");
        }

        let mut read = 0;

        while count > 0 {
            let chunk = count.min(SECTORS_PER_TRANSFER);

            // Stage the data in the DMA buffer, which the controller can
            // reach by physical address.
            let destination = phys_to_virt(self.buffer);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    data.as_ptr().add(read),
                    destination,
                    chunk * SECTOR_SIZE,
                );
            }

            self.command(ATA_WRITE_DMA_EXT, lba, chunk, true)?;

            read += chunk * SECTOR_SIZE;
            lba += chunk as u64;
            count -= chunk;
        }

        // Without this the data may sit in the drive's cache, and a reset or
        // power loss would lose it.
        self.command(ATA_FLUSH_CACHE_EXT, 0, 0, true)
    }

    /// Issue a single READ DMA EXT for up to one buffer's worth of sectors.
    fn read_chunk(&self, lba: u64, count: usize) -> Result<(), &'static str> {
        self.command(ATA_READ_DMA_EXT, lba, count, false)
    }

    /// Build and issue one ATA command in slot 0, then wait for it.
    ///
    /// `write` sets the direction bit in the command header, which is what
    /// tells the controller to read from memory rather than write to it.
    fn command(
        &self,
        opcode: u8,
        lba: u64,
        count: usize,
        write: bool,
    ) -> Result<(), &'static str> {
        // Clear stale error and interrupt state, or a previous failure looks
        // like this one's.
        self.write_port(PORT_SERR, self.read_port(PORT_SERR));
        self.write_port(PORT_IS, self.read_port(PORT_IS));

        self.wait_not_busy()?;

        unsafe {
            // Command header, slot 0.
            let header = phys_to_virt(self.command_list);
            core::ptr::write_bytes(header, 0, 32);

            // Command FIS length in dwords (5), the transfer direction, and
            // one PRDT entry — unless there is no data at all, as for a cache
            // flush.
            let prdt_entries: u16 = if count == 0 { 0 } else { 1 };
            let flags: u16 = 5 | if write { 1 << 6 } else { 0 };
            header.cast::<u16>().write_volatile(flags);
            header.add(2).cast::<u16>().write_volatile(prdt_entries);
            // Byte count transferred, cleared before each command.
            header.add(4).cast::<u32>().write_volatile(0);
            header.add(8).cast::<u64>().write_volatile(self.command_table);

            // Command table: the FIS, then the scatter-gather list.
            let table = phys_to_virt(self.command_table);
            core::ptr::write_bytes(table, 0, 128);

            let fis = table;
            fis.write_volatile(0x27); // host-to-device register FIS
            fis.add(1).write_volatile(1 << 7); // this FIS carries a command
            fis.add(2).write_volatile(opcode);

            fis.add(4).write_volatile(lba as u8);
            fis.add(5).write_volatile((lba >> 8) as u8);
            fis.add(6).write_volatile((lba >> 16) as u8);
            fis.add(7).write_volatile(1 << 6); // LBA mode
            fis.add(8).write_volatile((lba >> 24) as u8);
            fis.add(9).write_volatile((lba >> 32) as u8);
            fis.add(10).write_volatile((lba >> 40) as u8);

            fis.add(12).write_volatile(count as u8);
            fis.add(13).write_volatile((count >> 8) as u8);

            // One PRDT entry at offset 0x80, describing the whole buffer. The
            // byte count is stored biased by one.
            if count > 0 {
                let prdt = table.add(0x80);
                prdt.cast::<u64>().write_volatile(self.buffer);
                prdt.add(12)
                    .cast::<u32>()
                    .write_volatile((count * SECTOR_SIZE - 1) as u32);
            }
        }

        // Issue the command in slot 0 and wait for the controller to clear it.
        self.write_port(PORT_CI, 1);

        for _ in 0..50_000_000u64 {
            if self.read_port(PORT_CI) & 1 == 0 {
                break;
            }
            if self.read_port(PORT_IS) & IS_TFES != 0 {
                return Err("task file error during read");
            }
            core::hint::spin_loop();
        }

        if self.read_port(PORT_CI) & 1 != 0 {
            return Err("timed out waiting for the disk");
        }
        if self.read_port(PORT_TFD) & TFD_ERR != 0 {
            return Err("disk reported an error");
        }

        Ok(())
    }

    fn wait_not_busy(&self) -> Result<(), &'static str> {
        for _ in 0..50_000_000u64 {
            if self.read_port(PORT_TFD) & (TFD_BSY | TFD_DRQ) == 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err("disk stayed busy")
    }
}

pub static DISK: Mutex<Option<Ahci>> = Mutex::new(None);

/// Find an AHCI controller, map it, and bring up the first port with a disk.
pub fn init() -> Result<(u16, u16), &'static str> {
    // Class 1 (mass storage), subclass 6 (SATA), interface 1 (AHCI).
    let device = pci::find(0x01, 0x06, 0x01).ok_or("no AHCI controller found")?;
    device.enable_memory_and_bus_master();

    let abar_phys = device.bar(5);
    if abar_phys == 0 {
        return Err("AHCI controller has no ABAR");
    }

    // Controller registers are device memory, which the direct map does not
    // cover, so map them before the first access.
    memory::map_mmio(abar_phys, 0x2000).map_err(|_| "could not map the AHCI registers")?;
    let abar = phys_to_virt(abar_phys);

    let mut controller = Ahci {
        abar,
        port: 0,
        command_list: 0,
        fis_area: 0,
        command_table: 0,
        buffer: 0,
    };

    // Take ownership of the controller from the firmware.
    let ghc = controller.read_register(HBA_GHC);
    controller.write_register(HBA_GHC, ghc | GHC_AHCI_ENABLE);

    let implemented = controller.read_register(HBA_PI);
    let port = (0..32)
        .find(|port| {
            if implemented & (1 << port) == 0 {
                return false;
            }
            controller.port = *port as usize;

            // DET 3 means a device is present and communicating; the
            // signature distinguishes a disk from an optical drive.
            let detected = controller.read_port(PORT_SSTS) & 0x0F == 3;
            detected && controller.read_port(PORT_SIG) == SIGNATURE_SATA
        })
        .ok_or("no SATA disk attached")?;

    controller.port = port as usize;

    // Each structure gets its own frame: frames are 4 KiB aligned, which
    // satisfies AHCI's 1 KiB command list and 256 byte FIS alignment rules.
    controller.command_list = frame()?;
    controller.fis_area = frame()?;
    controller.command_table = frame()?;
    controller.buffer = frame()?;

    controller.stop();

    controller.write_port(PORT_CLB, controller.command_list as u32);
    controller.write_port(PORT_CLBU, (controller.command_list >> 32) as u32);
    controller.write_port(PORT_FB, controller.fis_area as u32);
    controller.write_port(PORT_FBU, (controller.fis_area >> 32) as u32);

    controller.write_port(PORT_SERR, controller.read_port(PORT_SERR));
    controller.write_port(PORT_IS, controller.read_port(PORT_IS));

    controller.start();

    let ids = (device.vendor_id(), device.device_id());
    *DISK.lock() = Some(controller);
    Ok(ids)
}

fn frame() -> Result<u64, &'static str> {
    memory::allocate_zeroed_frame()
        .map(|frame| frame.start_address().as_u64())
        .ok_or("out of memory for AHCI structures")
}

/// Read sectors from the boot disk.
pub fn read(lba: u64, count: usize, out: &mut [u8]) -> Result<(), &'static str> {
    x86_64::instructions::interrupts::without_interrupts(|| match DISK.lock().as_ref() {
        Some(disk) => disk.read(lba, count, out),
        None => Err("no disk"),
    })
}

/// Write sectors to the boot disk.
pub fn write(lba: u64, count: usize, data: &[u8]) -> Result<(), &'static str> {
    x86_64::instructions::interrupts::without_interrupts(|| match DISK.lock().as_ref() {
        Some(disk) => disk.write(lba, count, data),
        None => Err("no disk"),
    })
}
