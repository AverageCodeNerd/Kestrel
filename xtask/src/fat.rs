//! A minimal FAT32 formatter.
//!
//! UEFI mandates FAT for the EFI system partition, and firmware is much
//! happier with FAT32 than with the smaller variants. Long filenames matter
//! here for one specific reason: the bootloader insists on finding a file
//! called `limine.conf`, and a four-character extension does not fit in an
//! 8.3 short name.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

pub const SECTOR: usize = 512;

const ATTR_READ_ONLY: u8 = 0x01;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_LONG_NAME: u8 = 0x0F;

/// End-of-chain marker.
const EOC: u32 = 0x0FFF_FFFF;

/// A directory being staged before it is written to the image.
#[derive(Default)]
pub struct Dir {
    dirs: BTreeMap<String, Dir>,
    files: BTreeMap<String, Vec<u8>>,
}

impl Dir {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a file at a `/`-separated path, creating directories as needed.
    pub fn insert(&mut self, path: &str, data: Vec<u8>) {
        let mut parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        let name = parts.pop().expect("path with no filename");

        let mut dir = self;
        for part in parts {
            dir = dir.dirs.entry(part.to_string()).or_default();
        }
        dir.files.insert(name.to_string(), data);
    }

    /// Read a directory from the host filesystem.
    pub fn from_host(root: &Path) -> io::Result<Self> {
        let mut dir = Dir::new();
        walk(root, root, &mut dir)?;
        Ok(dir)
    }

    /// Bytes of file content, ignoring metadata overhead.
    pub fn total_bytes(&self) -> usize {
        self.files.values().map(|d| d.len()).sum::<usize>()
            + self.dirs.values().map(|d| d.total_bytes()).sum::<usize>()
    }
}

fn walk(root: &Path, current: &Path, dir: &mut Dir) -> io::Result<()> {
    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");

        if path.is_dir() {
            walk(root, &path, dir)?;
        } else {
            dir.insert(&relative, std::fs::read(&path)?);
        }
    }
    Ok(())
}

/// Which FAT variant to produce.
///
/// FAT32 for the GPT disk image, because that is what UEFI requires of an EFI
/// system partition. FAT16 for the El Torito image inside an ISO, where the
/// boot catalog records the image size as a 16-bit count of 512-byte sectors —
/// so the image must stay under 32 MiB, which is below FAT32's minimum cluster
/// count.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FatKind {
    Fat16,
    Fat32,
}

impl FatKind {
    fn entry_bytes(self) -> u32 {
        match self {
            FatKind::Fat16 => 2,
            FatKind::Fat32 => 4,
        }
    }

    fn end_of_chain(self) -> u32 {
        match self {
            FatKind::Fat16 => 0xFFFF,
            FatKind::Fat32 => EOC,
        }
    }

    fn media_entry(self) -> u32 {
        match self {
            FatKind::Fat16 => 0xFFF8,
            FatKind::Fat32 => 0x0FFF_FFF8,
        }
    }

    /// FAT16 keeps the root directory in a fixed area; FAT32 makes it a normal
    /// cluster chain.
    fn root_entries(self) -> u32 {
        match self {
            FatKind::Fat16 => 512,
            FatKind::Fat32 => 0,
        }
    }
}

pub struct Fat32 {
    /// The partition's bytes; index 0 is its first sector.
    data: Vec<u8>,
    kind: FatKind,
    sectors_per_cluster: u32,
    reserved_sectors: u32,
    num_fats: u32,
    fat_sectors: u32,
    total_sectors: u32,
    next_free: u32,
}

impl Fat32 {
    /// Format a volume of `total_sectors` 512-byte sectors.
    pub fn format(total_sectors: u32, volume_label: &str, kind: FatKind) -> Self {
        // FAT16 needs at least 4085 clusters to be recognised as FAT16 rather
        // than FAT12, so give it larger clusters on a small volume.
        let sectors_per_cluster = match kind {
            FatKind::Fat16 => 4,
            FatKind::Fat32 => 1,
        };
        let reserved_sectors = match kind {
            FatKind::Fat16 => 1,
            FatKind::Fat32 => 32,
        };
        let num_fats = 2;
        let root_dir_sectors = (kind.root_entries() * 32).div_ceil(SECTOR as u32);

        // Solve for the FAT size: it depends on the cluster count, which in
        // turn depends on how many sectors the FATs consume.
        let mut fat_sectors = 1;
        loop {
            let data_sectors =
                total_sectors - reserved_sectors - num_fats * fat_sectors - root_dir_sectors;
            let clusters = data_sectors / sectors_per_cluster;
            let needed = ((clusters + 2) * kind.entry_bytes()).div_ceil(SECTOR as u32);
            if needed <= fat_sectors {
                break;
            }
            fat_sectors = needed;
        }

        let mut fs = Self {
            data: vec![0; total_sectors as usize * SECTOR],
            kind,
            sectors_per_cluster,
            reserved_sectors,
            num_fats,
            fat_sectors,
            total_sectors,
            // Clusters 0 and 1 are reserved. On FAT32 the root directory then
            // takes cluster 2; on FAT16 it lives outside the data area.
            next_free: 2,
        };

        if kind == FatKind::Fat32 {
            fs.write_fat32_boot_sector(volume_label);
            fs.write_fs_info();
        } else {
            fs.write_fat16_boot_sector(volume_label);
        }

        // Reserved FAT entries: media descriptor and end-of-chain.
        fs.set_fat_entry(0, kind.media_entry());
        fs.set_fat_entry(1, kind.end_of_chain());

        fs
    }

    fn root_dir_sectors(&self) -> u32 {
        (self.kind.root_entries() * 32).div_ceil(SECTOR as u32)
    }

    pub fn cluster_count(&self) -> u32 {
        (self.total_sectors
            - self.reserved_sectors
            - self.num_fats * self.fat_sectors
            - self.root_dir_sectors())
            / self.sectors_per_cluster
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }

    /// FAT16's extended BPB sits at different offsets from FAT32's, and it
    /// declares a fixed root directory size.
    fn write_fat16_boot_sector(&mut self, label: &str) {
        let mut sector = [0u8; SECTOR];

        sector[0..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
        sector[3..11].copy_from_slice(b"MSDOS5.0");

        sector[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
        sector[13] = self.sectors_per_cluster as u8;
        sector[14..16].copy_from_slice(&(self.reserved_sectors as u16).to_le_bytes());
        sector[16] = self.num_fats as u8;
        sector[17..19].copy_from_slice(&(self.kind.root_entries() as u16).to_le_bytes());

        // The 16-bit total is used when it fits, which it always does here.
        if self.total_sectors < 0x10000 {
            sector[19..21].copy_from_slice(&(self.total_sectors as u16).to_le_bytes());
        } else {
            sector[32..36].copy_from_slice(&self.total_sectors.to_le_bytes());
        }

        sector[21] = 0xF8;
        sector[22..24].copy_from_slice(&(self.fat_sectors as u16).to_le_bytes());
        sector[24..26].copy_from_slice(&32u16.to_le_bytes());
        sector[26..28].copy_from_slice(&8u16.to_le_bytes());

        sector[36] = 0x80; // drive number
        sector[38] = 0x29; // extended boot signature
        sector[39..43].copy_from_slice(&0x4B45_5354u32.to_le_bytes());

        let mut label_bytes = [b' '; 11];
        for (i, b) in label.bytes().take(11).enumerate() {
            label_bytes[i] = b.to_ascii_uppercase();
        }
        sector[43..54].copy_from_slice(&label_bytes);
        sector[54..62].copy_from_slice(b"FAT16   ");

        sector[510] = 0x55;
        sector[511] = 0xAA;

        self.data[0..SECTOR].copy_from_slice(&sector);
    }

    fn write_fat32_boot_sector(&mut self, label: &str) {
        let mut sector = [0u8; SECTOR];

        // Short jump over the BPB, then a nop — firmware checks for this.
        sector[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
        sector[3..11].copy_from_slice(b"MSWIN4.1");

        sector[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
        sector[13] = self.sectors_per_cluster as u8;
        sector[14..16].copy_from_slice(&(self.reserved_sectors as u16).to_le_bytes());
        sector[16] = self.num_fats as u8;
        sector[17..19].copy_from_slice(&0u16.to_le_bytes()); // no fixed root dir on FAT32
        sector[19..21].copy_from_slice(&0u16.to_le_bytes()); // 16-bit total unused
        sector[21] = 0xF8; // fixed disk
        sector[22..24].copy_from_slice(&0u16.to_le_bytes()); // 16-bit FAT size unused
        sector[24..26].copy_from_slice(&32u16.to_le_bytes()); // sectors per track
        sector[26..28].copy_from_slice(&8u16.to_le_bytes()); // heads
        sector[28..32].copy_from_slice(&0u32.to_le_bytes()); // hidden sectors
        sector[32..36].copy_from_slice(&self.total_sectors.to_le_bytes());

        // FAT32 extended BPB.
        sector[36..40].copy_from_slice(&self.fat_sectors.to_le_bytes());
        sector[40..42].copy_from_slice(&0u16.to_le_bytes()); // flags: FATs mirrored
        sector[42..44].copy_from_slice(&0u16.to_le_bytes()); // version
        sector[44..48].copy_from_slice(&2u32.to_le_bytes()); // root cluster
        sector[48..50].copy_from_slice(&1u16.to_le_bytes()); // FSInfo sector
        sector[50..52].copy_from_slice(&6u16.to_le_bytes()); // backup boot sector
        sector[64] = 0x80; // drive number
        sector[66] = 0x29; // extended boot signature
        sector[67..71].copy_from_slice(&0x4B45_5354u32.to_le_bytes()); // volume id

        let mut label_bytes = [b' '; 11];
        for (i, b) in label.bytes().take(11).enumerate() {
            label_bytes[i] = b.to_ascii_uppercase();
        }
        sector[71..82].copy_from_slice(&label_bytes);
        sector[82..90].copy_from_slice(b"FAT32   ");

        sector[510] = 0x55;
        sector[511] = 0xAA;

        self.data[0..SECTOR].copy_from_slice(&sector);
        // The backup at sector 6 is what firmware falls back to.
        self.data[6 * SECTOR..7 * SECTOR].copy_from_slice(&sector);
    }

    fn write_fs_info(&mut self) {
        let mut sector = [0u8; SECTOR];
        sector[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
        sector[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
        sector[488..492].copy_from_slice(&u32::MAX.to_le_bytes()); // free count unknown
        sector[492..496].copy_from_slice(&u32::MAX.to_le_bytes()); // next free unknown
        sector[508..512].copy_from_slice(&0xAA55_0000u32.to_le_bytes());

        self.data[SECTOR..2 * SECTOR].copy_from_slice(&sector);
        self.data[7 * SECTOR..8 * SECTOR].copy_from_slice(&sector);
    }

    fn set_fat_entry(&mut self, cluster: u32, value: u32) {
        let width = self.kind.entry_bytes() as usize;

        for fat in 0..self.num_fats {
            let base = (self.reserved_sectors + fat * self.fat_sectors) as usize * SECTOR;
            let offset = base + cluster as usize * width;

            match self.kind {
                FatKind::Fat16 => {
                    self.data[offset..offset + 2].copy_from_slice(&(value as u16).to_le_bytes())
                }
                FatKind::Fat32 => self.data[offset..offset + 4]
                    // The top four bits of a FAT32 entry are reserved.
                    .copy_from_slice(&(value & 0x0FFF_FFFF).to_le_bytes()),
            }
        }
    }

    /// Byte offset of the fixed FAT16 root directory area.
    fn root_dir_offset(&self) -> usize {
        (self.reserved_sectors + self.num_fats * self.fat_sectors) as usize * SECTOR
    }

    fn cluster_offset(&self, cluster: u32) -> usize {
        let first_data_sector =
            self.reserved_sectors + self.num_fats * self.fat_sectors + self.root_dir_sectors();
        let sector = first_data_sector + (cluster - 2) * self.sectors_per_cluster;
        sector as usize * SECTOR
    }

    fn cluster_bytes(&self) -> usize {
        self.sectors_per_cluster as usize * SECTOR
    }

    /// Reserve a chain long enough for `len` bytes and link it in the FAT.
    fn allocate(&mut self, len: usize) -> Vec<u32> {
        let cluster_size = self.cluster_bytes();
        let count = len.div_ceil(cluster_size).max(1);

        let clusters: Vec<u32> = (0..count as u32).map(|i| self.next_free + i).collect();
        self.next_free += count as u32;

        assert!(
            self.next_free <= self.cluster_count() + 2,
            "image too small for its contents"
        );

        for window in clusters.windows(2) {
            self.set_fat_entry(window[0], window[1]);
        }
        self.set_fat_entry(*clusters.last().unwrap(), self.kind.end_of_chain());

        clusters
    }

    fn write_clusters(&mut self, clusters: &[u32], data: &[u8]) {
        let cluster_size = self.cluster_bytes();
        for (i, &cluster) in clusters.iter().enumerate() {
            let start = i * cluster_size;
            if start >= data.len() {
                break;
            }
            let end = (start + cluster_size).min(data.len());
            let offset = self.cluster_offset(cluster);
            self.data[offset..offset + (end - start)].copy_from_slice(&data[start..end]);
        }
    }

    /// Write a whole directory tree, starting at the root.
    pub fn write_tree(&mut self, root: &Dir) {
        match self.kind {
            FatKind::Fat32 => {
                // The FAT32 root is an ordinary chain that must begin at
                // cluster 2, so it has to be allocated before anything else.
                let clusters = self.allocate(entries_size(root, false));
                assert_eq!(clusters[0], 2, "root directory must land on cluster 2");

                let entries = self.build_dir(root, None);
                self.write_clusters(&clusters, &entries);
            }
            FatKind::Fat16 => {
                // The FAT16 root lives in a fixed area outside the data
                // region, and cannot grow.
                let entries = self.build_dir(root, None);
                let capacity = self.kind.root_entries() as usize * 32;
                assert!(
                    entries.len() <= capacity,
                    "too many root entries for a FAT16 root directory"
                );

                let offset = self.root_dir_offset();
                self.data[offset..offset + entries.len()].copy_from_slice(&entries);
            }
        }
    }

    /// Lay out one directory's contents, allocating and writing everything
    /// beneath it, and return its raw directory entries.
    ///
    /// `location` is `None` for the root, which has no `.`/`..` links.
    fn build_dir(&mut self, dir: &Dir, location: Option<(u32, u32)>) -> Vec<u8> {
        let mut entries: Vec<u8> = Vec::new();
        let mut used_names: Vec<[u8; 11]> = Vec::new();

        if let Some((self_cluster, parent_cluster)) = location {
            entries.extend_from_slice(&dot_entry(b".          ", self_cluster));
            // A `..` that refers to the root is written as cluster 0 by
            // convention, never as cluster 2.
            entries.extend_from_slice(&dot_entry(b"..         ", parent_cluster));
        }

        // The root's own cluster is 2 on FAT32 and "0" (the fixed area) on
        // FAT16; children record that as their parent.
        let own_cluster = location.map_or(
            if self.kind == FatKind::Fat32 { 2 } else { 0 },
            |(own, _)| own,
        );

        for (name, child) in &dir.dirs {
            let child_clusters = self.allocate(entries_size(child, true));
            let child_entries = self.build_dir(child, Some((child_clusters[0], own_cluster)));
            self.write_clusters(&child_clusters, &child_entries);

            let short = short_name(name, &mut used_names);
            append_entry(&mut entries, name, short, ATTR_DIRECTORY, child_clusters[0], 0);
        }

        for (name, data) in &dir.files {
            let file_clusters = self.allocate(data.len());
            self.write_clusters(&file_clusters, data);

            let short = short_name(name, &mut used_names);
            append_entry(
                &mut entries,
                name,
                short,
                ATTR_READ_ONLY,
                file_clusters[0],
                data.len() as u32,
            );
        }

        entries
    }
}

/// Bytes of directory entries a directory will need, so its chain can be
/// allocated before its contents are known.
fn entries_size(dir: &Dir, has_dot_entries: bool) -> usize {
    let mut count = if has_dot_entries { 2 } else { 0 };

    for name in dir.dirs.keys().chain(dir.files.keys()) {
        count += 1 + long_name_entries(name);
    }

    // One extra, always zero, terminates the directory listing.
    (count + 1) * 32
}

/// How many long-filename entries a name needs; zero if 8.3 suffices.
fn long_name_entries(name: &str) -> usize {
    if fits_short(name) {
        0
    } else {
        name.chars().count().div_ceil(13)
    }
}

/// Does this name fit in 8.3 without losing information?
fn fits_short(name: &str) -> bool {
    if name.chars().any(|c| c.is_lowercase()) {
        return false;
    }

    let mut parts = name.splitn(2, '.');
    let stem = parts.next().unwrap_or("");
    let ext = parts.next().unwrap_or("");

    if stem.is_empty() || stem.len() > 8 || ext.len() > 3 {
        return false;
    }

    name.chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || "_-~.".contains(c))
}

/// Derive a unique 8.3 short name, using the `~N` convention on collision.
fn short_name(name: &str, used: &mut Vec<[u8; 11]>) -> [u8; 11] {
    let mut parts = name.rsplitn(2, '.');
    let (ext, stem) = match name.find('.') {
        Some(_) => {
            let ext = parts.next().unwrap_or("");
            let stem = parts.next().unwrap_or("");
            (ext, stem)
        }
        None => ("", name),
    };

    let sanitise = |s: &str| -> Vec<u8> {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric() || "_-".contains(*c))
            .map(|c| c.to_ascii_uppercase() as u8)
            .collect()
    };

    let stem_bytes = sanitise(stem);
    let ext_bytes = sanitise(ext);

    let mut candidate = [b' '; 11];
    for (i, b) in ext_bytes.iter().take(3).enumerate() {
        candidate[8 + i] = *b;
    }

    // Use the name as-is when it already fits and nothing else claimed it.
    if fits_short(name) {
        for (i, b) in stem_bytes.iter().take(8).enumerate() {
            candidate[i] = *b;
        }
        if !used.contains(&candidate) {
            used.push(candidate);
            return candidate;
        }
        candidate[0..8].fill(b' ');
    }

    // "LIMINE~1.CON" style: truncate the stem and append a numeric tail.
    for suffix in 1..=99u32 {
        let tail = format!("~{suffix}");
        let keep = 8 - tail.len();

        let mut name8 = [b' '; 8];
        for (i, b) in stem_bytes.iter().take(keep).enumerate() {
            name8[i] = *b;
        }
        for (i, b) in tail.bytes().enumerate() {
            name8[keep + i] = b;
        }

        candidate[0..8].copy_from_slice(&name8);
        if !used.contains(&candidate) {
            used.push(candidate);
            return candidate;
        }
    }

    panic!("could not derive a unique short name for {name}");
}

/// The checksum that ties long-name entries to their short entry.
fn short_name_checksum(short: &[u8; 11]) -> u8 {
    let mut sum = 0u8;
    for &byte in short {
        sum = ((sum & 1) << 7).wrapping_add(sum >> 1).wrapping_add(byte);
    }
    sum
}

fn dot_entry(name: &[u8; 11], cluster: u32) -> [u8; 32] {
    let mut entry = [0u8; 32];
    entry[0..11].copy_from_slice(name);
    entry[11] = ATTR_DIRECTORY;
    entry[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    entry[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
    entry
}

/// Byte 12 flags saying the stored (always uppercase) name reads back
/// lowercased. A name that is only a case variant of its 8.3 form needs no
/// long-name entries at all, which is what other systems produce.
const CASE_STEM_LOWER: u8 = 0x08;
const CASE_EXTENSION_LOWER: u8 = 0x10;

/// Flags if `name` differs from its 8.3 form only by case, else `None`.
fn case_only_name(name: &str) -> Option<u8> {
    let (stem, extension) = match name.rfind('.') {
        Some(dot) => (&name[..dot], &name[dot + 1..]),
        None => (name, ""),
    };

    if stem.is_empty() || stem.len() > 8 || extension.len() > 3 {
        return None;
    }

    let usable = |text: &str| {
        text.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-~".contains(c))
    };
    // Each half has only one flag, so it must be entirely one case.
    let uniform = |text: &str| {
        !text.chars().any(|c| c.is_ascii_lowercase())
            || !text.chars().any(|c| c.is_ascii_uppercase())
    };

    if !usable(stem) || !usable(extension) || !uniform(stem) || !uniform(extension) {
        return None;
    }

    let mut flags = 0;
    if stem.chars().any(|c| c.is_ascii_lowercase()) {
        flags |= CASE_STEM_LOWER;
    }
    if extension.chars().any(|c| c.is_ascii_lowercase()) {
        flags |= CASE_EXTENSION_LOWER;
    }

    Some(flags)
}

/// Append a file or directory entry, preceded by long-name entries if needed.
fn append_entry(out: &mut Vec<u8>, name: &str, short: [u8; 11], attr: u8, cluster: u32, size: u32) {
    // Case-only names go straight into the short entry.
    if let Some(flags) = case_only_name(name) {
        let mut entry = [0u8; 32];
        let (stem, extension) = match name.rfind('.') {
            Some(dot) => (&name[..dot], &name[dot + 1..]),
            None => (name, ""),
        };

        entry[0..11].fill(b' ');
        for (index, c) in stem.chars().take(8).enumerate() {
            entry[index] = c.to_ascii_uppercase() as u8;
        }
        for (index, c) in extension.chars().take(3).enumerate() {
            entry[8 + index] = c.to_ascii_uppercase() as u8;
        }

        entry[11] = attr;
        entry[12] = flags;
        entry[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
        entry[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
        entry[28..32].copy_from_slice(&size.to_le_bytes());

        out.extend_from_slice(&entry);
        return;
    }

    let long_count = long_name_entries(name);

    if long_count > 0 {
        let checksum = short_name_checksum(&short);
        let utf16: Vec<u16> = name.encode_utf16().collect();

        // Long-name entries are stored in reverse, last chunk first.
        for index in (0..long_count).rev() {
            let mut entry = [0u8; 32];

            let mut sequence = (index + 1) as u8;
            if index == long_count - 1 {
                sequence |= 0x40; // marks the final chunk
            }
            entry[0] = sequence;
            entry[11] = ATTR_LONG_NAME;
            entry[13] = checksum;

            // 13 UTF-16 units split across three runs inside the entry.
            const SLOTS: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
            for (slot, &offset) in SLOTS.iter().enumerate() {
                let position = index * 13 + slot;
                let unit = match position.cmp(&utf16.len()) {
                    std::cmp::Ordering::Less => utf16[position],
                    std::cmp::Ordering::Equal => 0x0000, // terminator
                    std::cmp::Ordering::Greater => 0xFFFF, // padding
                };
                entry[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
            }

            out.extend_from_slice(&entry);
        }
    }

    let mut entry = [0u8; 32];
    entry[0..11].copy_from_slice(&short);
    entry[11] = attr;
    entry[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    entry[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
    entry[28..32].copy_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&entry);
}
