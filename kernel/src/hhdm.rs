//! The Higher-Half Direct Map.
//!
//! Limine maps all of physical memory (at least the first 4 GiB, which covers
//! the APIC MMIO region) at a fixed virtual offset. Until the kernel manages
//! its own page tables, this is how it reaches any physical address.

use core::sync::atomic::{AtomicU64, Ordering};

static OFFSET: AtomicU64 = AtomicU64::new(0);

pub fn init(offset: u64) {
    OFFSET.store(offset, Ordering::Relaxed);
}

pub fn offset() -> u64 {
    OFFSET.load(Ordering::Relaxed)
}

/// Translate a physical address into its direct-map virtual address.
pub fn phys_to_virt(phys: u64) -> *mut u8 {
    (offset() + phys) as *mut u8
}
