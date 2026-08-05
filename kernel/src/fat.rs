//! A read-only FAT32 driver for the boot disk's EFI system partition.
//!
//! This is the other side of the formatter in `xtask`: the same layout, read
//! back on real hardware. Long filenames matter here too — the files this has
//! to find include `limine.conf`.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use spin::Mutex;

use crate::ahci::{self, SECTOR_SIZE};
use crate::gpt;

const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_LONG_NAME: u8 = 0x0F;

/// Marks a directory entry as deleted; the rest of it is still intact.
const ENTRY_DELETED: u8 = 0xE5;
/// A zero first byte means this entry, and every one after it, is unused.
const ENTRY_END: u8 = 0x00;

/// Values at or above this in a FAT32 chain mean "no more clusters".
const CLUSTER_END: u32 = 0x0FFF_FFF8;

pub struct Entry {
    pub name: String,
    pub is_directory: bool,
    pub size: u32,
    first_cluster: u32,
}

pub struct Fat32 {
    /// Where the partition starts on disk, in sectors.
    partition_lba: u64,
    sectors_per_cluster: u32,
    reserved_sectors: u32,
    number_of_fats: u32,
    sectors_per_fat: u32,
    root_cluster: u32,
    total_sectors: u32,
}

fn read_u16(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

impl Fat32 {
    /// Read the BIOS parameter block and check it describes a FAT32 volume.
    pub fn mount(partition_lba: u64) -> Result<Self, &'static str> {
        let mut boot = [0u8; SECTOR_SIZE];
        ahci::read(partition_lba, 1, &mut boot)?;

        if read_u16(&boot, 510) != 0xAA55 {
            return Err("no boot signature on the partition");
        }
        if read_u16(&boot, 11) as usize != SECTOR_SIZE {
            return Err("unsupported sector size");
        }

        // FAT32 is identified by a zero 16-bit FAT size; the real one is the
        // 32-bit field in the extended BPB.
        let sectors_per_fat = if read_u16(&boot, 22) == 0 {
            read_u32(&boot, 36)
        } else {
            return Err("not a FAT32 volume");
        };

        // The 16-bit total is used on small volumes, the 32-bit one otherwise.
        let total_sectors = match read_u16(&boot, 19) {
            0 => read_u32(&boot, 32),
            small => small as u32,
        };

        Ok(Self {
            partition_lba,
            sectors_per_cluster: boot[13] as u32,
            reserved_sectors: read_u16(&boot, 14) as u32,
            number_of_fats: boot[16] as u32,
            sectors_per_fat,
            root_cluster: read_u32(&boot, 44),
            total_sectors,
        })
    }

    /// Highest valid cluster number, plus one.
    fn cluster_limit(&self) -> u32 {
        let data_sectors = self.total_sectors - self.first_data_sector();
        data_sectors / self.sectors_per_cluster + 2
    }

    fn first_data_sector(&self) -> u32 {
        self.reserved_sectors + self.number_of_fats * self.sectors_per_fat
    }

    fn cluster_lba(&self, cluster: u32) -> u64 {
        // Clusters are numbered from 2, and cluster 2 is the first data one.
        self.partition_lba
            + (self.first_data_sector() + (cluster - 2) * self.sectors_per_cluster) as u64
    }

    fn cluster_bytes(&self) -> usize {
        self.sectors_per_cluster as usize * SECTOR_SIZE
    }

    fn read_cluster(&self, cluster: u32) -> Result<Vec<u8>, &'static str> {
        let mut data = vec![0u8; self.cluster_bytes()];
        ahci::read(
            self.cluster_lba(cluster),
            self.sectors_per_cluster as usize,
            &mut data,
        )?;
        Ok(data)
    }

    /// Follow the FAT to the next cluster in a chain.
    fn next_cluster(&self, cluster: u32) -> Result<Option<u32>, &'static str> {
        let offset = cluster as usize * 4;
        let sector = self.reserved_sectors as u64 + (offset / SECTOR_SIZE) as u64;

        let mut data = [0u8; SECTOR_SIZE];
        ahci::read(self.partition_lba + sector, 1, &mut data)?;

        // The top four bits of a FAT32 entry are reserved.
        let next = read_u32(&data, offset % SECTOR_SIZE) & 0x0FFF_FFFF;
        Ok(if next >= CLUSTER_END { None } else { Some(next) })
    }

    /// Read a whole cluster chain, stopping once `limit` bytes are in hand.
    fn read_chain(&self, start: u32, limit: usize) -> Result<Vec<u8>, &'static str> {
        let mut data = Vec::new();
        let mut cluster = start;

        // A corrupt FAT could form a loop; cap the walk rather than hang.
        for _ in 0..1_000_000 {
            data.extend_from_slice(&self.read_cluster(cluster)?);

            if limit > 0 && data.len() >= limit {
                data.truncate(limit);
                return Ok(data);
            }

            match self.next_cluster(cluster)? {
                Some(next) => cluster = next,
                None => break,
            }
        }

        if limit > 0 && data.len() > limit {
            data.truncate(limit);
        }
        Ok(data)
    }

    /// Every cluster making up a chain.
    fn chain(&self, start: u32) -> Result<Vec<u32>, &'static str> {
        let mut clusters = Vec::new();
        let mut cluster = start;

        for _ in 0..1_000_000 {
            clusters.push(cluster);
            match self.next_cluster(cluster)? {
                Some(next) => cluster = next,
                None => break,
            }
        }

        Ok(clusters)
    }

    /// Overwrite one whole cluster.
    fn write_cluster(&self, cluster: u32, data: &[u8]) -> Result<(), &'static str> {
        let mut padded = vec![0u8; self.cluster_bytes()];
        let length = data.len().min(padded.len());
        padded[..length].copy_from_slice(&data[..length]);

        ahci::write(
            self.cluster_lba(cluster),
            self.sectors_per_cluster as usize,
            &padded,
        )
    }

    /// Set a FAT entry, in every copy of the FAT.
    ///
    /// The FATs are mirrored, so a write that updated only one would leave the
    /// volume inconsistent for any other driver that reads the second.
    fn set_fat_entry(&self, cluster: u32, value: u32) -> Result<(), &'static str> {
        let byte_offset = cluster as usize * 4;
        let sector_in_fat = (byte_offset / SECTOR_SIZE) as u64;
        let offset = byte_offset % SECTOR_SIZE;

        for copy in 0..self.number_of_fats as u64 {
            let sector = self.partition_lba
                + self.reserved_sectors as u64
                + copy * self.sectors_per_fat as u64
                + sector_in_fat;

            let mut data = [0u8; SECTOR_SIZE];
            ahci::read(sector, 1, &mut data)?;

            // The top four bits of a FAT32 entry are reserved and must be
            // carried over rather than zeroed.
            let existing = read_u32(&data, offset) & 0xF000_0000;
            let combined = existing | (value & 0x0FFF_FFFF);
            data[offset..offset + 4].copy_from_slice(&combined.to_le_bytes());

            ahci::write(sector, 1, &data)?;
        }

        Ok(())
    }

    fn fat_entry(&self, cluster: u32) -> Result<u32, &'static str> {
        let byte_offset = cluster as usize * 4;
        let sector = self.partition_lba
            + self.reserved_sectors as u64
            + (byte_offset / SECTOR_SIZE) as u64;

        let mut data = [0u8; SECTOR_SIZE];
        ahci::read(sector, 1, &mut data)?;
        Ok(read_u32(&data, byte_offset % SECTOR_SIZE) & 0x0FFF_FFFF)
    }

    /// Reserve `count` clusters and link them into a chain.
    fn allocate_chain(&self, count: usize) -> Result<Vec<u32>, &'static str> {
        let mut clusters = Vec::new();
        let limit = self.cluster_limit();
        let mut candidate = 2;

        while clusters.len() < count {
            if candidate >= limit {
                return Err("the disk is full");
            }
            if self.fat_entry(candidate)? == 0 {
                clusters.push(candidate);
            }
            candidate += 1;
        }

        // Link them, then terminate the chain.
        for pair in clusters.windows(2) {
            self.set_fat_entry(pair[0], pair[1])?;
        }
        self.set_fat_entry(*clusters.last().unwrap(), CLUSTER_END | 0x7)?;

        Ok(clusters)
    }

    /// Mark every cluster in a chain free.
    fn free_chain(&self, start: u32) -> Result<(), &'static str> {
        if start < 2 {
            return Ok(());
        }
        for cluster in self.chain(start)? {
            self.set_fat_entry(cluster, 0)?;
        }
        Ok(())
    }

    /// List one directory, given its starting cluster.
    fn read_directory(&self, cluster: u32) -> Result<Vec<Entry>, &'static str> {
        let data = self.read_chain(cluster, 0)?;
        let mut entries = Vec::new();

        // Long names arrive in reverse order before their short entry, so
        // collect the fragments and assemble when the short entry appears.
        let mut long_name: Vec<(u8, [u16; 13])> = Vec::new();

        for chunk in data.chunks_exact(32) {
            match chunk[0] {
                ENTRY_END => break,
                ENTRY_DELETED => {
                    long_name.clear();
                    continue;
                }
                _ => {}
            }

            let attributes = chunk[11];

            if attributes == ATTR_LONG_NAME {
                let sequence = chunk[0] & 0x1F;
                let mut part = [0u16; 13];
                // The 13 UTF-16 units are split across three runs.
                const SLOTS: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
                for (index, &offset) in SLOTS.iter().enumerate() {
                    part[index] = read_u16(chunk, offset);
                }
                long_name.push((sequence, part));
                continue;
            }

            if attributes & ATTR_VOLUME_ID != 0 {
                long_name.clear();
                continue;
            }

            let name = if long_name.is_empty() {
                short_name(chunk)
            } else {
                long_name.sort_by_key(|(sequence, _)| *sequence);
                assemble_long_name(&long_name)
            };
            long_name.clear();

            // `.` and `..` are real entries on disk, but callers want a
            // listing, not the links.
            if name == "." || name == ".." {
                continue;
            }

            let first_cluster =
                ((read_u16(chunk, 20) as u32) << 16) | read_u16(chunk, 26) as u32;

            entries.push(Entry {
                name,
                is_directory: attributes & ATTR_DIRECTORY != 0,
                size: read_u32(chunk, 28),
                first_cluster,
            });
        }

        Ok(entries)
    }

    /// Resolve an absolute path to its directory entry.
    fn lookup(&self, path: &str) -> Result<Option<Entry>, &'static str> {
        let mut cluster = self.root_cluster;
        let mut found: Option<Entry> = None;

        for component in path.split('/').filter(|part| !part.is_empty()) {
            let entries = self.read_directory(cluster)?;

            let Some(entry) = entries
                .into_iter()
                .find(|entry| entry.name.eq_ignore_ascii_case(component))
            else {
                return Ok(None);
            };

            cluster = if entry.first_cluster == 0 {
                self.root_cluster
            } else {
                entry.first_cluster
            };
            found = Some(entry);
        }

        Ok(found)
    }

    pub fn list(&self, path: &str) -> Result<Option<Vec<Entry>>, &'static str> {
        let cluster = match self.lookup(path)? {
            None if is_root(path) => self.root_cluster,
            None => return Ok(None),
            Some(entry) if entry.is_directory => {
                if entry.first_cluster == 0 {
                    self.root_cluster
                } else {
                    entry.first_cluster
                }
            }
            Some(_) => return Ok(None),
        };

        Ok(Some(self.read_directory(cluster)?))
    }

    pub fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, &'static str> {
        let Some(entry) = self.lookup(path)? else {
            return Ok(None);
        };
        if entry.is_directory {
            return Ok(None);
        }
        if entry.size == 0 {
            return Ok(Some(Vec::new()));
        }

        Ok(Some(self.read_chain(entry.first_cluster, entry.size as usize)?))
    }

    /// Resolve a path's parent directory to its starting cluster.
    fn parent_cluster(&self, path: &str) -> Result<Option<u32>, &'static str> {
        let mut parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        if parts.pop().is_none() {
            return Ok(None);
        }

        let mut cluster = self.root_cluster;
        for part in parts {
            let Some(entry) = self
                .read_directory(cluster)?
                .into_iter()
                .find(|entry| entry.name.eq_ignore_ascii_case(part) && entry.is_directory)
            else {
                return Ok(None);
            };
            cluster = if entry.first_cluster == 0 {
                self.root_cluster
            } else {
                entry.first_cluster
            };
        }

        Ok(Some(cluster))
    }

    /// Create or overwrite a file.
    pub fn write_file(&self, path: &str, data: &[u8]) -> Result<(), &'static str> {
        let name = path
            .rsplit('/')
            .find(|part| !part.is_empty())
            .ok_or("invalid path")?;

        let parent = self
            .parent_cluster(path)?
            .ok_or("parent directory does not exist")?;

        // Lay the contents down first, so a failure part-way leaves the
        // directory untouched rather than pointing at a half-written file.
        let clusters = if data.is_empty() {
            Vec::new()
        } else {
            let needed = data.len().div_ceil(self.cluster_bytes());
            let clusters = self.allocate_chain(needed)?;

            for (index, &cluster) in clusters.iter().enumerate() {
                let start = index * self.cluster_bytes();
                let end = (start + self.cluster_bytes()).min(data.len());
                self.write_cluster(cluster, &data[start..end])?;
            }

            clusters
        };

        let first_cluster = clusters.first().copied().unwrap_or(0);

        // Replace an existing entry in place if there is one, freeing the
        // storage it used to point at.
        let mut directory = self.read_chain(parent, 0)?;

        if let Some(location) = locate_entry(&directory, name) {
            if location.first_cluster >= 2 {
                self.free_chain(location.first_cluster)?;
            }

            let slot = location.slot * 32;
            write_cluster_fields(&mut directory[slot..slot + 32], first_cluster, data.len() as u32);
            return self.write_directory(parent, &directory);
        }

        // Otherwise append a new entry, growing the directory if it is full.
        let entries = build_entry(name, first_cluster, data.len() as u32, ATTR_ARCHIVE, &directory);
        self.insert_entry(parent, &mut directory, &entries)
    }

    /// Place a prepared group of entries in a directory, growing it if there
    /// is no room, and write the result back.
    fn insert_entry(
        &self,
        parent: u32,
        directory: &mut Vec<u8>,
        entries: &[u8],
    ) -> Result<(), &'static str> {
        let start = match free_slots(directory, entries.len() / 32) {
            Some(start) => start,
            None => {
                let extra = self.extend_directory(parent)?;
                let start = directory.len() / 32;
                directory.resize(directory.len() + extra, 0);
                start
            }
        };

        let offset = start * 32;
        directory[offset..offset + entries.len()].copy_from_slice(entries);
        self.write_directory(parent, directory)
    }

    /// Create a directory.
    ///
    /// A FAT directory is just a cluster holding entries, but it must start
    /// with `.` and `..` — those links are real entries on disk, not something
    /// the filesystem synthesises.
    pub fn create_directory(&self, path: &str) -> Result<(), &'static str> {
        let name = path
            .rsplit('/')
            .find(|part| !part.is_empty())
            .ok_or("invalid path")?;

        let parent = self
            .parent_cluster(path)?
            .ok_or("parent directory does not exist")?;

        let mut directory = self.read_chain(parent, 0)?;
        if locate_entry(&directory, name).is_some() {
            return Err("already exists");
        }

        let cluster = self.allocate_chain(1)?[0];

        let mut contents = vec![0u8; self.cluster_bytes()];
        contents[0..32].copy_from_slice(&dot_entry(b".          ", cluster));
        // A `..` that refers to the root is recorded as cluster 0 by
        // convention, never as the root's actual cluster number.
        let parent_link = if parent == self.root_cluster { 0 } else { parent };
        contents[32..64].copy_from_slice(&dot_entry(b"..         ", parent_link));
        self.write_cluster(cluster, &contents)?;

        // Directories always record a size of zero; their length comes from
        // the cluster chain.
        let entries = build_entry(name, cluster, 0, ATTR_DIRECTORY, &directory);
        self.insert_entry(parent, &mut directory, &entries)
    }

    /// Remove an empty directory.
    pub fn remove_directory(&self, path: &str) -> Result<(), &'static str> {
        let name = path
            .rsplit('/')
            .find(|part| !part.is_empty())
            .ok_or("invalid path")?;
        let parent = self.parent_cluster(path)?.ok_or("no such directory")?;

        let mut directory = self.read_chain(parent, 0)?;
        let location = locate_entry(&directory, name).ok_or("no such directory")?;

        if !location.is_directory {
            return Err("not a directory");
        }

        // `read_directory` filters out `.` and `..`, so anything left is real
        // content and the directory is not empty.
        if location.first_cluster >= 2 && !self.read_directory(location.first_cluster)?.is_empty() {
            return Err("directory is not empty");
        }

        if location.first_cluster >= 2 {
            self.free_chain(location.first_cluster)?;
        }

        for slot in location.first_slot..=location.slot {
            directory[slot * 32] = ENTRY_DELETED;
        }

        self.write_directory(parent, &directory)
    }

    /// Delete a file, releasing its clusters.
    pub fn remove_file(&self, path: &str) -> Result<(), &'static str> {
        let name = path
            .rsplit('/')
            .find(|part| !part.is_empty())
            .ok_or("invalid path")?;
        let parent = self.parent_cluster(path)?.ok_or("no such file")?;

        let mut directory = self.read_chain(parent, 0)?;
        let location = locate_entry(&directory, name).ok_or("no such file")?;

        if location.is_directory {
            return Err("is a directory");
        }
        if location.first_cluster >= 2 {
            self.free_chain(location.first_cluster)?;
        }

        // Mark the short entry and its long-name entries deleted. The rest of
        // each slot is left alone, which is what FAT expects.
        for slot in location.first_slot..=location.slot {
            directory[slot * 32] = ENTRY_DELETED;
        }

        self.write_directory(parent, &directory)
    }

    /// Write a directory's slots back over its cluster chain.
    fn write_directory(&self, start: u32, data: &[u8]) -> Result<(), &'static str> {
        let clusters = self.chain(start)?;
        let size = self.cluster_bytes();

        for (index, &cluster) in clusters.iter().enumerate() {
            let offset = index * size;
            if offset >= data.len() {
                break;
            }
            let end = (offset + size).min(data.len());
            self.write_cluster(cluster, &data[offset..end])?;
        }

        Ok(())
    }

    /// Append one zeroed cluster to a directory, returning its size in bytes.
    fn extend_directory(&self, start: u32) -> Result<usize, &'static str> {
        let clusters = self.chain(start)?;
        let last = *clusters.last().ok_or("empty directory chain")?;

        let new = self.allocate_chain(1)?[0];
        self.write_cluster(new, &[])?;
        self.set_fat_entry(last, new)?;

        Ok(self.cluster_bytes())
    }

    pub fn is_directory(&self, path: &str) -> Result<bool, &'static str> {
        if is_root(path) {
            return Ok(true);
        }
        Ok(self.lookup(path)?.is_some_and(|entry| entry.is_directory))
    }

    pub fn exists(&self, path: &str) -> Result<bool, &'static str> {
        if is_root(path) {
            return Ok(true);
        }
        Ok(self.lookup(path)?.is_some())
    }
}

/// Offsets of the 13 UTF-16 units inside a long-name entry, which the format
/// splits across three separate runs.
const LONG_NAME_SLOTS: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];

/// Where a directory entry lives, in 32-byte slots from the start of the
/// directory's data.
struct Location {
    /// The short entry.
    slot: usize,
    /// The first slot of the group, which is a long-name entry if there is one.
    first_slot: usize,
    first_cluster: u32,
    is_directory: bool,
}

/// Find an entry by name within a directory's raw slots.
fn locate_entry(directory: &[u8], name: &str) -> Option<Location> {
    let mut long: Vec<(u8, [u16; 13])> = Vec::new();
    let mut group_start = 0usize;

    for (slot, chunk) in directory.chunks_exact(32).enumerate() {
        match chunk[0] {
            ENTRY_END => break,
            ENTRY_DELETED => {
                long.clear();
                group_start = slot + 1;
                continue;
            }
            _ => {}
        }

        let attributes = chunk[11];

        if attributes == ATTR_LONG_NAME {
            if long.is_empty() {
                group_start = slot;
            }
            let mut part = [0u16; 13];
            for (index, &offset) in LONG_NAME_SLOTS.iter().enumerate() {
                part[index] = read_u16(chunk, offset);
            }
            long.push((chunk[0] & 0x1F, part));
            continue;
        }

        if attributes & ATTR_VOLUME_ID != 0 {
            long.clear();
            group_start = slot + 1;
            continue;
        }

        let entry_name = if long.is_empty() {
            short_name(chunk)
        } else {
            long.sort_by_key(|(sequence, _)| *sequence);
            assemble_long_name(&long)
        };
        let first_slot = if long.is_empty() { slot } else { group_start };
        long.clear();
        group_start = slot + 1;

        if entry_name.eq_ignore_ascii_case(name) && entry_name != "." && entry_name != ".." {
            return Some(Location {
                slot,
                first_slot,
                first_cluster: ((read_u16(chunk, 20) as u32) << 16) | read_u16(chunk, 26) as u32,
                is_directory: attributes & ATTR_DIRECTORY != 0,
            });
        }
    }

    None
}

/// Find `count` consecutive unused slots.
fn free_slots(directory: &[u8], count: usize) -> Option<usize> {
    let mut run = 0;

    for (slot, chunk) in directory.chunks_exact(32).enumerate() {
        if chunk[0] == ENTRY_END || chunk[0] == ENTRY_DELETED {
            run += 1;
            if run == count {
                return Some(slot + 1 - count);
            }
        } else {
            run = 0;
        }
    }

    None
}

/// Overwrite the cluster and size fields of an existing short entry.
fn write_cluster_fields(entry: &mut [u8], first_cluster: u32, size: u32) {
    entry[20..22].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
    entry[26..28].copy_from_slice(&(first_cluster as u16).to_le_bytes());
    entry[28..32].copy_from_slice(&size.to_le_bytes());
}

/// A `.` or `..` link, which FAT stores as an ordinary directory entry.
fn dot_entry(name: &[u8; 11], cluster: u32) -> [u8; 32] {
    let mut entry = [0u8; 32];
    entry[0..11].copy_from_slice(name);
    entry[11] = ATTR_DIRECTORY;
    write_cluster_fields(&mut entry, cluster, 0);
    entry
}

/// Build the slots for a new entry: long-name entries if needed, then the
/// short entry.
fn build_entry(
    name: &str,
    first_cluster: u32,
    size: u32,
    attributes: u8,
    directory: &[u8],
) -> Vec<u8> {
    // A name that is only a case variant of its 8.3 form fits the short entry
    // outright, provided nothing else has claimed that short name.
    let case_flags = case_only_name(name).filter(|_| {
        let mut candidate = [b' '; 11];
        fill_short(&mut candidate, name);
        !short_name_taken(directory, &candidate)
    });

    if let Some(flags) = case_flags {
        let mut entry = [0u8; 32];
        fill_short_slice(&mut entry, name);
        entry[11] = attributes;
        entry[12] = flags;
        write_cluster_fields(&mut entry, first_cluster, size);
        return entry.to_vec();
    }

    let short = unique_short_name(name, directory);
    let mut out = Vec::new();

    if !fits_short(name) {
        let checksum = short_name_checksum(&short);
        let units: Vec<u16> = name.encode_utf16().collect();
        let parts = units.len().div_ceil(13);

        // Long-name entries are stored in reverse, last chunk first.
        for index in (0..parts).rev() {
            let mut entry = [0u8; 32];
            entry[0] = (index + 1) as u8;
            if index == parts - 1 {
                entry[0] |= 0x40; // marks the final chunk
            }
            entry[11] = ATTR_LONG_NAME;
            entry[13] = checksum;

            for (slot, &offset) in LONG_NAME_SLOTS.iter().enumerate() {
                let position = index * 13 + slot;
                let unit = match position.cmp(&units.len()) {
                    core::cmp::Ordering::Less => units[position],
                    core::cmp::Ordering::Equal => 0x0000,
                    core::cmp::Ordering::Greater => 0xFFFF,
                };
                entry[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
            }

            out.extend_from_slice(&entry);
        }
    }

    let mut entry = [0u8; 32];
    entry[0..11].copy_from_slice(&short);
    entry[11] = attributes;
    write_cluster_fields(&mut entry, first_cluster, size);
    out.extend_from_slice(&entry);

    out
}

/// Write `name` uppercased into an 8.3 field pair.
fn fill_short(field: &mut [u8; 11], name: &str) {
    let (stem, extension) = match name.rfind('.') {
        Some(dot) => (&name[..dot], &name[dot + 1..]),
        None => (name, ""),
    };

    field.fill(b' ');
    for (index, c) in stem.chars().take(8).enumerate() {
        field[index] = c.to_ascii_uppercase() as u8;
    }
    for (index, c) in extension.chars().take(3).enumerate() {
        field[8 + index] = c.to_ascii_uppercase() as u8;
    }
}

fn fill_short_slice(entry: &mut [u8; 32], name: &str) {
    let mut field = [b' '; 11];
    fill_short(&mut field, name);
    entry[0..11].copy_from_slice(&field);
}

/// Can this name be stored as 8.3 without losing information?
fn fits_short(name: &str) -> bool {
    if name.chars().any(|c| c.is_lowercase()) {
        return false;
    }

    let mut parts = name.splitn(2, '.');
    let stem = parts.next().unwrap_or("");
    let extension = parts.next().unwrap_or("");

    !stem.is_empty()
        && stem.len() <= 8
        && extension.len() <= 3
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || "_-~.".contains(c))
}

/// Derive an 8.3 name that no existing entry already uses.
fn unique_short_name(name: &str, directory: &[u8]) -> [u8; 11] {
    let (stem, extension) = match name.rfind('.') {
        Some(dot) => (&name[..dot], &name[dot + 1..]),
        None => (name, ""),
    };

    let sanitise = |text: &str| -> Vec<u8> {
        text.chars()
            .filter(|c| c.is_ascii_alphanumeric() || "_-".contains(*c))
            .map(|c| c.to_ascii_uppercase() as u8)
            .collect()
    };

    let stem_bytes = sanitise(stem);
    let extension_bytes = sanitise(extension);

    let mut candidate = [b' '; 11];
    for (index, byte) in extension_bytes.iter().take(3).enumerate() {
        candidate[8 + index] = *byte;
    }

    if fits_short(name) {
        for (index, byte) in stem_bytes.iter().take(8).enumerate() {
            candidate[index] = *byte;
        }
        if !short_name_taken(directory, &candidate) {
            return candidate;
        }
        candidate[0..8].fill(b' ');
    }

    for suffix in 1..=999u32 {
        let tail = alloc::format!("~{suffix}");
        let keep = 8 - tail.len();

        let mut name8 = [b' '; 8];
        for (index, byte) in stem_bytes.iter().take(keep).enumerate() {
            name8[index] = *byte;
        }
        for (index, byte) in tail.bytes().enumerate() {
            name8[keep + index] = byte;
        }

        candidate[0..8].copy_from_slice(&name8);
        if !short_name_taken(directory, &candidate) {
            return candidate;
        }
    }

    candidate
}

fn short_name_taken(directory: &[u8], candidate: &[u8; 11]) -> bool {
    directory.chunks_exact(32).any(|chunk| {
        chunk[0] != ENTRY_END
            && chunk[0] != ENTRY_DELETED
            && chunk[11] != ATTR_LONG_NAME
            && &chunk[0..11] == candidate.as_slice()
    })
}

/// The checksum tying long-name entries to their short entry.
fn short_name_checksum(short: &[u8; 11]) -> u8 {
    let mut sum = 0u8;
    for &byte in short {
        sum = ((sum & 1) << 7).wrapping_add(sum >> 1).wrapping_add(byte);
    }
    sum
}

fn is_root(path: &str) -> bool {
    path.split('/').all(|part| part.is_empty())
}

/// Byte 12 of a short entry carries two flags saying that the stored name,
/// which is always uppercase on disk, should be read back lowercased.
const CASE_STEM_LOWER: u8 = 0x08;
const CASE_EXTENSION_LOWER: u8 = 0x10;

/// Rebuild an 8.3 name, trimming the field padding and adding the dot back.
fn short_name(entry: &[u8]) -> String {
    let flags = entry[12];
    let stem = core::str::from_utf8(&entry[0..8]).unwrap_or("").trim_end();
    let extension = core::str::from_utf8(&entry[8..11]).unwrap_or("").trim_end();

    let mut name = String::new();
    if flags & CASE_STEM_LOWER != 0 {
        name.extend(stem.chars().map(|c| c.to_ascii_lowercase()));
    } else {
        name.push_str(stem);
    }

    if !extension.is_empty() {
        name.push('.');
        if flags & CASE_EXTENSION_LOWER != 0 {
            name.extend(extension.chars().map(|c| c.to_ascii_lowercase()));
        } else {
            name.push_str(extension);
        }
    }

    name
}

/// If a name differs from its 8.3 form only by case, it needs no long-name
/// entries: the short entry can hold it uppercased, with flags saying which
/// half to lowercase on the way back out. That is what other systems produce,
/// and it avoids littering the disk with `NAME~1` entries.
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
    if !usable(stem) || !usable(extension) {
        return None;
    }

    // Each half has to be entirely one case, since there is only one flag for
    // each and no way to record a mixture.
    let uniform = |text: &str| {
        !text.chars().any(|c| c.is_ascii_lowercase()) || !text.chars().any(|c| c.is_ascii_uppercase())
    };
    if !uniform(stem) || !uniform(extension) {
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

fn assemble_long_name(parts: &[(u8, [u16; 13])]) -> String {
    let mut units = Vec::new();

    for (_, part) in parts {
        for &unit in part {
            // 0 terminates the name; 0xFFFF is padding after it.
            if unit == 0 || unit == 0xFFFF {
                break;
            }
            units.push(unit);
        }
    }

    char::decode_utf16(units)
        .map(|result| result.unwrap_or('?'))
        .collect()
}

pub static FILESYSTEM: Mutex<Option<Fat32>> = Mutex::new(None);

/// Find the EFI system partition and mount it read-only.
pub fn init() -> Result<u64, &'static str> {
    let partition = gpt::find_esp()?;
    let filesystem = Fat32::mount(partition.first_lba)?;

    let sectors = partition.last_lba - partition.first_lba + 1;
    *FILESYSTEM.lock() = Some(filesystem);
    Ok(sectors)
}

/// Run `body` against the mounted disk filesystem, if there is one.
pub fn with<T>(body: impl FnOnce(&Fat32) -> T) -> Option<T> {
    x86_64::instructions::interrupts::without_interrupts(|| FILESYSTEM.lock().as_ref().map(body))
}

pub fn mounted() -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| FILESYSTEM.lock().is_some())
}
