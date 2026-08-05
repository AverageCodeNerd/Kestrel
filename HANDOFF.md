# Kestrel — handoff briefing

A from-scratch x86-64 operating system in Rust. This file is a self-contained
briefing for continuing the work without the original conversation.

Location: `C:\Users\Eddie\Desktop\Kestrel`
Size: ~7,600 lines of kernel across ~35 modules, plus build tooling and three
userspace programs.

---

## Building and running

Toolchain: **Rust nightly** with the `x86_64-unknown-none` target and the
**`x86_64-pc-windows-gnu` host** (chosen deliberately — the machine has no
Visual Studio, and the GNU host bundles its own linker). Nightly is required,
not preferred: `abi_x86_interrupt` is still unstable. QEMU is at
`C:\Program Files\qemu`. No C cross-compiler is needed; `x86_64-unknown-none`
links with the bundled `rust-lld`.

```bash
cargo xtask run                      # build + boot in QEMU (4 cores, networking)
cargo xtask image --release          # produce build/kestrel.{img,vhd,iso}
cargo xtask icon                     # regenerate assets/kestrel-*.png
.\release.ps1                        # stage a versioned release + SHA256SUMS
```

The version lives in **`kernel/Cargo.toml`** and nowhere else: `main.rs` exposes
it as `VERSION`, and the boot banner, the `/etc/motd` greeting, `uname`, the
`version` command and `release.ps1`'s output directory all read it from there.
Bump it in one place.

Useful flags: `--release`, `--cpus N`, `--image` (boot the real disk image),
`--vhd`, `--iso`, `--keep-image`, `--headless`, `--timeout N`,
`--screenshot f.png`, `--keys "text"`, `--serial-keys "text"`, `--pcap f.pcap`.

**Never run `cargo build` at the workspace root alone.** The kernel needs
`--target x86_64-unknown-none`, and the workspace defaults to the host-side
`xtask`. Use `cargo xtask`.

### Workspace layout

```
kernel/     the kernel (no_std, x86_64-unknown-none)
xtask/      build driver: compiles, stages an ESP, makes images, drives QEMU
user/       hello, fault, selfmod — userspace ELF programs
logo/       shared no_std crate describing the icon as code
limine/     prebuilt Limine v11 binaries (shallow clone, carries its own .git)
assets/     generated PNG icons
dist/       images staged for VirtualBox
```

---

## Testing approach

The build driver can drive the guest, which is how everything was verified:

- `--keys "ls\nps\n"` injects **real PS/2 scancodes** via the QEMU monitor.
  Supports `{up}`, `{left}`, `{home}`, `{del}`, `{mouse:dx,dy}`, `{click}`.
- `--serial-keys "..."` types over the serial console instead and returns the
  output — the only way in on machines with no PS/2.
- `--screenshot f.png` captures the framebuffer.
- `--pcap f.pcap` records all network traffic for host-side inspection.

**Principle worth keeping:** verify against something other than the kernel
itself. Host-side Python parsers were written for the ELF, the FAT32 image, and
the pcap. A self-consistent bug — for instance writing only one copy of the FAT
— is invisible to the kernel's own reader.

---

## What works

**Boot** — Limine as a UEFI application. Static `ET_EXEC` ELF at
`0xffffffff80000000`. Communication via structs in `.limine_requests`.

**Console** — framebuffer text with an 8x8 font, plus a 16550 serial log.

**CPU** — per-core GDT and TSS with IST stacks; IDT covering every exception.

**Interrupts** — ACPI (RSDP/XSDT/MADT) to find the APICs, local APIC timer
calibrated against the PIT to 100 Hz, I/O APIC routing, PS/2 keyboard and mouse.

**Memory** — frame allocator with an intrusive free list, page-table ownership,
32 MiB heap, per-process address spaces.

**Processes** — preemptive scheduling on 4 cores with per-core run queues; ELF
loader; ring 3; `SYSCALL`/`SYSRET`; W^X enforced with the NX bit.

**Storage** — PCI, AHCI (SATA) polled DMA, GPT, read-write FAT32 with long
filenames, mounted at `/disk`. Files persist across reboots.

**Network** — e1000 driver, ARP, IPv4, ICMP, UDP, DNS, client TCP.
`fetch http://example.com/` retrieves a real page.

**Shell** — line editing (arrows, home/end, delete), command history, ~30
commands. `help` lists them.

**Desktop** — compositor with a panel, a launcher menu, desktop shortcuts,
windows with title bars and focus states, and a cursor. Entered with `desktop`,
left with Esc. **Windows drag by their title bars.** Still incomplete — see
below.

The drag bug that stalled this for two days was not the mouse driver: the
button *was* arriving, but every pointer event with the button held re-ran the
click handlers, so a title-bar drag re-grabbed the window each frame and the
offset never moved it. The fix is press-edge detection — `left_was_down` in
`Desktop`, so a click acts once on the transition from up to down, and while a
drag is live nothing else is hit-tested. Any new clickable region must go
*after* that edge check, or it will fire continuously while the button is held.

---

## Known bugs and unfinished work

Ordered by how worthwhile they are to pick up.

1. **Networking fails under VirtualBox.** `arp: timed out transmitting`; the
   transmit descriptor never reports done. Works in QEMU. Adding the
   datasheet's `TCTL` collision-threshold and collision-distance fields did not
   fix it. Trace the descriptor status bit to see whether VirtualBox completes
   it at all.

2. **The ISO does not boot on VirtualBox** — see "Running it in a VM". A beta
   that ships only a VHD is a poor first impression, so this is a release
   blocker rather than a nicety.

3. **`kill` is blunt** — a task killed while holding a lock leaks it.

4. **Only one terminal window.** The desktop's windows are fixed at entry;
   there is no way to open a second one, and the launcher only focuses what
   already exists rather than spawning anything.

5. **No task migration between cores.** Tasks are pinned at spawn. Migration
   needs the release-versus-save race solved (see below).

6. **No TLS**, so https is unreachable; and `fetch` is a client, not a browser —
   nothing parses or renders HTML.

7. **No `fork`**; `exec` always creates a new process rather than replacing the
   caller.

### The terminal window

The shell runs *inside* the desktop, and no command implementation knows it.
Every command already writes with `print!`, so `terminal::start_capture()`
diverts that byte stream into a staging buffer instead of the framebuffer
console, and the desktop loop drains it into a window each frame.

Three things make it work:

- **The line editor repaints, it does not append.** It emits carriage return,
  prompt, contents, spaces over the previous line's tail, then walks back. So
  `terminal::Terminal` is a grid with a cursor and overwrite semantics, not a
  list of finished lines. `print::move_left` feeds its movement in as backspace
  bytes so cursor motion and text stay one ordered stream.
- **Input is handled outside the `DESKTOP` lock.** `on_key` runs entire
  commands, and a command printing while this task held that lock would
  deadlock against the compositor. The same applies to `terminal::drain`,
  which swaps the buffer out under its own lock and processes it after
  releasing.
- **`desktop` is dispatched from `on_key`, which the desktop loop calls.**
  Without the `in_desktop` guard, typing `desktop` inside the desktop nests a
  second one inside the first.

The panic handler is unaffected: it bypasses `_print` and writes to the console
directly, so a panic is still visible even with capture on.

### Desktop rendering

The compositor now uses **region-based damage tracking**. `Desktop::damage`
holds the union of changed rectangles; rendering clips every layer to that
region and `present` writes only those pixels to the framebuffer. Cursor motion
invalidates its old and new 12x12 bounds, while a dragged window invalidates its
old and new bounds. Changes to focus or stacking invalidate the whole screen,
because either window may overlap any other. When idle, `render` returns without
any framebuffer work.

Two consequences that are easy to reintroduce:

- **Draw the cursor at `last_cursor`, never at the live pointer.** The mouse
  interrupt moves the pointer asynchronously, so re-reading it inside `render`
  can place the arrow outside the region that was invalidated for it: half is
  clipped away and the other half is never erased, leaving a trail.
- **Anything that paints the framebuffer behind the compositor must say so.**
  A full repaint used to erase a stray `println!` within a frame; now the
  console's pixels would simply stay. `print::_print` sets
  `desktop::SCREEN_DISTURBED`, and `render` turns that into a full invalidate.
  It is a bare atomic on purpose — `_print` can run *inside* `desktop::with`,
  so taking the `DESKTOP` lock there would deadlock.

To check a capture from outside the kernel, scan it for the cursor's outline
colour (`0x101828`): more than one cluster of those pixels means a stale cursor
was left on screen, which is the failure this scheme is prone to.

---

## Gotchas that cost real time

Each of these was a bug that took a while to find. Worth reading before
touching the relevant area.

- **MMIO is not in the bootloader's direct map.** Limine maps RAM, not device
  registers. APIC and PCI BARs must be mapped before first touch — which is why
  memory management has to initialise *before* interrupts.

- **The syscall stub must preserve the argument registers.** `dispatch` is an
  ordinary Rust function and clobbers every caller-saved register. Omitting
  `rdi`/`rsi`/`rdx`/`r8`–`r10` corrupts **optimised user builds only**, because
  debug builds do not keep live values there across the asm block.

- **`swapgs` must balance, and `exit` never returns.** The entry swap has no
  matching exit swap when a task dies, so the next system call on that core
  swaps a zero base in and faults on its first instruction. `task::finish`
  resets both GS bases outright rather than trusting the swaps.

- **`EFER` is per-core.** Enabling NX only on the boot processor makes every
  no-execute page fault on the other cores and nowhere else.

- **ACPI tables need unaligned reads.** They are byte-packed, and the XSDT's
  array of 64-bit pointers starts at offset 36, so it is never 8-byte aligned.

- **Interrupt handlers must not print or allocate.** They can preempt code
  holding those locks, and a spinlock taken twice on one CPU is a hang.
  `println!` masks interrupts for its critical section.

- **The 8042 must be initialised, not assumed.** Firmware is not guaranteed to
  leave scanning enabled or IRQ1 unmasked, and an unread byte in the output
  buffer blocks all further keyboard interrupts forever.

- **`--keep-image` keeps the old kernel too.** The kernel lives on that image.
  Two test runs were wasted on stale code before this was spotted.

- **Task migration has a race.** Marking a task runnable while it still runs on
  another core, before its context is saved, corrupts its stack. This is why
  tasks are pinned.

---

## Running it in a VM

**The VM must boot UEFI, not BIOS** — there is no legacy boot path — and
**Secure Boot must be off**, because Limine is not signed.

- **QEMU** — `cargo xtask run`. Everything works, networking included.
- **VirtualBox** — a VM named `Kestrel` is registered and verified booting:
  EFI on, 4 CPUs, 2048 MB, 128 MB VRAM, SATA/AHCI, NIC set to Intel PRO/1000 MT
  (82540EM), serial port 1 logging to `dist\serial.log`, booting
  `dist\kestrel.vhd`. **Use the VHD, not the ISO** — on VirtualBox's virtual
  DVD, Limine stops at "Could not meaningfully match the boot device handle
  with a volume… Press any key" and never reaches the kernel. Networking does
  not work here (see above).

  After rebuilding, run `dist\refresh-vm.ps1` rather than copying by hand. It
  detaches the disk before overwriting it — VirtualBox keeps the attached file
  open, so a plain copy fails part-way and leaves a torn image — and it refuses
  to run while the VM is up. The VHD footer carries a **fixed UUID**, so
  VirtualBox will not register `build\` and `dist\` copies at the same time;
  if it ever complains that a disk with that UUID already exists, unregister
  the stale one with `VBoxManage closemedium disk <uuid>` (that only
  unregisters, it does not delete the file).

  The serial log is the easiest way to see what happened on a failed boot,
  since it captures the whole banner before anything reaches the framebuffer.
- **Hyper-V** — Generation 2 only (Gen 1 is BIOS). **The keyboard will not
  work**: Gen 2 has no PS/2 controller, its keyboard is a VMBus device and there
  is no VMBus driver. Use the serial console:
  `Set-VMComPort -VMName Kestrel -Number 1 -Path \\.\pipe\kestrel`, then attach
  PuTTY to that pipe.
