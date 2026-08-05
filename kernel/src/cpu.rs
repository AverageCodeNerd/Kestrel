//! Per-CPU identity and bookkeeping.
//!
//! A core can always work out which one it is by reading its local APIC ID —
//! that register is physically per-CPU — so no thread-local machinery is
//! needed just to answer "who am I".

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

pub use crate::gdt::MAX_CPUS;

/// LAPIC ID of each known core, indexed by CPU number.
const UNKNOWN: u32 = u32::MAX;
#[allow(clippy::declare_interior_mutable_const)]
const NO_ID: AtomicU32 = AtomicU32::new(UNKNOWN);
static LAPIC_IDS: [AtomicU32; MAX_CPUS] = [NO_ID; MAX_CPUS];

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU64 = AtomicU64::new(0);
static TICKS: [AtomicU64; MAX_CPUS] = [ZERO; MAX_CPUS];

static COUNT: AtomicUsize = AtomicUsize::new(0);
static ONLINE: AtomicUsize = AtomicUsize::new(0);

/// Record a core's LAPIC ID against its index, before it starts.
pub fn register(index: usize, lapic_id: u32) {
    if index >= MAX_CPUS {
        return;
    }
    LAPIC_IDS[index].store(lapic_id, Ordering::Release);
    COUNT.fetch_max(index + 1, Ordering::AcqRel);
}

/// Which core is this? Safe to call from an interrupt handler.
pub fn index() -> usize {
    let lapic_id = crate::apic::local_apic_id() as u32;

    for (index, known) in LAPIC_IDS.iter().enumerate() {
        if known.load(Ordering::Acquire) == lapic_id {
            return index;
        }
    }

    // Before registration, or a core we never enumerated: treat it as the
    // boot processor rather than indexing out of range.
    0
}

pub fn mark_online() {
    ONLINE.fetch_add(1, Ordering::AcqRel);
}

pub fn online() -> usize {
    ONLINE.load(Ordering::Acquire)
}

pub fn count() -> usize {
    COUNT.load(Ordering::Acquire)
}

pub fn tick(index: usize) {
    if index < MAX_CPUS {
        TICKS[index].fetch_add(1, Ordering::Relaxed);
    }
}

pub fn ticks(index: usize) -> u64 {
    if index < MAX_CPUS {
        TICKS[index].load(Ordering::Relaxed)
    } else {
        0
    }
}

/// Per-core scratch the syscall entry stub reaches through GS.
///
/// The field order is load-bearing: the stub addresses these by fixed offset.
#[repr(C)]
pub struct CpuData {
    /// Offset 0: the stack this core enters the kernel on.
    pub kernel_stack: u64,
    /// Offset 8: where the user's stack pointer is parked mid-syscall.
    pub user_stack: u64,
}

impl CpuData {
    const fn new() -> Self {
        Self {
            kernel_stack: 0,
            user_stack: 0,
        }
    }
}

static mut CPU_DATA: [CpuData; MAX_CPUS] = [const { CpuData::new() }; MAX_CPUS];

/// MSR holding the GS base that `swapgs` swaps *in*.
const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;
const IA32_GS_BASE: u32 = 0xC000_0101;

/// Point this core's shadow GS base at its own data.
///
/// The kernel pointer lives in the *shadow* base, not the active one, so that
/// a user program cannot reach it — and, more importantly, cannot break the
/// syscall path by loading a GS selector, which would otherwise zero the base
/// the stub depends on. `swapgs` exchanges the two on kernel entry.
///
/// # Safety
/// `index` must be this core's own index.
pub unsafe fn init_gs(index: usize) {
    let pointer = unsafe { &raw const CPU_DATA[index] } as u64;

    unsafe {
        x86_64::registers::model_specific::Msr::new(IA32_KERNEL_GS_BASE).write(pointer);
        // Ring 3 sees a zero base until it enters the kernel.
        x86_64::registers::model_specific::Msr::new(IA32_GS_BASE).write(0);
    }
}

/// Enable the CPU features the kernel relies on.
///
/// EFER is per-core, so every core has to do this for itself. Miss it on one
/// core and any page marked no-execute faults there with a reserved-bit error,
/// because without NXE bit 63 is not a permission bit at all.
pub fn enable_features() {
    use x86_64::registers::model_specific::{Efer, EferFlags};

    unsafe {
        Efer::update(|flags| {
            flags.insert(EferFlags::NO_EXECUTE_ENABLE);
            flags.insert(EferFlags::SYSTEM_CALL_EXTENSIONS);
        });
    }
}

/// Set the stack this core will enter the kernel on for system calls.
pub fn set_syscall_stack(index: usize, top: u64) {
    if index < MAX_CPUS {
        unsafe { (*(&raw mut CPU_DATA))[index].kernel_stack = top };
    }
}

pub fn lapic_id(index: usize) -> u32 {
    if index < MAX_CPUS {
        LAPIC_IDS[index].load(Ordering::Acquire)
    } else {
        UNKNOWN
    }
}
