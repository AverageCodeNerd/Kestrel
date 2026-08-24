//! Kestrel — a small x86-64 kernel booted by Limine.

#![no_std]
#![no_main]
// Exception handlers need the interrupt calling convention, which the CPU
// defines and Rust has not stabilised.
#![feature(abi_x86_interrupt)]

extern crate alloc;

mod acpi;
mod ahci;
mod allocator;
mod apic;
mod console;
mod clock;
mod cpu;
mod desktop;
mod elf;
mod fat;
mod font;
mod gpt;
mod gdt;
mod hhdm;
mod interrupts;
mod keyboard;
mod memory;
mod menu;
mod mouse;
mod net;
mod notify;
mod pci;
mod pic;
mod power;
mod pit;
mod print;
mod process;
mod serial;
mod settings;
mod shell;
mod smp;
mod store;
mod syscall;
mod task;
mod terminal;
mod theme;
mod update;
mod usermode;
mod vfs;

use limine::request::{
    BootloaderInfoRequest, ExecutableAddressRequest, FramebufferRequest, HhdmRequest, MemmapRequest,
    ModulesRequest, RsdpRequest,
};
use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker};

use crate::console::{CONSOLE, Console};
use crate::serial::SERIAL;

/// The release this kernel was built from, taken from `kernel/Cargo.toml` so
/// there is exactly one place to bump it. The banner, the `version` command
/// and the release script all read it from here.
pub const VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"), " beta");

/// A name for this release, so it is something to talk about rather than a
/// number to look up. This one is about being able to make things on it:
/// install programs, draw, and write your own.
pub const CODENAME: &str = "Make Things";

/// Limine only scans for requests between these two markers, which lets the
/// linker place everything else freely.
#[used]
#[unsafe(link_section = ".limine_requests_start_marker")]
static REQUESTS_START: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[unsafe(link_section = ".limine_requests_end_marker")]
static REQUESTS_END: RequestsEndMarker = RequestsEndMarker::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static BASE_REVISION: BaseRevision = BaseRevision::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static FRAMEBUFFER: FramebufferRequest = FramebufferRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static BOOTLOADER_INFO: BootloaderInfoRequest = BootloaderInfoRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static MEMMAP: MemmapRequest = MemmapRequest::new();

/// Offset at which Limine direct-maps all of physical memory. Everything the
/// memory manager does later hangs off this.
#[used]
#[unsafe(link_section = ".limine_requests")]
static HHDM: HhdmRequest = HhdmRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static EXECUTABLE_ADDRESS: ExecutableAddressRequest = ExecutableAddressRequest::new();

/// Root of the ACPI tables, where the APICs are described. At base revision 6
/// Limine reports this already translated into the direct map.
#[used]
#[unsafe(link_section = ".limine_requests")]
static RSDP: RsdpRequest = RsdpRequest::new();

/// User programs, loaded into memory by the bootloader. Until there is a disk
/// driver, this is where executables come from.
#[used]
#[unsafe(link_section = ".limine_requests")]
static MODULES: ModulesRequest = ModulesRequest::new();

/// Entry point. Named in `linker.ld` via `ENTRY(kmain)`.
#[unsafe(no_mangle)]
pub extern "C" fn kmain() -> ! {
    SERIAL.lock().init();

    // If the bootloader doesn't speak our base revision, none of the responses
    // below can be trusted.
    if !BASE_REVISION.is_supported() {
        halt();
    }

    // The direct map underpins every physical access below, so record it
    // before anything tries to reach hardware.
    if let Some(response) = HHDM.response() {
        hhdm::init(response.offset);
    }

    if let Some(response) = FRAMEBUFFER.response() {
        if let Some(fb) = response.framebuffers().first() {
            // Before the console, because how large a character is depends on
            // how large the screen is, and both of them ask `theme` that.
            theme::note_screen(fb.width as usize, fb.height as usize);

            let console = unsafe { Console::new(fb) };
            *CONSOLE.lock() = Some(console);
        }
    }

    banner();
    report_boot_info();

    println!();
    // The boot processor is always CPU 0.
    unsafe {
        gdt::init(0);
        cpu::init_gs(0);
    }
    println!("gdt          : loaded, TSS active");
    interrupts::init();
    println!("idt          : loaded, CPU exceptions handled");

    usermode::init();
    println!("syscall      : SYSCALL/SYSRET enabled");

    // Prove the IDT works: this traps into the breakpoint handler and comes
    // back. If the table were wrong we'd triple fault instead.
    x86_64::instructions::interrupts::int3();
    println!("int3         : returned cleanly");

    start_memory();
    start_disk();
    start_interrupts();

    task::init_cpu(0);
    println!("scheduler    : ready");

    // Only after interrupts and the heap are up: each core needs the shared
    // IDT and the calibrated timer rate before it can run.
    smp::init();

    // After the timer, whose ticks carry the time forward between readings.
    match clock::init() {
        Some(now) => println!(
            "clock        : {} {} {} {:02}:{:02}:{:02} UTC",
            clock::weekday(&now),
            now.day,
            clock::MONTHS[(now.month.clamp(1, 12) - 1) as usize],
            now.hour,
            now.minute,
            now.second
        ),
        None => println!("clock        : no RTC - times will read as unknown"),
    }

    vfs::init();
    println!("ramdisk      : mounted at /");

    load_modules();

    // After the disk, since that is where settings live, and before the shell,
    // so the desktop is already the user's own the first time it is opened.
    if let Some(skipped) = theme::load_from_disk() {
        println!("theme        : loaded from {}", theme::CONFIG_PATH);
        for complaint in skipped {
            println!("             : {complaint}");
        }
    }

    start_network();

    println!();
    shell::run();
}

/// Copy the bootloader's modules into /bin, so programs can be `exec`ed by
/// path. A disk driver would replace this wholesale.
fn load_modules() {
    let Some(response) = MODULES.response() else {
        println!("modules      : none");
        return;
    };

    let modules = response.modules();
    if modules.is_empty() {
        println!("modules      : none");
        return;
    }

    vfs::with(|fs| fs.mkdir(store::REPOSITORY).ok());

    let mut count = 0usize;
    for module in modules {
        // Use the basename of the path Limine reports.
        let path = module.path();
        let name = path.rsplit('/').next().unwrap_or(path);
        let target = alloc::format!("{}/{name}", store::REPOSITORY);

        match vfs::with(|fs| fs.write(&target, module.data())) {
            Some(Ok(())) => count += 1,
            Some(Err(e)) => println!("repo         : {target} failed - {}", e.as_str()),
            None => {}
        }
    }

    // These are offered, not installed: the catalogue is one of them, so it is
    // not itself a package.
    println!(
        "repo         : {} packages available ('store' to see them)",
        count.saturating_sub(1)
    );
}

/// Bring up the network card and announce ourselves.
fn start_network() {
    match net::init() {
        Ok(mac) => {
            println!(
                "network      : e1000 {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
            );

            let (ip, gateway) = net::config(|c| (c.ip, c.gateway)).unwrap();
            println!("address      : {ip}, gateway {gateway}");

            // Nothing can be sent before the link is up — the card simply
            // never completes the descriptor — so wait for it rather than
            // firing the first frame into a cable that is not plugged in yet.
            match net::e1000::await_link() {
                Some(0) => {}
                Some(ms) => println!("link         : up after {ms} ms"),
                None => println!("link         : down; the network will not work"),
            }

            // Resolving the gateway both populates the cache and proves the
            // transmit path works: a reply can only come back if the request
            // actually left the card.
            if let Err(e) = net::arp::request(gateway) {
                println!("arp          : {e}");
            }
        }
        Err(e) => println!("network      : {e}"),
    }
}

/// Take ownership of physical memory and bring up the heap.
fn start_memory() {
    let Some(memmap) = MEMMAP.response() else {
        println!("memory       : no memory map; cannot continue");
        halt();
    };

    unsafe { memory::init(memmap.entries()) };
    println!(
        "frames       : {} MiB usable",
        memory::usable_bytes() / (1024 * 1024)
    );

    match allocator::init() {
        Ok(()) => println!(
            "heap         : {} KiB at {:#x}",
            allocator::HEAP_SIZE / 1024,
            allocator::HEAP_START
        ),
        Err(e) => {
            println!("heap         : FAILED - {e}");
            halt();
        }
    }

    heap_smoke_test();
}

/// Exercise the allocator before anything depends on it. A broken heap that
/// only shows up later is far harder to diagnose than one that fails here.
fn heap_smoke_test() {
    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::vec::Vec;

    let boxed = Box::new(0xC0FFEEu64);
    assert_eq!(*boxed, 0xC0FFEE);

    // Grow a vector well past its initial capacity so it reallocates, which
    // exercises alloc/dealloc and the free-list coalescing.
    let mut numbers: Vec<u64> = Vec::new();
    for i in 0..2048 {
        numbers.push(i);
    }
    assert_eq!(numbers.iter().sum::<u64>(), 2047 * 2048 / 2);

    // Repeated alloc/free of varying sizes: if coalescing were broken, the
    // heap would fragment until one of these returned null and panicked.
    for round in 0..64 {
        let scratch: Vec<u8> = Vec::with_capacity(round * 32);
        drop(scratch);
    }

    let text = String::from("heap works");
    assert_eq!(text.len(), 10);

    println!("             : {text}, {} frames used", memory::allocated_frames());
}

/// Find and initialise the boot disk.
fn start_disk() {
    match ahci::init() {
        Ok((vendor, device)) => println!("ahci         : controller {vendor:04x}:{device:04x}"),
        Err(e) => {
            println!("ahci         : {e}");
            return;
        }
    }

    // Prove the DMA path works before trusting it: LBA 1 of a GPT disk is the
    // partition table header, which starts with a known signature.
    let mut sector = [0u8; ahci::SECTOR_SIZE];
    match ahci::read(1, 1, &mut sector) {
        Ok(()) => {
            let signature = core::str::from_utf8(&sector[0..8]).unwrap_or("<not text>");
            println!("disk         : read LBA 1, signature {signature:?}");
        }
        Err(e) => {
            println!("disk         : read failed - {e}");
            return;
        }
    }

    match fat::init() {
        Ok(sectors) => println!(
            "fat32        : ESP mounted at {} ({} MiB)",
            vfs::DISK_MOUNT,
            sectors / 2048
        ),
        Err(e) => println!("fat32        : {e}"),
    }
}

/// Mask the legacy PIC, bring up the APICs, and unmask interrupts.
fn start_interrupts() {
    pic::disable();
    println!("pic          : remapped and masked");

    let rsdp = match RSDP.response() {
        Some(response) => response.address as *const u8,
        None => {
            println!("acpi         : no RSDP; running without interrupts");
            return;
        }
    };

    let Some(tables) = (unsafe { acpi::parse(rsdp) }) else {
        println!("acpi         : could not parse the MADT");
        return;
    };

    println!("acpi         : lapic at {:#x}", tables.local_apic_address);
    match tables.io_apic {
        Some(io_apic) => println!(
            "ioapic       : at {:#x}, gsi base {}",
            io_apic.address, io_apic.gsi_base
        ),
        None => println!("ioapic       : none found"),
    }

    if let Err(e) = apic::init(&tables) {
        println!("apic         : FAILED - {e}");
        return;
    }
    println!(
        "apic timer   : {} counts/s, firing at {} Hz",
        apic::timer_counts_per_second(),
        apic::TIMER_FREQUENCY
    );

    // Read the routing back out of the chip rather than trusting the write.
    // Bit 16 set would mean the line is still masked.
    let (gsi, _) = tables.resolve_irq(1);
    if let Some(io_apic) = tables.io_apic {
        let entry = unsafe { apic::redirection_entry(&io_apic, gsi) };
        println!(
            "keyboard     : irq 1 -> gsi {gsi}, vector {}, {}",
            entry & 0xFF,
            if entry & (1 << 16) != 0 { "MASKED" } else { "unmasked" }
        );
    }

    // Must happen before interrupts are unmasked: a byte the firmware left in
    // the 8042 would otherwise block IRQ1 forever.
    if keyboard::init() {
        // The mouse shares the controller, so it can only be set up once the
        // keyboard has finished reconfiguring it.
        if mouse::init() {
            if let Some(response) = FRAMEBUFFER.response() {
                if let Some(fb) = response.framebuffers().first() {
                    mouse::set_bounds(fb.width as usize, fb.height as usize);
                }
            }
            println!("ps/2         : keyboard and mouse ready");
        } else {
            println!("ps/2         : keyboard ready, no mouse");
        }
    } else {
        // Hyper-V Generation 2 has no 8042 at all; its keyboard is a synthetic
        // VMBus device. Say so, rather than leaving a dead prompt.
        println!("ps/2         : no keyboard - use the serial console for input");
    }

    interrupts::enable();
    println!("interrupts   : enabled");

    // Confirm the timer is actually delivering, rather than assuming the LVT
    // programming took. 50 ms at 100 Hz should be about 5 ticks.
    let before = apic::ticks();
    pit::wait_micros(50_000);
    println!(
        "timer        : {} ticks in 50 ms",
        apic::ticks() - before
    );
}

fn banner() {
    // The mark first, with the boot log flowing beneath it. Sized from the
    // screen rather than fixed: 96 pixels is a badge on a 1280x800 display and
    // a postage stamp on a 4K one.
    print::logo(96 * theme::screen_scale());

    if let Some(console) = CONSOLE.lock().as_mut() {
        console.set_fg(0x7A, 0xC7, 0xFF);
    }
    println!("+-----------------------------------+");
    // The box is 35 characters wide inside the bars, and "  Kestrel " takes
    // ten of them. Saturating, so a longer version string shrinks the padding
    // rather than underflowing the width and taking the banner down with it.
    println!(
        "|  Kestrel {VERSION}{:width$}|",
        "",
        width = 25usize.saturating_sub(VERSION.len()).max(1)
    );
    println!("+-----------------------------------+");
    println!();
    if let Some(console) = CONSOLE.lock().as_mut() {
        console.set_fg(0xD0, 0xD6, 0xE0);
    }
}

fn report_boot_info() {
    if let Some(info) = BOOTLOADER_INFO.response() {
        println!("bootloader   : {} {}", info.name(), info.version());
    }

    if let Some(revision) = BASE_REVISION.actual_revision() {
        println!("base revision: {revision}");
    }

    if let Some(hhdm) = HHDM.response() {
        println!("hhdm offset  : {:#018x}", hhdm.offset);
    }

    if let Some(addr) = EXECUTABLE_ADDRESS.response() {
        println!(
            "kernel base  : virt {:#018x}  phys {:#018x}",
            addr.virtual_base, addr.physical_base
        );
    }

    if let Some(response) = FRAMEBUFFER.response() {
        if let Some(fb) = response.framebuffers().first() {
            println!(
                "framebuffer  : {}x{} @ {} bpp",
                fb.width, fb.height, fb.bpp
            );
        }
    }

    if let Some(memmap) = MEMMAP.response() {
        let entries = memmap.entries();
        let usable: u64 = entries
            .iter()
            .filter(|e| e.type_ == limine::memmap::MEMMAP_USABLE)
            .map(|e| e.length)
            .sum();
        println!(
            "memory       : {} MiB usable across {} regions",
            usable / (1024 * 1024),
            entries.len()
        );
    }
}

fn halt() -> ! {
    loop {
        x86_64::instructions::hlt();
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    use core::fmt::Write;

    // Bypass the normal print path: the panic may have come from code that
    // already holds the serial lock, and blocking on it would hang silently
    // instead of reporting anything.
    let mut serial = crate::serial::SerialPort::com1();
    serial
        .write_fmt(format_args!("\n*** KERNEL PANIC ***\n{info}\n"))
        .ok();

    // Show it on screen too, but only if the console happens to be free.
    if let Some(Some(console)) = CONSOLE.try_lock().as_deref_mut() {
        console.set_fg(0xFF, 0x5C, 0x5C);
        console
            .write_fmt(format_args!("\n*** KERNEL PANIC ***\n{info}\n"))
            .ok();
    }

    halt()
}






