//! Turning an ELF file into a running user process.

use x86_64::VirtAddr;

use crate::memory::AddressSpace;
use crate::task::{self, Start};
use crate::{elf, println};

/// The user stack sits well above any plausible program image and well below
/// the kernel half.
const USER_STACK_TOP: u64 = 0x0000_0000_1000_0000; // 256 MiB
const USER_STACK_SIZE: u64 = 64 * 1024;

pub enum Error {
    NoAddressSpace,
    Elf(elf::Error),
    StackMappingFailed,
}

impl Error {
    pub fn as_str(&self) -> &'static str {
        match self {
            Error::NoAddressSpace => "out of memory for a new address space",
            Error::Elf(e) => e.as_str(),
            Error::StackMappingFailed => "could not map the user stack",
        }
    }
}

/// Load an ELF image into a fresh address space and queue it to run.
///
/// The process runs concurrently with everything else; this returns as soon as
/// it is on the run queue.
pub fn spawn(name: &str, image: &[u8]) -> Result<u64, Error> {
    let mut space = AddressSpace::new().ok_or(Error::NoAddressSpace)?;

    let loaded = elf::load(image, &mut space).map_err(Error::Elf)?;

    // Writable, and emphatically not executable: a stack that could be
    // executed is the classic way a buffer overflow becomes code execution.
    space
        .map_user(
            VirtAddr::new(USER_STACK_TOP - USER_STACK_SIZE),
            USER_STACK_SIZE,
            true,
            false,
        )
        .map_err(|_| Error::StackMappingFailed)?;

    // The System V ABI wants a 16-byte aligned stack pointer at entry.
    let stack = VirtAddr::new(USER_STACK_TOP - 16);

    println!(
        "exec: {name} entry {:#x}, image ends at {:#x}",
        loaded.entry.as_u64(),
        loaded.end.as_u64()
    );

    Ok(task::spawn_task(
        name,
        Start::User {
            entry: loaded.entry,
            stack,
        },
        Some(space),
    ))
}
