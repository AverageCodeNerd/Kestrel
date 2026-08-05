//! Kernel heap: a first-fit free-list allocator.
//!
//! Free blocks store their own bookkeeping inside the free memory itself, so
//! the allocator needs no storage of its own. Adjacent frees are coalesced on
//! release, which keeps long-running fragmentation in check.

use core::alloc::{GlobalAlloc, Layout};
use core::mem::{align_of, size_of};
use core::ptr;

use spin::Mutex;
use x86_64::VirtAddr;

use crate::memory;

/// Somewhere in the higher half well clear of the direct map and the kernel.
pub const HEAP_START: u64 = 0xffff_9000_0000_0000;
/// Sized to hold user program images: the ramdisk keeps a copy of every
/// module, and loading one takes another while the ELF is parsed.
pub const HEAP_SIZE: u64 = 32 * 1024 * 1024;

/// A node in the free list, stored inside the free block it describes.
struct FreeBlock {
    size: usize,
    next: Option<&'static mut FreeBlock>,
}

impl FreeBlock {
    const fn new(size: usize) -> Self {
        Self { size, next: None }
    }

    fn start(&self) -> usize {
        self as *const Self as usize
    }

    fn end(&self) -> usize {
        self.start() + self.size
    }
}

pub struct Heap {
    /// Dummy head; `head.next` is the first real free block.
    head: FreeBlock,
}

impl Heap {
    pub const fn new() -> Self {
        Self {
            head: FreeBlock::new(0),
        }
    }

    /// # Safety
    /// The range must be mapped, writable, and otherwise unused.
    pub unsafe fn init(&mut self, start: usize, size: usize) {
        unsafe { self.push_free(start, size) }
    }

    /// Return a region to the free list, merging it with any neighbours.
    ///
    /// # Safety
    /// The region must be unused and large enough to hold a `FreeBlock`.
    unsafe fn push_free(&mut self, address: usize, size: usize) {
        debug_assert_eq!(align_up(address, align_of::<FreeBlock>()), address);
        debug_assert!(size >= size_of::<FreeBlock>());

        // Walk the list in address order so neighbours can be detected.
        let mut current = &mut self.head;
        while let Some(ref mut next) = current.next {
            if next.start() > address {
                break;
            }
            current = current.next.as_mut().unwrap();
        }

        let merges_into_previous =
            current.size != 0 && current.end() == address;
        let follower_start = current.next.as_ref().map(|n| n.start());

        if merges_into_previous {
            current.size += size;
            // Absorb the following block too if we just closed the gap.
            if follower_start == Some(current.end()) {
                let follower = current.next.take().unwrap();
                current.size += follower.size;
                current.next = follower.next.take();
            }
            return;
        }

        let mut block = FreeBlock::new(size);
        // Merge with the next block if this region runs right into it.
        if follower_start == Some(address + size) {
            let follower = current.next.take().unwrap();
            block.size += follower.size;
            block.next = follower.next.take();
        } else {
            block.next = current.next.take();
        }

        let ptr = address as *mut FreeBlock;
        unsafe {
            ptr.write(block);
            current.next = Some(&mut *ptr);
        }
    }

    /// Unlink a block big enough for `size`/`align`.
    ///
    /// Returns the block's bounds and the aligned start of the allocation, as
    /// plain addresses — deliberately not a reference, since the caller is
    /// about to write new free blocks over that same memory.
    fn take_block(&mut self, size: usize, align: usize) -> Option<(usize, usize, usize)> {
        let mut current = &mut self.head;

        while current.next.is_some() {
            let candidate = current.next.as_mut().unwrap();

            match Self::carve(candidate, size, align) {
                Some(start) => {
                    let bounds = (candidate.start(), candidate.end());
                    let next = candidate.next.take();
                    current.next = next;
                    return Some((bounds.0, bounds.1, start));
                }
                None => {
                    current = current.next.as_mut().unwrap();
                }
            }
        }

        None
    }

    /// Can this block satisfy the request? If so, where does it start?
    fn carve(block: &FreeBlock, size: usize, align: usize) -> Option<usize> {
        let start = align_up(block.start(), align);
        let end = start.checked_add(size)?;

        if end > block.end() {
            return None;
        }

        // Leftovers at either end have to be big enough to become free blocks
        // in their own right, otherwise they would be unreclaimable. The front
        // gap only appears for over-aligned requests.
        let front = start - block.start();
        if front > 0 && front < size_of::<FreeBlock>() {
            return None;
        }

        let back = block.end() - end;
        if back > 0 && back < size_of::<FreeBlock>() {
            return None;
        }

        Some(start)
    }

    /// Round a request up so every allocation can also serve as a free block.
    fn adjust(layout: Layout) -> (usize, usize) {
        let layout = layout
            .align_to(align_of::<FreeBlock>())
            .expect("alignment overflow")
            .pad_to_align();
        (layout.size().max(size_of::<FreeBlock>()), layout.align())
    }
}

fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

pub struct LockedHeap(Mutex<Heap>);

impl LockedHeap {
    pub const fn new() -> Self {
        Self(Mutex::new(Heap::new()))
    }
}

unsafe impl GlobalAlloc for LockedHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let (size, align) = Heap::adjust(layout);
        let mut heap = self.0.lock();

        match heap.take_block(size, align) {
            Some((block_start, block_end, start)) => {
                let front = start - block_start;
                let back = block_end - (start + size);

                unsafe {
                    if front > 0 {
                        heap.push_free(block_start, front);
                    }
                    if back > 0 {
                        heap.push_free(start + size, back);
                    }
                }

                start as *mut u8
            }
            None => ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let (size, _) = Heap::adjust(layout);
        unsafe { self.0.lock().push_free(ptr as usize, size) }
    }
}

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::new();

/// Map the heap region and hand it to the allocator.
pub fn init() -> Result<(), &'static str> {
    memory::map_anonymous(VirtAddr::new(HEAP_START), HEAP_SIZE)
        .map_err(|_| "could not map the heap")?;

    unsafe {
        ALLOCATOR
            .0
            .lock()
            .init(HEAP_START as usize, HEAP_SIZE as usize);
    }

    Ok(())
}
