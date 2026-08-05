//! Per-CPU Global Descriptor Tables and Task State Segments.
//!
//! Limine leaves us on its own GDT, which we must not keep using — it can be
//! reclaimed once we're done with the bootloader's memory. We also need a TSS
//! so that faults which may have wrecked the kernel stack (double fault, page
//! fault) can switch to a private stack via the Interrupt Stack Table.
//!
//! All of it is per-CPU. The TSS has to be, because `rsp0` says where *this*
//! core lands when it enters the kernel from ring 3, and two cores sharing that
//! would trample each other's saved state. The GDT follows because it holds the
//! descriptor naming the TSS.
//!
//! These live in static arrays rather than on the heap because the boot CPU
//! loads its tables long before there is a heap to allocate from.

use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{CS, DS, ES, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;

/// Cores beyond this are ignored. Raising it costs stack space in `.bss`.
pub const MAX_CPUS: usize = 8;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;
pub const PAGE_FAULT_IST_INDEX: u16 = 1;

const STACK_SIZE: usize = 4096 * 5;

#[repr(align(16))]
struct Stack(#[allow(dead_code)] [u8; STACK_SIZE]);

impl Stack {
    const fn new() -> Self {
        Self([0; STACK_SIZE])
    }
}

/// Everything one core needs before it can take a fault or enter ring 3.
struct CpuTables {
    gdt: GlobalDescriptorTable,
    tss: TaskStateSegment,
    double_fault_stack: Stack,
    page_fault_stack: Stack,
    /// Where the CPU switches on entry from ring 3, until a task sets its own.
    privilege_stack: Stack,
}

impl CpuTables {
    const fn new() -> Self {
        Self {
            gdt: GlobalDescriptorTable::new(),
            tss: TaskStateSegment::new(),
            double_fault_stack: Stack::new(),
            page_fault_stack: Stack::new(),
            privilege_stack: Stack::new(),
        }
    }
}

static mut CPUS: [CpuTables; MAX_CPUS] = [const { CpuTables::new() }; MAX_CPUS];

/// Stacks grow downwards, so the IST entry wants the *end* of the array.
fn stack_top(stack: &Stack) -> VirtAddr {
    VirtAddr::from_ptr(stack) + STACK_SIZE as u64
}

fn tables(cpu: usize) -> &'static mut CpuTables {
    // Each core only ever touches its own entry.
    unsafe { &mut (*(&raw mut CPUS))[cpu] }
}

#[derive(Clone, Copy)]
pub struct Selectors {
    pub kernel_code: SegmentSelector,
    pub kernel_data: SegmentSelector,
    pub user_code: SegmentSelector,
    pub user_data: SegmentSelector,
    /// Loaded once at startup; kept for completeness.
    #[allow(dead_code)]
    pub tss: SegmentSelector,
}

/// The selectors are identical on every core, since each GDT is built the same
/// way; only the TSS descriptor's contents differ.
static mut SELECTORS: Option<Selectors> = None;

pub fn selectors() -> Selectors {
    unsafe { (*(&raw const SELECTORS)).expect("the GDT has not been initialised") }
}

/// Point this core at the stack it should switch to on entry from ring 3.
///
/// Called on every context switch, so an interrupt arriving while a user
/// program runs lands on that program's own kernel stack.
pub fn set_kernel_stack(cpu: usize, top: VirtAddr) {
    tables(cpu).tss.privilege_stack_table[0] = top;
}

/// Top of a core's boot ring-0 stack, used until the first task sets its own.
pub fn kernel_stack_top(cpu: usize) -> VirtAddr {
    tables(cpu).tss.privilege_stack_table[0]
}

/// Build and load this core's descriptor tables.
///
/// # Safety
/// `cpu` must be this core's index, and used by no other core.
pub unsafe fn init(cpu: usize) {
    let tables = tables(cpu);

    tables.tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] =
        stack_top(&tables.double_fault_stack);
    tables.tss.interrupt_stack_table[PAGE_FAULT_IST_INDEX as usize] =
        stack_top(&tables.page_fault_stack);
    tables.tss.privilege_stack_table[0] = stack_top(&tables.privilege_stack);

    // The descriptor records the TSS's address, which is fixed, so it does not
    // matter that the contents above were only just written.
    let tss_ref: &'static TaskStateSegment = unsafe { &*(&raw const tables.tss) };

    let kernel_code = tables.gdt.append(Descriptor::kernel_code_segment());
    let kernel_data = tables.gdt.append(Descriptor::kernel_data_segment());
    // SYSRET derives the user selectors by offset from one value, which only
    // works if user data sits directly below user code.
    let user_data = tables.gdt.append(Descriptor::user_data_segment());
    let user_code = tables.gdt.append(Descriptor::user_code_segment());
    let tss = tables.gdt.append(Descriptor::tss_segment(tss_ref));

    let selectors = Selectors {
        kernel_code,
        kernel_data,
        user_code,
        user_data,
        tss,
    };
    unsafe { *(&raw mut SELECTORS) = Some(selectors) };

    // `load` wants a 'static reference; this table lives for the whole run.
    let gdt: &'static GlobalDescriptorTable = unsafe { &*(&raw const tables.gdt) };
    gdt.load();

    unsafe {
        // Reloading CS is what actually commits us to the new code descriptor.
        CS::set_reg(kernel_code);
        SS::set_reg(kernel_data);
        DS::set_reg(kernel_data);
        ES::set_reg(kernel_data);
        load_tss(tss);
    }
}
