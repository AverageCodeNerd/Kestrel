//! A minimal ISO 9660 image with an El Torito EFI boot entry.
//!
//! Two filesystems are involved, which is easy to get wrong:
//!
//! * The **El Torito boot image** is a FAT16 volume embedded in the ISO. UEFI
//!   firmware loads `EFI/BOOT/BOOTX64.EFI` from it. That is all it is for.
//! * The **ISO 9660 filesystem** is what Limine itself then reads, scanning
//!   for `limine.conf` and loading the kernel named in it.
//!
//! Putting the config only in the FAT image is not enough: Limine cannot match
//! the El Torito boot handle to a volume and falls back to scanning, so the
//! config and kernel must be reachable through ISO 9660 as well.
//!
//! ISO 9660 filenames are 8.3, uppercase, and version-suffixed â€” `LIMINE.CON;1`
//! rather than `limine.conf`. Rock Ridge `NM` entries in each record's
//! system-use area carry the real name, which Limine's driver reads.

use std::io;
use std::path::Path;

use crate::crc::crc32;
use crate::fat::{Dir, Fat32, FatKind, SECTOR};

/// ISO 9660 logical sector size, unrelated to the 512-byte disk sector.
const ISO_SECTOR: usize = 2048;

const PRIMARY_DESCRIPTOR_LBA: u32 = 16;
const BOOT_DESCRIPTOR_LBA: u32 = 17;
const TERMINATOR_LBA: u32 = 18;
const BOOT_CATALOG_LBA: u32 = 19;
const PATH_TABLE_L_LBA: u32 = 20;
const PATH_TABLE_M_LBA: u32 = 21;
const ROOT_DIRECTORY_LBA: u32 = 22;
const BOOT_DIRECTORY_LBA: u32 = 23;
const REPO_DIRECTORY_LBA: u32 = 24;
/// File extents begin here; the boot image follows them.
const FIRST_FILE_LBA: u32 = 25;

/// Size of the embedded FAT16 volume. Must stay under 32 MiB so its size in
/// 512-byte sectors fits the boot catalog's 16-bit field.
const BOOT_IMAGE_MEGABYTES: u32 = 16;

const FLAG_DIRECTORY: u8 = 0x02;

/// Build `iso` from the contents of `esp_dir`. Returns the image size.
pub fn build(esp_dir: &Path, iso: &Path) -> io::Result<u64> {
    let tree = Dir::from_host(esp_dir)?;

    let boot_sectors = BOOT_IMAGE_MEGABYTES * 1024 * 1024 / SECTOR as u32;
    if boot_sectors > u16::MAX as u32 {
        return Err(io::Error::other(
            "boot image too large for the El Torito sector count field",
        ));
    }

    let mut fs = Fat32::format(boot_sectors, "KESTREL", FatKind::Fat16);
    fs.write_tree(&tree);
    let boot_image = fs.into_bytes();

    // Everything Limine has to find through ISO 9660: its config, the kernel,
    // and every user program it is asked to load as a module. Leaving the
    // modules out here would boot but leave /repo empty.
    let config = std::fs::read(esp_dir.join("EFI/BOOT/limine.conf"))?;
    let kernel = std::fs::read(esp_dir.join("boot/kestrel"))?;

    let mut programs: Vec<(String, Vec<u8>)> = Vec::new();
    let bin = esp_dir.join("repo");
    if bin.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(&bin)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().to_string();
            programs.push((name, std::fs::read(entry.path())?));
        }
    }

    let config_lba = FIRST_FILE_LBA;
    let config_sectors = config.len().div_ceil(ISO_SECTOR) as u32;
    let kernel_lba = config_lba + config_sectors;
    let kernel_sectors = kernel.len().div_ceil(ISO_SECTOR) as u32;

    // Lay the programs out after the kernel.
    let mut program_lbas = Vec::new();
    let mut next_lba = kernel_lba + kernel_sectors;
    for (_, data) in &programs {
        program_lbas.push(next_lba);
        next_lba += data.len().div_ceil(ISO_SECTOR).max(1) as u32;
    }

    let boot_image_lba = next_lba;
    let boot_image_sectors = boot_image.len().div_ceil(ISO_SECTOR) as u32;
    let total_sectors = boot_image_lba + boot_image_sectors;

    let mut image = vec![0u8; total_sectors as usize * ISO_SECTOR];

    write_primary_descriptor(&mut image, total_sectors);
    write_boot_descriptor(&mut image);
    write_terminator(&mut image);
    write_boot_catalog(&mut image, boot_image_lba, boot_image.len());
    write_path_tables(&mut image);

    // Root: `.`, `..`, the two subdirectories, and limine.conf.
    {
        let sector = sector_mut(&mut image, ROOT_DIRECTORY_LBA);
        let mut offset = 0;
        offset += put(sector, offset, &record(ROOT_DIRECTORY_LBA, ISO_SECTOR as u32, FLAG_DIRECTORY, Name::Dot));
        offset += put(sector, offset, &record(ROOT_DIRECTORY_LBA, ISO_SECTOR as u32, FLAG_DIRECTORY, Name::DotDot));
        offset += put(
            sector,
            offset,
            &record(BOOT_DIRECTORY_LBA, ISO_SECTOR as u32, FLAG_DIRECTORY, Name::Named("BOOT", "boot")),
        );
        offset += put(
            sector,
            offset,
            &record(REPO_DIRECTORY_LBA, ISO_SECTOR as u32, FLAG_DIRECTORY, Name::Named("REPO", "repo")),
        );
        put(
            sector,
            offset,
            &record(config_lba, config.len() as u32, 0, Name::Named("LIMINE.CON;1", "limine.conf")),
        );
    }

    // /boot: `.`, `..`, and the kernel.
    {
        let sector = sector_mut(&mut image, BOOT_DIRECTORY_LBA);
        let mut offset = 0;
        offset += put(sector, offset, &record(BOOT_DIRECTORY_LBA, ISO_SECTOR as u32, FLAG_DIRECTORY, Name::Dot));
        offset += put(sector, offset, &record(ROOT_DIRECTORY_LBA, ISO_SECTOR as u32, FLAG_DIRECTORY, Name::DotDot));
        put(
            sector,
            offset,
            &record(kernel_lba, kernel.len() as u32, 0, Name::Named("KESTREL;1", "kestrel")),
        );
    }

    // /repo: `.`, `..`, and every package.
    {
        let mut entries: Vec<u8> = Vec::new();
        entries.extend_from_slice(&record(REPO_DIRECTORY_LBA, ISO_SECTOR as u32, FLAG_DIRECTORY, Name::Dot));
        entries.extend_from_slice(&record(ROOT_DIRECTORY_LBA, ISO_SECTOR as u32, FLAG_DIRECTORY, Name::DotDot));

        for (index, (name, data)) in programs.iter().enumerate() {
            // Program names are short and lowercase, so the ISO identifier is
            // the uppercased name with the mandatory version suffix, and the
            // Rock Ridge entry carries the real one.
            let iso_name = format!("{};1", name.to_uppercase());
            entries.extend_from_slice(&record(
                program_lbas[index],
                data.len() as u32,
                0,
                Name::Named(&iso_name, name),
            ));
        }

        assert!(
            entries.len() <= ISO_SECTOR,
            "too many programs for a one-sector directory"
        );
        let sector = sector_mut(&mut image, REPO_DIRECTORY_LBA);
        sector[..entries.len()].copy_from_slice(&entries);
    }

    write_at(&mut image, config_lba, &config);
    write_at(&mut image, kernel_lba, &kernel);
    for (index, (_, data)) in programs.iter().enumerate() {
        write_at(&mut image, program_lbas[index], data);
    }
    write_at(&mut image, boot_image_lba, &boot_image);

    // Room for the backup GPT, which by definition lives in the last sectors.
    // ISO 9660 readers ignore anything past the volume size in the descriptor,
    // so this is invisible to them.
    //
    // Rounded up to a whole 2048-byte sector: VirtualBox refuses to attach an
    // ISO whose length is not a multiple of the ISO sector size, reporting only
    // "Could not get the storage format of the medium (VERR_NOT_SUPPORTED)".
    let with_tail = image.len() + GPT_TAIL_SECTORS as usize * SECTOR;
    image.resize(with_tail.next_multiple_of(ISO_SECTOR), 0);
    write_hybrid_gpt(&mut image, boot_image_lba, boot_image_sectors);

    std::fs::write(iso, &image)?;
    Ok(image.len() as u64)
}

/// 512-byte sectors reserved at the end for the backup GPT: one header plus a
/// 128-entry array.
const GPT_TAIL_SECTORS: u64 = 33;

/// EFI System Partition type GUID, and fixed identifiers so the image is
/// reproducible.
const ESP_TYPE_GUID: [u8; 16] = [
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
];
const DISK_GUID: [u8; 16] = *b"KestrelIsoGuid00";
const PARTITION_GUID: [u8; 16] = *b"KestrelIsoEsp001";

/// Expose the embedded El Torito boot image as an EFI system partition.
///
/// Without this the ISO boots, but Limine stops with "Could not meaningfully
/// match the boot device handle with a volume... Press any key" â€” on some
/// firmware, VirtualBox's included, the El Torito boot handle cannot be
/// correlated with any volume Limine knows about, and it waits for a keypress
/// before falling back. Describing the same bytes as a partition as well gives
/// the firmware a normal ESP to boot from and a handle that does match. This is
/// what `xorriso`'s `-efi-boot-part --protective-msdos-label` produces, and why
/// the standard Limine ISO recipe passes them.
///
/// The partition table lives in the ISO 9660 *system area* â€” the first 32 KiB,
/// which the format reserves and never uses.
fn write_hybrid_gpt(image: &mut [u8], boot_image_lba: u32, boot_image_sectors: u32) {
    let total_sectors = (image.len() / SECTOR) as u64;
    let last_lba = total_sectors - 1;

    // ISO sectors are 2048 bytes and GPT counts 512-byte ones.
    let scale = (ISO_SECTOR / SECTOR) as u64;
    let partition_start = boot_image_lba as u64 * scale;
    let partition_end = partition_start + boot_image_sectors as u64 * scale - 1;

    write_protective_mbr(image, total_sectors);

    let mut entries = vec![0u8; 128 * 128];
    entries[0..16].copy_from_slice(&ESP_TYPE_GUID);
    entries[16..32].copy_from_slice(&PARTITION_GUID);
    entries[32..40].copy_from_slice(&partition_start.to_le_bytes());
    entries[40..48].copy_from_slice(&partition_end.to_le_bytes());
    for (index, unit) in "EFI System Partition".encode_utf16().enumerate() {
        let offset = 56 + index * 2;
        entries[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
    }

    let entries_crc = crc32(&entries);
    let primary_entries_lba = 2u64;
    let backup_entries_lba = last_lba - 32;

    let primary_start = primary_entries_lba as usize * SECTOR;
    image[primary_start..primary_start + entries.len()].copy_from_slice(&entries);
    let backup_start = backup_entries_lba as usize * SECTOR;
    image[backup_start..backup_start + entries.len()].copy_from_slice(&entries);

    // The usable range must contain the partition: the boot image sits in the
    // middle of the ISO, so first-usable is right after the primary array.
    let first_usable = primary_entries_lba + 32;
    let last_usable = last_lba - GPT_TAIL_SECTORS;

    let primary = gpt_header(1, last_lba, first_usable, last_usable, primary_entries_lba, entries_crc);
    image[SECTOR..SECTOR + 92].copy_from_slice(&primary);

    let backup = gpt_header(last_lba, 1, first_usable, last_usable, backup_entries_lba, entries_crc);
    let backup_header_start = last_lba as usize * SECTOR;
    image[backup_header_start..backup_header_start + 92].copy_from_slice(&backup);
}

fn write_protective_mbr(image: &mut [u8], total_sectors: u64) {
    let entry = &mut image[446..462];
    entry[0] = 0x00;
    entry[1..4].copy_from_slice(&[0x00, 0x02, 0x00]);
    entry[4] = 0xEE;
    entry[5..8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    entry[8..12].copy_from_slice(&1u32.to_le_bytes());
    entry[12..16].copy_from_slice(&(total_sectors - 1).min(u32::MAX as u64).to_le_bytes()[0..4]);

    image[510] = 0x55;
    image[511] = 0xAA;
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
    header[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
    header[12..16].copy_from_slice(&92u32.to_le_bytes());
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

fn sector_mut(image: &mut [u8], lba: u32) -> &mut [u8] {
    let start = lba as usize * ISO_SECTOR;
    &mut image[start..start + ISO_SECTOR]
}

fn write_at(image: &mut [u8], lba: u32, data: &[u8]) {
    let start = lba as usize * ISO_SECTOR;
    image[start..start + data.len()].copy_from_slice(data);
}

fn put(sector: &mut [u8], offset: usize, record: &[u8]) -> usize {
    sector[offset..offset + record.len()].copy_from_slice(record);
    record.len()
}

/// ISO 9660 stores most numbers twice, little-endian then big-endian.
fn both_endian_u32(out: &mut [u8], value: u32) {
    out[0..4].copy_from_slice(&value.to_le_bytes());
    out[4..8].copy_from_slice(&value.to_be_bytes());
}

fn both_endian_u16(out: &mut [u8], value: u16) {
    out[0..2].copy_from_slice(&value.to_le_bytes());
    out[2..4].copy_from_slice(&value.to_be_bytes());
}

enum Name<'a> {
    Dot,
    DotDot,
    /// The 8.3 ISO identifier, and the real name for the Rock Ridge entry.
    Named(&'a str, &'a str),
}

/// Build a directory record, with a Rock Ridge `NM` entry for real names.
fn record(lba: u32, length: u32, flags: u8, name: Name) -> Vec<u8> {
    let (identifier, long_name): (&[u8], Option<&str>) = match name {
        Name::Dot => (&[0u8], None),
        Name::DotDot => (&[1u8], None),
        Name::Named(iso, real) => (iso.as_bytes(), Some(real)),
    };

    let mut out = vec![0u8; 33];
    out[1] = 0; // extended attribute length
    both_endian_u32(&mut out[2..10], lba);
    both_endian_u32(&mut out[10..18], length);

    // Recording date: years since 1900, month, day, hour, minute, second,
    // then offset from GMT in 15-minute intervals.
    out[18] = 126; // 2026
    out[19] = 1;
    out[20] = 1;

    out[25] = flags;
    both_endian_u16(&mut out[28..32], 1); // volume sequence number
    out[32] = identifier.len() as u8;
    out.extend_from_slice(identifier);

    // A record's identifier is padded so the total stays even; the header is
    // 33 bytes, so an even-length identifier needs one byte of padding.
    if identifier.len() % 2 == 0 {
        out.push(0);
    }

    if let Some(real) = long_name {
        // Rock Ridge NM: signature, length, version, flags, then the name.
        out.extend_from_slice(b"NM");
        out.push((5 + real.len()) as u8);
        out.push(1); // entry version
        out.push(0); // flags: name is complete, no continuation
        out.extend_from_slice(real.as_bytes());
    }

    // Directory records must themselves be an even number of bytes. A single
    // trailing byte is ignored by the system-use area parser.
    if out.len() % 2 == 1 {
        out.push(0);
    }

    let length = out.len() as u8;
    out[0] = length;
    out
}

fn write_primary_descriptor(image: &mut [u8], total_sectors: u32) {
    let sector = sector_mut(image, PRIMARY_DESCRIPTOR_LBA);

    sector[0] = 1; // primary volume descriptor
    sector[1..6].copy_from_slice(b"CD001");
    sector[6] = 1; // version

    sector[8..40].fill(b' '); // system identifier
    sector[40..72].fill(b' '); // volume identifier
    sector[40..47].copy_from_slice(b"KESTREL");

    both_endian_u32(&mut sector[80..88], total_sectors);
    both_endian_u16(&mut sector[120..124], 1); // volume set size
    both_endian_u16(&mut sector[124..128], 1); // volume sequence number
    both_endian_u16(&mut sector[128..132], ISO_SECTOR as u16);

    both_endian_u32(&mut sector[132..140], PATH_TABLE_SIZE);
    sector[140..144].copy_from_slice(&PATH_TABLE_L_LBA.to_le_bytes());
    sector[148..152].copy_from_slice(&PATH_TABLE_M_LBA.to_be_bytes());

    // The root directory record is embedded directly in the descriptor, and
    // is always the fixed 34-byte form.
    let root = record(ROOT_DIRECTORY_LBA, ISO_SECTOR as u32, FLAG_DIRECTORY, Name::Dot);
    sector[156..156 + root.len()].copy_from_slice(&root);

    sector[190..318].fill(b' '); // volume set identifier
    sector[318..446].fill(b' '); // publisher
    sector[446..574].fill(b' '); // data preparer
    sector[574..702].fill(b' '); // application
    sector[702..739].fill(b' '); // copyright file
    sector[739..776].fill(b' '); // abstract file
    sector[776..813].fill(b' '); // bibliographic file

    for range in [813..830, 830..847, 847..864, 864..881] {
        sector[range].copy_from_slice(b"0000000000000000\0");
    }

    sector[881] = 1; // file structure version
}

/// Points the firmware at the boot catalog.
fn write_boot_descriptor(image: &mut [u8]) {
    let sector = sector_mut(image, BOOT_DESCRIPTOR_LBA);

    sector[0] = 0; // boot record
    sector[1..6].copy_from_slice(b"CD001");
    sector[6] = 1;
    sector[7..7 + 23].copy_from_slice(b"EL TORITO SPECIFICATION");
    sector[71..75].copy_from_slice(&BOOT_CATALOG_LBA.to_le_bytes());
}

fn write_terminator(image: &mut [u8]) {
    let sector = sector_mut(image, TERMINATOR_LBA);
    sector[0] = 0xFF;
    sector[1..6].copy_from_slice(b"CD001");
    sector[6] = 1;
}

/// The catalog is what actually makes this bootable: a validation entry
/// declaring the EFI platform, followed by an entry naming the boot image.
fn write_boot_catalog(image: &mut [u8], boot_image_lba: u32, boot_image_bytes: usize) {
    let sector = sector_mut(image, BOOT_CATALOG_LBA);

    // Validation entry.
    sector[0] = 1; // header id
    sector[1] = 0xEF; // platform: UEFI
    sector[30] = 0x55;
    sector[31] = 0xAA;

    // The 16-bit words of this entry must sum to zero.
    let mut sum: u16 = 0;
    for i in (0..32).step_by(2) {
        sum = sum.wrapping_add(u16::from_le_bytes([sector[i], sector[i + 1]]));
    }
    let checksum = (!sum).wrapping_add(1);
    sector[28..30].copy_from_slice(&checksum.to_le_bytes());

    // Default entry, describing the FAT volume to hand to the firmware.
    let entry = &mut sector[32..64];
    entry[0] = 0x88; // bootable
    entry[1] = 0; // no emulation: treat the image as a raw volume
    entry[2..4].copy_from_slice(&0u16.to_le_bytes()); // load segment (unused)
    entry[4] = 0; // system type
    let virtual_sectors = (boot_image_bytes / 512) as u16;
    entry[6..8].copy_from_slice(&virtual_sectors.to_le_bytes());
    entry[8..12].copy_from_slice(&boot_image_lba.to_le_bytes());
}

/// Root (identifier `\0`, padded to even), plus `BOOT` and `REPO`.
const PATH_TABLE_SIZE: u32 = 10 + 12 + 12;

/// Path tables list every directory. Type L is little-endian, type M big.
fn write_path_tables(image: &mut [u8]) {
    for (lba, big_endian) in [(PATH_TABLE_L_LBA, false), (PATH_TABLE_M_LBA, true)] {
        let sector = sector_mut(image, lba);

        let extent = |value: u32| -> [u8; 4] {
            if big_endian { value.to_be_bytes() } else { value.to_le_bytes() }
        };
        let parent = |value: u16| -> [u8; 2] {
            if big_endian { value.to_be_bytes() } else { value.to_le_bytes() }
        };

        // Directory 1: the root, whose name is a single zero byte.
        sector[0] = 1; // identifier length
        sector[1] = 0; // extended attribute length
        sector[2..6].copy_from_slice(&extent(ROOT_DIRECTORY_LBA));
        sector[6..8].copy_from_slice(&parent(1)); // the root is its own parent
        sector[8] = 0;
        // byte 9 is padding, keeping the record even

        // Directory 2: /BOOT.
        sector[10] = 4;
        sector[11] = 0;
        sector[12..16].copy_from_slice(&extent(BOOT_DIRECTORY_LBA));
        sector[16..18].copy_from_slice(&parent(1));
        sector[18..22].copy_from_slice(b"BOOT");

        // Directory 3: /REPO. Four characters, so the record is already even
        // and needs no padding byte after it.
        sector[22] = 4;
        sector[23] = 0;
        sector[24..28].copy_from_slice(&extent(REPO_DIRECTORY_LBA));
        sector[28..30].copy_from_slice(&parent(1));
        sector[30..34].copy_from_slice(b"REPO");
    }
}

