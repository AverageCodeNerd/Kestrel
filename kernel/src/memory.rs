//! Physical frame allocation and virtual memory.
//!
//! Limine hands over a memory map and page tables that already cover RAM
//! through the direct map — but *not* device MMIO such as the APIC registers.
//! Mapping those is the first thing the kernel needs its own paging code for.

use core::sync::atomic::{AtomicU64, Ordering};

use limine::memmap::{Entry, MEMMAP_USABLE};
use spin::Mutex;
use x86_64::registers::control::Cr3;
use x86_64::registers::model_specific::{Efer, EferFlags};
use x86_64::structures::paging::mapper::MapToError;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};

use crate::hhdm;

/// Hands out 4 KiB frames from the regions Limine marked usable.
///
/// This is a one-way bump allocator: nothing is ever returned to it. That is
/// fine for boot-time structures and the heap, which are never freed.
pub struct BootFrameAllocator {
    entries: &'static [&'static Entry],
    region: usize,
    offset: u64,
    allocated: u64,
    /// Head of a list of returned frames.
    ///
    /// The link is stored *inside* each free frame, reached through the direct
    /// map. That matters because this allocator has to work before the heap
    /// exists — indeed it is what the heap is built from — so it cannot use a
    /// `Vec` to track anything.
    free_list: Option<PhysFrame>,
    free_count: u64,
}

impl BootFrameAllocator {
    /// # Safety
    /// The memory map must be the one Limine reported, and the caller must
    /// ensure only one allocator hands out these frames.
    pub unsafe fn new(entries: &'static [&'static Entry]) -> Self {
        Self {
            entries,
            region: 0,
            offset: 0,
            allocated: 0,
            free_list: None,
            free_count: 0,
        }
    }

    pub fn allocated_frames(&self) -> u64 {
        self.allocated
    }

    pub fn free_frames(&self) -> u64 {
        self.free_count
    }

    /// Return a frame for reuse.
    fn push_free(&mut self, frame: PhysFrame) {
        let next = self.free_list.map_or(0, |f| f.start_address().as_u64());

        unsafe {
            let link = hhdm::phys_to_virt(frame.start_address().as_u64()) as *mut u64;
            link.write(next);
        }

        self.free_list = Some(frame);
        self.free_count += 1;
        self.allocated -= 1;
    }

    fn pop_free(&mut self) -> Option<PhysFrame> {
        let frame = self.free_list?;

        let next = unsafe {
            let link = hhdm::phys_to_virt(frame.start_address().as_u64()) as *const u64;
            link.read()
        };

        self.free_list = if next == 0 {
            None
        } else {
            Some(PhysFrame::containing_address(PhysAddr::new(next)))
        };

        self.free_count -= 1;
        self.allocated += 1;
        Some(frame)
    }

    /// Total usable physical memory, in bytes.
    pub fn usable_bytes(&self) -> u64 {
        self.entries
            .iter()
            .filter(|e| e.type_ == MEMMAP_USABLE)
            .map(|e| e.length)
            .sum()
    }
}

unsafe impl FrameAllocator<Size4KiB> for BootFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        // Reuse before taking fresh ground.
        if let Some(frame) = self.pop_free() {
            return Some(frame);
        }

        while self.region < self.entries.len() {
            let entry = self.entries[self.region];

            if entry.type_ == MEMMAP_USABLE && self.offset + 4096 <= entry.length {
                let address = entry.base + self.offset;
                self.offset += 4096;
                self.allocated += 1;
                return Some(PhysFrame::containing_address(PhysAddr::new(address)));
            }

            // Region exhausted or unusable; move to the next one.
            self.region += 1;
            self.offset = 0;
        }
        None
    }
}

static MAPPER: Mutex<Option<OffsetPageTable<'static>>> = Mutex::new(None);
static FRAMES: Mutex<Option<BootFrameAllocator>> = Mutex::new(None);

/// Adopt the page tables Limine left active, reachable through the direct map.
///
/// # Safety
/// The direct map must already be initialised, and this must be called once.
unsafe fn adopt_page_tables() -> OffsetPageTable<'static> {
    let (frame, _flags) = Cr3::read();
    let table = hhdm::phys_to_virt(frame.start_address().as_u64()) as *mut PageTable;
    unsafe { OffsetPageTable::new(&mut *table, VirtAddr::new(hhdm::offset())) }
}

/// The page tables the kernel itself runs on. Every process address space
/// copies its higher half from here.
static KERNEL_PML4: AtomicU64 = AtomicU64::new(0);

/// # Safety
/// Must be called once, after the direct map is known.
pub unsafe fn init(entries: &'static [&'static Entry]) {
    unsafe {
        *MAPPER.lock() = Some(adopt_page_tables());
        *FRAMES.lock() = Some(BootFrameAllocator::new(entries));
    }

    // Permit the no-execute bit before anything tries to set it. Without this
    // the CPU treats bit 63 of a page table entry as reserved, and every
    // access to such a page faults regardless of what it was doing.
    unsafe {
        Efer::update(|flags| flags.insert(EferFlags::NO_EXECUTE_ENABLE));
    }

    let (frame, _) = Cr3::read();
    KERNEL_PML4.store(frame.start_address().as_u64(), Ordering::Relaxed);

    populate_higher_half();
}

/// Give every higher-half PML4 slot a page table up front.
///
/// A process address space is created by copying the kernel's higher-half
/// PML4 entries. If the kernel later allocated a *new* top-level entry — by
/// growing the heap into a fresh 512 GiB region, say — existing address spaces
/// would never see it, and the kernel would fault the moment a user process
/// was running. Pre-filling every slot means the entries are shared pointers
/// that can never change again.
fn populate_higher_half() {
    let mut mapper = MAPPER.lock();
    let mut frames = FRAMES.lock();
    let (Some(mapper), Some(frames)) = (mapper.as_mut(), frames.as_mut()) else {
        return;
    };

    let pml4 = mapper.level_4_table_mut();

    for index in 256..512 {
        if !pml4[index].is_unused() {
            continue;
        }

        let Some(frame) = frames.allocate_frame() else { return };
        unsafe {
            core::ptr::write_bytes(hhdm::phys_to_virt(frame.start_address().as_u64()), 0, 4096);
        }
        pml4[index].set_frame(frame, PageTableFlags::PRESENT | PageTableFlags::WRITABLE);
    }
}

/// An isolated set of page tables: one per user process.
///
/// The higher half is shared with the kernel, so system calls and interrupts
/// keep working without a page-table switch. The lower half is private.
pub struct AddressSpace {
    pml4: PhysFrame,
}

impl AddressSpace {
    pub fn new() -> Option<Self> {
        let mut frames = FRAMES.lock();
        let frame = frames.as_mut()?.allocate_frame()?;
        drop(frames);

        unsafe {
            let table = &mut *(hhdm::phys_to_virt(frame.start_address().as_u64()) as *mut PageTable);
            let kernel =
                &*(hhdm::phys_to_virt(KERNEL_PML4.load(Ordering::Relaxed)) as *const PageTable);

            table.zero();
            // Share the kernel's higher half; leave the lower half empty.
            for index in 256..512 {
                table[index] = kernel[index].clone();
            }
        }

        Some(Self { pml4: frame })
    }

    pub fn frame(&self) -> PhysFrame {
        self.pml4
    }

    /// # Safety
    /// The returned mapper aliases this address space's tables; do not hold
    /// two at once.
    unsafe fn mapper(&self) -> OffsetPageTable<'static> {
        let table = hhdm::phys_to_virt(self.pml4.start_address().as_u64()) as *mut PageTable;
        unsafe { OffsetPageTable::new(&mut *table, VirtAddr::new(hhdm::offset())) }
    }

    /// Map fresh zeroed pages into this address space, reachable from ring 3.
    ///
    /// Marking a page non-executable requires `EFER.NXE`; without it the CPU
    /// treats bit 63 as reserved and every access faults.
    pub fn map_user(
        &mut self,
        start: VirtAddr,
        size: u64,
        writable: bool,
        executable: bool,
    ) -> Result<(), MapToError<Size4KiB>> {
        let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
        if writable {
            flags |= PageTableFlags::WRITABLE;
        }
        if !executable {
            flags |= PageTableFlags::NO_EXECUTE;
        }
        // Intermediate tables must always allow writes; permission is decided
        // by the leaf entry.
        let parent = PageTableFlags::PRESENT
            | PageTableFlags::WRITABLE
            | PageTableFlags::USER_ACCESSIBLE;

        let mut mapper = unsafe { self.mapper() };
        let mut frames = FRAMES.lock();
        let frames = frames.as_mut().ok_or(MapToError::FrameAllocationFailed)?;

        let first = Page::<Size4KiB>::containing_address(start);
        let last = Page::<Size4KiB>::containing_address(start + size - 1u64);

        for page in Page::range_inclusive(first, last) {
            // Already mapped is fine: segments can share a page at the edges.
            if mapper.translate_page(page).is_ok() {
                continue;
            }

            let frame = frames
                .allocate_frame()
                .ok_or(MapToError::FrameAllocationFailed)?;

            unsafe {
                core::ptr::write_bytes(hhdm::phys_to_virt(frame.start_address().as_u64()), 0, 4096);
                mapper
                    .map_to_with_table_flags(page, frame, flags, parent, frames)?
                    .flush();
            }
        }

        Ok(())
    }

    /// Copy bytes into this address space through the direct map, without
    /// switching to it.
    pub fn write(&self, mut address: VirtAddr, data: &[u8]) -> Result<(), &'static str> {
        let mapper = unsafe { self.mapper() };
        let mut remaining = data;

        while !remaining.is_empty() {
            let page = Page::<Size4KiB>::containing_address(address);
            let frame = mapper
                .translate_page(page)
                .map_err(|_| "target address is not mapped")?;

            let offset = (address.as_u64() - page.start_address().as_u64()) as usize;
            let chunk = remaining.len().min(4096 - offset);

            unsafe {
                let dest = hhdm::phys_to_virt(frame.start_address().as_u64()).add(offset);
                core::ptr::copy_nonoverlapping(remaining.as_ptr(), dest, chunk);
            }

            address += chunk as u64;
            remaining = &remaining[chunk..];
        }

        Ok(())
    }
}

/// Hand a frame back to the allocator.
pub fn free_frame(frame: PhysFrame) {
    if let Some(frames) = FRAMES.lock().as_mut() {
        frames.push_free(frame);
    }
}

pub fn free_frames() -> u64 {
    FRAMES.lock().as_ref().map_or(0, |f| f.free_frames())
}

impl Drop for AddressSpace {
    /// Release every frame this address space owns.
    ///
    /// Only the lower half is walked. The upper half is the kernel's, shared
    /// by every address space — freeing those tables would pull the kernel out
    /// from under everything still running.
    fn drop(&mut self) {
        unsafe fn free_level(table_frame: PhysFrame, level: u8, entries: core::ops::Range<usize>) {
            let table = unsafe {
                &*(hhdm::phys_to_virt(table_frame.start_address().as_u64()) as *const PageTable)
            };

            for index in entries {
                let entry = &table[index];
                if entry.is_unused() || !entry.flags().contains(PageTableFlags::PRESENT) {
                    continue;
                }

                let Ok(child) = entry.frame() else { continue };

                // A huge page is data, not another level of table.
                let huge = entry.flags().contains(PageTableFlags::HUGE_PAGE);
                if level > 1 && !huge {
                    unsafe { free_level(child, level - 1, 0..512) };
                }

                free_frame(child);
            }
        }

        // Levels: 4 = PML4, walking down to the leaf page tables at level 1.
        unsafe { free_level(self.pml4, 4, 0..256) };
        free_frame(self.pml4);
    }
}

/// Switch to a process's page tables, or back to the kernel's.
///
/// # Safety
/// The kernel must be mapped in the target address space, which
/// `AddressSpace::new` guarantees.
pub unsafe fn activate(space: Option<PhysFrame>) {
    let frame = match space {
        Some(frame) => frame,
        None => PhysFrame::containing_address(PhysAddr::new(KERNEL_PML4.load(Ordering::Relaxed))),
    };

    let (current, flags) = Cr3::read();
    if current != frame {
        unsafe { Cr3::write(frame, flags) };
    }
}

/// Take a zeroed physical frame, for things that need a known physical
/// address — DMA buffers and device command structures.
pub fn allocate_zeroed_frame() -> Option<PhysFrame> {
    let mut frames = FRAMES.lock();
    let frame = frames.as_mut()?.allocate_frame()?;

    unsafe {
        core::ptr::write_bytes(hhdm::phys_to_virt(frame.start_address().as_u64()), 0, 4096);
    }

    Some(frame)
}

pub fn usable_bytes() -> u64 {
    FRAMES.lock().as_ref().map_or(0, |f| f.usable_bytes())
}

pub fn allocated_frames() -> u64 {
    FRAMES.lock().as_ref().map_or(0, |f| f.allocated_frames())
}

/// Map `size` bytes of physical memory at its direct-map address, so that
/// `hhdm::phys_to_virt` keeps working for it.
///
/// Used for device registers, which are mapped uncached — caching APIC writes
/// would be a subtle disaster.
pub fn map_mmio(phys: u64, size: u64) -> Result<(), MapToError<Size4KiB>> {
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::NO_CACHE
        | PageTableFlags::WRITE_THROUGH;

    let mut mapper = MAPPER.lock();
    let mut frames = FRAMES.lock();
    let (Some(mapper), Some(frames)) = (mapper.as_mut(), frames.as_mut()) else {
        return Err(MapToError::FrameAllocationFailed);
    };

    let start = hhdm::offset() + phys;
    let first = Page::<Size4KiB>::containing_address(VirtAddr::new(start));
    let last = Page::<Size4KiB>::containing_address(VirtAddr::new(start + size - 1));

    for page in Page::range_inclusive(first, last) {
        let offset = page.start_address().as_u64() - hhdm::offset();
        let frame = PhysFrame::containing_address(PhysAddr::new(offset));

        match unsafe { mapper.map_to(page, frame, flags, frames) } {
            Ok(flush) => flush.flush(),
            // Already mapped is not a failure — the direct map may cover part
            // of the range already.
            Err(MapToError::PageAlreadyMapped(_)) => {}
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

/// Map a fresh, zeroed, writable region of `size` bytes at `start`.
pub fn map_anonymous(start: VirtAddr, size: u64) -> Result<(), MapToError<Size4KiB>> {
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;

    let mut mapper = MAPPER.lock();
    let mut frames = FRAMES.lock();
    let (Some(mapper), Some(frames)) = (mapper.as_mut(), frames.as_mut()) else {
        return Err(MapToError::FrameAllocationFailed);
    };

    let first = Page::<Size4KiB>::containing_address(start);
    let last = Page::<Size4KiB>::containing_address(start + size - 1u64);

    for page in Page::range_inclusive(first, last) {
        let frame = frames
            .allocate_frame()
            .ok_or(MapToError::FrameAllocationFailed)?;

        // Frames from the bootloader's usable regions hold whatever the last
        // owner left behind; the heap must not start life full of garbage.
        unsafe {
            core::ptr::write_bytes(hhdm::phys_to_virt(frame.start_address().as_u64()), 0, 4096);
            mapper.map_to(page, frame, flags, frames)?.flush();
        }
    }

    Ok(())
}
