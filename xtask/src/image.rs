//! Builds a bootable disk image: GPT partition table, one FAT32 EFI system
//! partition, and optionally a fixed-format VHD wrapper.
//!
//! QEMU's `fat:rw:` directory trick is convenient but only QEMU understands
//! it. VirtualBox, Hyper-V, and VMware all want a real image.

use std::io;
use std::path::Path;

use crate::crc::crc32;
use crate::fat::{Dir, Fat32, FatKind, SECTOR};

/// Where the partition starts. 1 MiB in, which is the modern alignment
/// convention and leaves plenty of room for the GPT.
const PARTITION_START_LBA: u64 = 2048;

/// GPT reserves 33 sectors at each end: one header plus 32 of entry array.
const GPT_SECTORS: u64 = 33;

const ESP_TYPE_GUID: [u8; 16] = [
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
];

/// Fixed identifiers. Real GUIDs would be random, but a reproducible image is
/// more useful than a unique one here.
const DISK_GUID: [u8; 16] = *b"KestrelDiskGuid0";
const PARTITION_GUID: [u8; 16] = *b"KestrelEspGuid01";

/// Build `image` from the contents of `esp_dir`.
///
/// Returns the total image size in bytes.
pub fn build(esp_dir: &Path, image: &Path, megabytes: u64) -> io::Result<u64> {
    let tree = Dir::from_host(esp_dir)?;

    let total_sectors = megabytes * 1024 * 1024 / SECTOR as u64;
    let partition_sectors = total_sectors - PARTITION_START_LBA - GPT_SECTORS;

    let needed = tree.total_bytes() as u64;
    let capacity = partition_sectors * SECTOR as u64;
    if needed + 1024 * 1024 > capacity {
        return Err(io::Error::other(format!(
            "contents are {needed} bytes but the partition holds only {capacity}"
        )));
    }

    // Format the partition and lay the files into it.
    // UEFI requires an EFI system partition on a hard disk to be FAT32.
    let mut fs = Fat32::format(partition_sectors as u32, "KESTREL", FatKind::Fat32);
    fs.write_tree(&tree);
    let partition = fs.into_bytes();

    let mut disk = vec![0u8; total_sectors as usize * SECTOR];
    let start = PARTITION_START_LBA as usize * SECTOR;
    disk[start..start + partition.len()].copy_from_slice(&partition);

    write_protective_mbr(&mut disk, total_sectors);
    write_gpt(&mut disk, total_sectors, partition_sectors);

    std::fs::write(image, &disk)?;
    Ok(disk.len() as u64)
}

/// A single 0xEE partition spanning the disk, so tools that only understand
/// MBR see the disk as fully used rather than as empty.
fn write_protective_mbr(disk: &mut [u8], total_sectors: u64) {
    let entry = &mut disk[446..462];
    entry[0] = 0x00; // not bootable
    entry[1..4].copy_from_slice(&[0x00, 0x02, 0x00]); // CHS start, cosmetic
    entry[4] = 0xEE; // GPT protective
    entry[5..8].copy_from_slice(&[0xFF, 0xFF, 0xFF]); // CHS end, cosmetic
    entry[8..12].copy_from_slice(&1u32.to_le_bytes());
    entry[12..16].copy_from_slice(&(total_sectors - 1).min(u32::MAX as u64).to_le_bytes()[0..4]);

    disk[510] = 0x55;
    disk[511] = 0xAA;
}

fn write_gpt(disk: &mut [u8], total_sectors: u64, partition_sectors: u64) {
    let last_lba = total_sectors - 1;
    let first_usable = 2 + 32;
    let last_usable = last_lba - GPT_SECTORS;

    // 128 entries of 128 bytes, of which we use one.
    let mut entries = vec![0u8; 128 * 128];
    let partition_end = PARTITION_START_LBA + partition_sectors - 1;

    entries[0..16].copy_from_slice(&ESP_TYPE_GUID);
    entries[16..32].copy_from_slice(&PARTITION_GUID);
    entries[32..40].copy_from_slice(&PARTITION_START_LBA.to_le_bytes());
    entries[40..48].copy_from_slice(&partition_end.to_le_bytes());
    entries[48..56].copy_from_slice(&0u64.to_le_bytes()); // attributes
    for (i, unit) in "EFI System Partition".encode_utf16().enumerate() {
        let offset = 56 + i * 2;
        entries[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
    }

    let entries_crc = crc32(&entries);

    // Primary array at LBA 2, backup array just below the backup header.
    let primary_entries_lba = 2u64;
    let backup_entries_lba = last_lba - 32;

    let primary_start = primary_entries_lba as usize * SECTOR;
    disk[primary_start..primary_start + entries.len()].copy_from_slice(&entries);

    let backup_start = backup_entries_lba as usize * SECTOR;
    disk[backup_start..backup_start + entries.len()].copy_from_slice(&entries);

    let primary = gpt_header(
        1,
        last_lba,
        first_usable,
        last_usable,
        primary_entries_lba,
        entries_crc,
    );
    disk[SECTOR..SECTOR + 92].copy_from_slice(&primary);

    // The backup header swaps "my LBA" and "alternate LBA".
    let backup = gpt_header(
        last_lba,
        1,
        first_usable,
        last_usable,
        backup_entries_lba,
        entries_crc,
    );
    let backup_header_start = last_lba as usize * SECTOR;
    disk[backup_header_start..backup_header_start + 92].copy_from_slice(&backup);
}

fn gpt_header(
    my_lba: u64,
    alternate_lba: u64,
    first_usable: u64,
    last_usable: u64,
    entries_lba: u64,
    entries_crc: u32,
) -> [u8; 92] {
    let mut header = [0u8; 92];

    header[0..8].copy_from_slice(b"EFI PART");
    header[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes()); // revision 1.0
    header[12..16].copy_from_slice(&92u32.to_le_bytes());
    // 16..20 is the header CRC, left zero while it is computed.
    header[24..32].copy_from_slice(&my_lba.to_le_bytes());
    header[32..40].copy_from_slice(&alternate_lba.to_le_bytes());
    header[40..48].copy_from_slice(&first_usable.to_le_bytes());
    header[48..56].copy_from_slice(&last_usable.to_le_bytes());
    header[56..72].copy_from_slice(&DISK_GUID);
    header[72..80].copy_from_slice(&entries_lba.to_le_bytes());
    header[80..84].copy_from_slice(&128u32.to_le_bytes());
    header[84..88].copy_from_slice(&128u32.to_le_bytes());
    header[88..92].copy_from_slice(&entries_crc.to_le_bytes());

    let checksum = crc32(&header);
    header[16..20].copy_from_slice(&checksum.to_le_bytes());

    header
}

/// Append a fixed-disk VHD footer, which is all that separates a raw image
/// from something Hyper-V and VirtualBox will mount.
pub fn write_vhd(raw: &Path, vhd: &Path) -> io::Result<()> {
    let mut data = std::fs::read(raw)?;
    let size = data.len() as u64;

    let mut footer = [0u8; 512];
    footer[0..8].copy_from_slice(b"conectix");
    footer[8..12].copy_from_slice(&2u32.to_be_bytes()); // temporary disk = no
    footer[12..16].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // format 1.0
    footer[16..24].copy_from_slice(&u64::MAX.to_be_bytes()); // no dynamic header
    footer[24..28].copy_from_slice(&0u32.to_be_bytes()); // creation time
    footer[28..32].copy_from_slice(b"kstl");
    footer[32..36].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    footer[36..40].copy_from_slice(b"Wi2k");
    footer[40..48].copy_from_slice(&size.to_be_bytes()); // original size
    footer[48..56].copy_from_slice(&size.to_be_bytes()); // current size

    // The geometry is legacy CHS and only has to be self-consistent.
    let (cylinders, heads, sectors) = geometry(size / 512);
    footer[56..58].copy_from_slice(&cylinders.to_be_bytes());
    footer[58] = heads;
    footer[59] = sectors;
    footer[60..64].copy_from_slice(&2u32.to_be_bytes()); // fixed disk
    // 64..68 is the checksum, computed below.
    footer[68..84].copy_from_slice(b"KestrelVhdUuid01");

    let sum: u32 = footer.iter().map(|&b| b as u32).sum();
    footer[64..68].copy_from_slice(&(!sum).to_be_bytes());

    data.extend_from_slice(&footer);
    std::fs::write(vhd, &data)
}

/// The CHS translation given in the VHD specification, transcribed directly.
fn geometry(total_sectors: u64) -> (u16, u8, u8) {
    let total = total_sectors.min(65535 * 16 * 255);

    let mut sectors_per_track;
    let mut heads;
    let mut cylinder_times_heads;

    if total >= 65535 * 16 * 63 {
        sectors_per_track = 255;
        heads = 16;
        cylinder_times_heads = total / sectors_per_track as u64;
    } else {
        sectors_per_track = 17;
        cylinder_times_heads = total / sectors_per_track as u64;

        heads = ((cylinder_times_heads + 1023) / 1024).max(4) as u8;

        if cylinder_times_heads >= (heads as u64) * 1024 || heads > 16 {
            sectors_per_track = 31;
            heads = 16;
            cylinder_times_heads = total / sectors_per_track as u64;
        }

        if cylinder_times_heads >= (heads as u64) * 1024 {
            sectors_per_track = 63;
            heads = 16;
            cylinder_times_heads = total / sectors_per_track as u64;
        }
    }

    let cylinders = (cylinder_times_heads / heads as u64) as u16;
    (cylinders, heads, sectors_per_track)
}
