//! A loader for static ELF64 executables.
//!
//! Only what is needed to start a process: validate the header, then map and
//! populate each PT_LOAD segment in the target address space. No dynamic
//! linking, no relocations — user programs are linked static and
//! position-dependent.

use x86_64::VirtAddr;

use crate::memory::AddressSpace;

const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];
const CLASS_64: u8 = 2;
const LITTLE_ENDIAN: u8 = 1;
const TYPE_EXECUTABLE: u16 = 2;
const MACHINE_X86_64: u16 = 62;

const PT_LOAD: u32 = 1;

const PF_EXECUTE: u32 = 1;
const PF_WRITE: u32 = 2;

#[derive(Debug)]
pub enum Error {
    TooSmall,
    NotAnElf,
    Not64Bit,
    WrongEndianness,
    NotExecutable,
    WrongArchitecture,
    SegmentOutOfRange,
    /// A segment wanted to live in the kernel's half of the address space.
    SegmentNotInUserSpace,
    MappingFailed,
}

impl Error {
    pub fn as_str(&self) -> &'static str {
        match self {
            Error::TooSmall => "file is too small to be an ELF",
            Error::NotAnElf => "not an ELF file",
            Error::Not64Bit => "not a 64-bit ELF",
            Error::WrongEndianness => "not little-endian",
            Error::NotExecutable => "not an executable",
            Error::WrongArchitecture => "not an x86-64 binary",
            Error::SegmentOutOfRange => "a segment extends past the end of the file",
            Error::SegmentNotInUserSpace => "a segment is outside user space",
            Error::MappingFailed => "could not map a segment",
        }
    }
}

fn read_u16(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn read_u64(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

/// User space is the lower half; anything above belongs to the kernel.
const USER_LIMIT: u64 = 0x0000_8000_0000_0000;

pub struct Loaded {
    pub entry: VirtAddr,
    /// Highest address any segment occupies, so a heap or stack can be placed
    /// clear of the image.
    pub end: VirtAddr,
}

/// Map `data`'s loadable segments into `space` and report its entry point.
pub fn load(data: &[u8], space: &mut AddressSpace) -> Result<Loaded, Error> {
    if data.len() < 64 {
        return Err(Error::TooSmall);
    }
    if data[0..4] != ELF_MAGIC {
        return Err(Error::NotAnElf);
    }
    if data[4] != CLASS_64 {
        return Err(Error::Not64Bit);
    }
    if data[5] != LITTLE_ENDIAN {
        return Err(Error::WrongEndianness);
    }
    if read_u16(data, 16) != TYPE_EXECUTABLE {
        return Err(Error::NotExecutable);
    }
    if read_u16(data, 18) != MACHINE_X86_64 {
        return Err(Error::WrongArchitecture);
    }

    let entry = read_u64(data, 24);
    let phoff = read_u64(data, 32) as usize;
    let phentsize = read_u16(data, 54) as usize;
    let phnum = read_u16(data, 56) as usize;

    if phoff + phnum * phentsize > data.len() {
        return Err(Error::SegmentOutOfRange);
    }

    let mut highest = 0u64;

    for index in 0..phnum {
        let header = phoff + index * phentsize;

        if read_u32(data, header) != PT_LOAD {
            continue;
        }

        let flags = read_u32(data, header + 4);
        let offset = read_u64(data, header + 8) as usize;
        let vaddr = read_u64(data, header + 16);
        let filesz = read_u64(data, header + 32) as usize;
        let memsz = read_u64(data, header + 40);

        if memsz == 0 {
            continue;
        }
        if offset + filesz > data.len() {
            return Err(Error::SegmentOutOfRange);
        }
        // A segment claiming a kernel address would otherwise let a crafted
        // binary write over the kernel through the loader.
        if vaddr >= USER_LIMIT || vaddr + memsz > USER_LIMIT {
            return Err(Error::SegmentNotInUserSpace);
        }

        // Apply the segment's real permissions: code ends up read-execute and
        // data read-write-noexecute, so a program cannot rewrite its own code
        // or run its own data.
        //
        // Nothing needs to be temporarily writable, because the contents are
        // copied through the direct map rather than through this mapping.
        // Pages arrive zeroed, so the tail beyond filesz — .bss — is already
        // correct.
        //
        // This assumes segments do not share a page, which the linker script
        // guarantees by aligning each to 4 KiB; if two did, whichever was
        // mapped first would win.
        space
            .map_user(
                VirtAddr::new(vaddr),
                memsz,
                flags & PF_WRITE != 0,
                flags & PF_EXECUTE != 0,
            )
            .map_err(|_| Error::MappingFailed)?;

        if filesz > 0 {
            space
                .write(VirtAddr::new(vaddr), &data[offset..offset + filesz])
                .map_err(|_| Error::MappingFailed)?;
        }

        highest = highest.max(vaddr + memsz);
    }

    if entry >= USER_LIMIT {
        return Err(Error::SegmentNotInUserSpace);
    }

    Ok(Loaded {
        entry: VirtAddr::new(entry),
        end: VirtAddr::new(highest),
    })
}
