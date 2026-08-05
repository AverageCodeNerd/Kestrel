//! Starting the application processors.
//!
//! Limine has already taken the other cores out of reset and parked them
//! spinning on a pointer, which spares us the real-mode trampoline and INIT-SIPI
//! dance. Writing a function address into a core's info block sends it there.

use limine::mp::MpInfo;
use limine::request::MpRequest;

use crate::{apic, cpu, gdt, interrupts, println};

/// Zero flags: do not ask for x2APIC, because the APIC driver addresses the
/// local APIC through memory-mapped registers.
#[used]
#[unsafe(link_section = ".limine_requests")]
pub static MP: MpRequest = MpRequest::new(0);

/// Where an application processor begins life.
///
/// It arrives on a stack Limine provided, with no descriptor tables of its own
/// and its local APIC switched off, so it has to build all of that before it
/// can take an interrupt.
unsafe extern "C" fn start_ap(info: &MpInfo) -> ! {
    let index = info.extra_argument() as usize;

    // EFER is per-core: NX and SYSCALL have to be enabled here too.
    cpu::enable_features();

    unsafe {
        gdt::init(index);
        cpu::init_gs(index);
    }
    // The IDT is shared; every core points at the same table.
    interrupts::init();
    // Each core enables SYSCALL for itself: LSTAR and friends are per-CPU.
    crate::syscall::init();
    apic::init_ap();

    // Give this core an idle task, so the scheduler has something to switch
    // away from when work lands on its queue.
    crate::task::init_cpu(index);

    cpu::mark_online();
    interrupts::enable();

    // The idle loop: run anything queued to this core, otherwise sleep until
    // the next interrupt.
    loop {
        crate::task::yield_now();
        x86_64::instructions::hlt();
    }
}

/// Start every core the firmware reported, and wait for them to check in.
pub fn init() {
    let Some(response) = MP.response() else {
        println!("smp          : not supported by the bootloader");
        return;
    };

    let cpus = response.cpus();
    let boot_lapic_id = response.bsp_lapic_id;

    // Give the boot processor index 0 so its tables, already loaded, match.
    let mut next_index = 1;
    for info in cpus {
        if info.lapic_id == boot_lapic_id {
            cpu::register(0, info.lapic_id);
        } else if next_index < cpu::MAX_CPUS {
            cpu::register(next_index, info.lapic_id);
            next_index += 1;
        }
    }

    cpu::mark_online(); // the core running this one

    for info in cpus {
        if info.lapic_id == boot_lapic_id {
            continue;
        }

        // Find the index registered above.
        let Some(index) = (0..cpu::MAX_CPUS).find(|&i| cpu::lapic_id(i) == info.lapic_id) else {
            continue;
        };

        info.bootstrap(start_ap, index as u64);
    }

    // Wait briefly for the others; a core that never checks in is reported
    // rather than hanging the boot.
    let expected = next_index.min(cpus.len());
    for _ in 0..200 {
        if cpu::online() >= expected {
            break;
        }
        crate::pit::wait_micros(1_000);
    }

    println!(
        "smp          : {} of {} cores online",
        cpu::online(),
        cpus.len()
    );
}
