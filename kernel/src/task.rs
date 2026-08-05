//! Preemptive round-robin scheduling of kernel threads.
//!
//! A context switch here is deliberately minimal: it saves the six callee-saved
//! registers, swaps the stack pointer, and returns. Everything else the ABI
//! already guarantees is dead across a function call, and anything the CPU
//! pushed for an interrupt stays on the outgoing task's own stack.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::structures::paging::PhysFrame;

use crate::cpu::MAX_CPUS;
use crate::memory::AddressSpace;

const STACK_SIZE: usize = 64 * 1024;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Runnable,
    Finished,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Runnable => "runnable",
            State::Finished => "finished",
        }
    }
}

/// A snapshot of one task, for reporting without holding the scheduler lock.
pub struct TaskInfo {
    pub id: u64,
    pub name: String,
    pub state: State,
    pub quanta: u64,
    pub running: bool,
    /// True if this task runs a user program in its own address space.
    pub user: bool,
    pub exit_code: u64,
    /// The core this task is pinned to.
    pub cpu: usize,
}

/// What a task should do when it first runs.
#[derive(Clone, Copy)]
pub enum Start {
    /// Nothing: this task was already running when the scheduler started.
    Bootstrap,
    /// A kernel function, in ring 0.
    Kernel(fn()),
    /// A user program: drop to ring 3 at this entry point and stack.
    User { entry: VirtAddr, stack: VirtAddr },
}

pub struct Task {
    pub id: u64,
    pub name: String,
    pub state: State,
    /// How many times this task has been scheduled in.
    pub quanta: u64,
    /// Exit status, once finished.
    pub exit_code: u64,
    /// Saved stack pointer while this task is not running.
    rsp: u64,
    /// Owns the task's kernel stack; dropping the task frees it.
    _stack: Vec<u8>,
    /// A separate stack for entries from ring 3. It cannot be the kernel
    /// stack above: that one holds the frame the task will return through, and
    /// an interrupt from user mode would land on top of it.
    _ring0_stack: Vec<u8>,
    ring0_top: u64,
    /// `None` for kernel tasks, which run on the kernel's own page tables.
    pub address_space: Option<AddressSpace>,
    start: Start,
}

impl Task {
    /// The task that is already running when the scheduler starts. It has no
    /// stack of its own — it inherits the boot stack — and its `rsp` is filled
    /// in the first time it is switched away from.
    fn bootstrap(name: String, cpu: usize) -> Self {
        Self {
            // The boot processor's task is 0; the other cores' idle tasks take
            // ordinary ids.
            id: if cpu == 0 {
                0
            } else {
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            },
            name,
            state: State::Runnable,
            quanta: 0,
            exit_code: 0,
            rsp: 0,
            _stack: Vec::new(),
            _ring0_stack: Vec::new(),
            ring0_top: crate::gdt::kernel_stack_top(cpu).as_u64(),
            address_space: None,
            start: Start::Bootstrap,
        }
    }

    fn new(name: &str, start: Start, address_space: Option<AddressSpace>) -> Self {
        let mut stack = vec![0u8; STACK_SIZE];
        let mut ring0_stack = vec![0u8; STACK_SIZE];
        let ring0_top = (ring0_stack.as_mut_ptr() as u64 + STACK_SIZE as u64) & !0xF;

        // Build the stack so that the first context switch into this task
        // "returns" into the trampoline, exactly as if it had been switched
        // away from at the top of that function.
        let top = stack.as_mut_ptr() as u64 + STACK_SIZE as u64;
        let top = top & !0xF; // the ABI wants a 16-byte aligned stack

        let rsp = unsafe {
            let mut slot = top as *mut u64;

            // A fake return address, so that once the trampoline is entered
            // the stack has the same alignment a `call` would have produced.
            slot = slot.sub(1);
            slot.write(0);

            slot = slot.sub(1);
            slot.write(trampoline as extern "C" fn() -> ! as u64);

            // rbp, rbx, r12, r13, r14, r15 — popped by switch_context.
            for _ in 0..6 {
                slot = slot.sub(1);
                slot.write(0);
            }

            slot as u64
        };

        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            name: name.to_string(),
            state: State::Runnable,
            quanta: 0,
            exit_code: 0,
            rsp,
            _stack: stack,
            _ring0_stack: ring0_stack,
            ring0_top,
            address_space,
            start,
        }
    }
}

/// Swap the current stack for another task's.
///
/// Saves the callee-saved registers, writes the resulting stack pointer to
/// `*old_rsp`, then restores the same registers from `new_rsp` and returns —
/// which lands in whichever task owns that stack.
#[unsafe(naked)]
unsafe extern "C" fn switch_context(old_rsp: *mut u64, new_rsp: u64) {
    core::arch::naked_asm!(
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",
        "mov rsp, rsi",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
    )
}

/// Where every new task begins.
extern "C" fn trampoline() -> ! {
    // A fresh task inherits IF=0 from whatever context switched into it, and
    // our switch does not restore RFLAGS. Without this the timer would never
    // fire again and the machine would wedge on the first spawned task.
    x86_64::instructions::interrupts::enable();

    let start = SCHEDULER.lock().current_start(crate::cpu::index());

    match start {
        Start::Kernel(entry) => {
            entry();
            finish(0);
        }
        // Never returns: a user program leaves only through `exit`, which
        // marks the task finished from inside the syscall handler.
        Start::User { entry, stack } => unsafe { crate::usermode::enter(entry, stack) },
        Start::Bootstrap => finish(0),
    }
}

/// Mark the running task finished and never run it again.
pub fn finish(code: u64) -> ! {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let cpu = crate::cpu::index();

        // A task reaching here never returns through whatever brought it into
        // the kernel. If that was a system call, the entry `swapgs` has no
        // matching one on the way out, and the *next* system call on this core
        // would swap a zero base in and fault. Reset both bases outright, so
        // the core is left in the same state as if it had never entered.
        unsafe { crate::cpu::init_gs(cpu) };

        SCHEDULER.lock().mark_finished(cpu, code);
    });

    loop {
        // A finished task is never picked again, so this only ever switches
        // away — but if it is somehow the last runnable task, idle politely.
        yield_now();
        x86_64::instructions::hlt();
    }
}

pub struct Scheduler {
    /// One run queue per core.
    ///
    /// Tasks are pinned to a core when spawned and never migrate. That is
    /// deliberate: moving a task between cores means one core marking it
    /// runnable while it is still executing on another, and the window between
    /// "released" and "context actually saved" is a race that corrupts the
    /// task's stack. Pinning removes the window entirely, at the cost of no
    /// load balancing after the fact.
    ///
    /// Boxed so a task's address — and therefore its saved `rsp` slot — stays
    /// put when a queue grows.
    queues: [Vec<Box<Task>>; MAX_CPUS],
    /// Index into `queues[cpu]` of what each core is currently running.
    current: [usize; MAX_CPUS],
    switches: u64,
}

impl Scheduler {
    const fn new() -> Self {
        Self {
            queues: [const { Vec::new() }; MAX_CPUS],
            current: [0; MAX_CPUS],
            switches: 0,
        }
    }

    fn current_start(&self, cpu: usize) -> Start {
        self.queues[cpu]
            .get(self.current[cpu])
            .map_or(Start::Bootstrap, |t| t.start)
    }

    pub fn current_id(&self, cpu: usize) -> u64 {
        self.queues[cpu].get(self.current[cpu]).map_or(0, |t| t.id)
    }

    fn mark_finished(&mut self, cpu: usize, code: u64) {
        let index = self.current[cpu];
        if let Some(task) = self.queues[cpu].get_mut(index) {
            task.state = State::Finished;
            task.exit_code = code;
        }
    }

    /// Next runnable task on this core, after the current one and wrapping.
    fn pick_next(&self, cpu: usize) -> Option<usize> {
        let queue = &self.queues[cpu];
        let count = queue.len();
        if count == 0 {
            return None;
        }

        (1..=count)
            .map(|step| (self.current[cpu] + step) % count)
            .find(|&i| queue[i].state == State::Runnable)
    }

    /// The core with the shortest run queue, for placing a new task.
    fn least_loaded(&self) -> usize {
        (0..crate::cpu::online().max(1))
            .min_by_key(|&cpu| self.queues[cpu].len())
            .unwrap_or(0)
    }

    pub fn switches(&self) -> u64 {
        self.switches
    }

    pub fn describe(&self) -> Vec<TaskInfo> {
        let mut out = Vec::new();

        for (cpu, queue) in self.queues.iter().enumerate() {
            for (index, task) in queue.iter().enumerate() {
                out.push(TaskInfo {
                    id: task.id,
                    name: task.name.clone(),
                    state: task.state,
                    quanta: task.quanta,
                    running: index == self.current[cpu],
                    user: task.address_space.is_some(),
                    exit_code: task.exit_code,
                    cpu,
                });
            }
        }

        out.sort_by_key(|info| info.id);
        out
    }
}

pub static SCHEDULER: Mutex<Scheduler> = Mutex::new(Scheduler::new());

/// Register the code currently running on `cpu` as a task, so that core has
/// something to switch away from.
pub fn init_cpu(cpu: usize) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut scheduler = SCHEDULER.lock();
        if scheduler.queues[cpu].is_empty() {
            let name = if cpu == 0 {
                String::from("kmain")
            } else {
                alloc::format!("idle{cpu}")
            };
            scheduler.queues[cpu].push(Box::new(Task::bootstrap(name, cpu)));
        }
    });
}

pub fn spawn(name: &str, entry: fn()) -> u64 {
    spawn_task(name, Start::Kernel(entry), None)
}

/// Add a task to a run queue, with an optional private address space.
///
/// The core is chosen once, here, and never changes.
pub fn spawn_task(name: &str, start: Start, address_space: Option<AddressSpace>) -> u64 {
    let task = Box::new(Task::new(name, start, address_space));
    let id = task.id;

    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut scheduler = SCHEDULER.lock();
        let cpu = scheduler.least_loaded();
        scheduler.queues[cpu].push(task);
    });

    id
}

/// Pick the next task and switch to it.
///
/// Safe to call from an interrupt handler. The scheduler lock is always
/// released *before* the stack is swapped: holding it across a switch would
/// leave it locked by a task that is no longer running.
/// Why a call to `schedule` did not result in a switch.
pub static SCHEDULE_CALLS: AtomicU64 = AtomicU64::new(0);
pub static LOCK_FAILURES: AtomicU64 = AtomicU64::new(0);
pub static NO_CANDIDATE: AtomicU64 = AtomicU64::new(0);
/// Nothing to switch to because only one task exists.
pub static NOTHING_ELSE: AtomicU64 = AtomicU64::new(0);

pub fn schedule() {
    SCHEDULE_CALLS.fetch_add(1, Ordering::Relaxed);
    let cpu = crate::cpu::index();

    let Some(mut scheduler) = SCHEDULER.try_lock() else {
        // Another core holds the scheduler, or this one is inspecting it.
        // Skip the quantum rather than spin with interrupts off.
        LOCK_FAILURES.fetch_add(1, Ordering::Relaxed);
        return;
    };

    if scheduler.queues[cpu].len() < 2 {
        NOTHING_ELSE.fetch_add(1, Ordering::Relaxed);
        return;
    }

    let Some(next) = scheduler.pick_next(cpu) else {
        return;
    };

    if next == scheduler.current[cpu] {
        NO_CANDIDATE.fetch_add(1, Ordering::Relaxed);
        return;
    }

    let current = scheduler.current[cpu];
    scheduler.current[cpu] = next;
    scheduler.switches += 1;
    scheduler.queues[cpu][next].quanta += 1;

    // Raw pointers taken while the lock is held, used after it is dropped.
    // Safe because the tasks are boxed and never move or are freed, and
    // because a task only ever runs on the core that owns its queue.
    let old_rsp = &mut scheduler.queues[cpu][current].rsp as *mut u64;
    let new_rsp = scheduler.queues[cpu][next].rsp;
    let new_ring0 = scheduler.queues[cpu][next].ring0_top;
    let new_cr3: Option<PhysFrame> = scheduler.queues[cpu][next]
        .address_space
        .as_ref()
        .map(|space| space.frame());

    drop(scheduler);

    // Tell this core which stack to use if the incoming task is interrupted or
    // makes a system call from ring 3.
    crate::gdt::set_kernel_stack(cpu, VirtAddr::new(new_ring0));
    crate::syscall::set_kernel_stack(cpu, new_ring0);

    // Swap address spaces. Safe to do while running kernel code because every
    // address space shares the kernel's higher half, including these stacks.
    unsafe { crate::memory::activate(new_cr3) };

    unsafe { switch_context(old_rsp, new_rsp) };
}

/// Drop finished tasks, releasing their stacks, page tables, and frames.
///
/// Done here rather than at exit because a task cannot free the stack it is
/// still standing on — it has to be gone first.
pub fn reap() -> usize {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut scheduler = SCHEDULER.lock();
        let mut collected = 0;

        // Every core's queue is fair game: a finished task is running nowhere,
        // and each queue's own current task is protected by id below.
        for cpu in 0..MAX_CPUS {
            let Some(current_id) = scheduler.queues[cpu]
                .get(scheduler.current[cpu])
                .map(|t| t.id)
            else {
                continue;
            };

            let before = scheduler.queues[cpu].len();
            scheduler.queues[cpu]
                .retain(|task| task.state != State::Finished || task.id == current_id);
            collected += before - scheduler.queues[cpu].len();

            // Removing entries shifts everything after them, so re-find the
            // running task rather than trusting the old index.
            scheduler.current[cpu] = scheduler.queues[cpu]
                .iter()
                .position(|task| task.id == current_id)
                .unwrap_or(0);
        }

        collected
    })
}

/// Stop a task. It will not run again, and `reap` will collect it.
///
/// Blunt: if the task was preempted while holding a lock, that lock stays
/// held. Fine for stopping a runaway program, not a general mechanism.
pub fn kill(id: u64) -> Result<(), &'static str> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut scheduler = SCHEDULER.lock();

        if id == 0 {
            return Err("cannot kill the kernel task");
        }

        // A task another core is currently running would be left holding
        // whatever it holds, so refuse those too.
        for cpu in 0..MAX_CPUS {
            if scheduler.queues[cpu]
                .get(scheduler.current[cpu])
                .map(|t| t.id)
                == Some(id)
            {
                return Err("cannot kill a running task");
            }
        }

        for cpu in 0..MAX_CPUS {
            if let Some(task) = scheduler.queues[cpu].iter_mut().find(|task| task.id == id) {
                task.state = State::Finished;
                task.exit_code = 137;
                return Ok(());
            }
        }

        Err("no such task")
    })
}

/// Give up the rest of this task's time slice.
pub fn yield_now() {
    x86_64::instructions::interrupts::without_interrupts(schedule);
}
