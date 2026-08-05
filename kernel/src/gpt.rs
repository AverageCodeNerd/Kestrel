//! Just enough GPT to find the EFI system partition on the boot disk.

use alloc::vec;

use crate::ahci::{self, SECTOR_SIZE};

/// {C12A7328-F81F-11D2-BA4B-00A0C93EC93B}, stored in GPT's mixed-endian form:
/// the first three fields little-endian, the last two as plain bytes.
const ESP_TYPE_GUID: [u8; 16] = [
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
];

pub struct Partition {
    pub first_lba: u64,
    pub last_lba: u64,
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn read_u64(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

/// Locate the EFI system partition, which is where the bootloader and kernel
/// live and therefore the one filesystem we know exists.
pub fn find_esp() -> Result<Partition, &'static str> {
    let mut header = [0u8; SECTOR_SIZE];
    ahci::read(1, 1, &mut header)?;

    if &header[0..8] != b"EFI PART" {
        return Err("no GPT header on the boot disk");
    }

    let entries_lba = read_u64(&header, 72);
    let entry_count = read_u32(&header, 80) as usize;
    let entry_size = read_u32(&header, 84) as usize;

    if entry_size < 128 || entry_count == 0 {
        return Err("malformed GPT partition table");
    }

    // The entry array is usually 32 sectors; read it in one go.
    let bytes = entry_count * entry_size;
    let sectors = bytes.div_ceil(SECTOR_SIZE);
    let mut table = vec![0u8; sectors * SECTOR_SIZE];
    ahci::read(entries_lba, sectors, &mut table)?;

    for index in 0..entry_count {
        let entry = index * entry_size;
        if table[entry..entry + 16] != ESP_TYPE_GUID {
            continue;
        }

        return Ok(Partition {
            first_lba: read_u64(&table, entry + 32),
            last_lba: read_u64(&table, entry + 40),
        });
    }

    Err("no EFI system partition found")
}
