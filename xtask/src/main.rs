//! Build driver for Kestrel: compiles the kernel, assembles a bootable EFI
//! system partition around Limine, and launches it under QEMU.
//!
//! Run via the cargo alias: `cargo xtask run`.

mod crc;
mod fat;
mod icon;
mod image;
mod iso;
mod png;

use std::io::Write;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const TARGET: &str = "x86_64-unknown-none";
const MONITOR_PORT: u16 = 55555;
const SERIAL_PORT: u16 = 55556;

struct Options {
    release: bool,
    headless: bool,
    screenshot: Option<PathBuf>,
    /// Text to type into the guest over the QEMU monitor before capturing.
    keys: Option<String>,
    /// Boot the real disk image rather than QEMU's `fat:rw:` directory.
    from_image: bool,
    /// Boot the VHD instead of the raw image, to verify the VHD wrapper.
    from_vhd: bool,
    /// Boot the ISO from a virtual DVD drive.
    from_iso: bool,
    /// Leave an existing disk image alone instead of regenerating it, so
    /// anything the guest wrote to it survives.
    keep_image: bool,
    /// Text to type at the guest over the *serial* console rather than the
    /// keyboard, for testing machines that have no PS/2 controller.
    serial_keys: Option<String>,
    /// How many cores to give the guest.
    cpus: u32,
    /// Capture the guest's network traffic to this pcap file.
    pcap: Option<PathBuf>,
    /// Seconds to let the guest run before capturing and quitting.
    timeout: u64,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("run");

    let opts = Options {
        release: args.iter().any(|a| a == "--release"),
        headless: args.iter().any(|a| a == "--headless"),
        screenshot: flag_value(&args, "--screenshot").map(PathBuf::from),
        keys: flag_value(&args, "--keys"),
        from_image: args.iter().any(|a| a == "--image" || a == "--vhd"),
        from_vhd: args.iter().any(|a| a == "--vhd"),
        from_iso: args.iter().any(|a| a == "--iso"),
        keep_image: args.iter().any(|a| a == "--keep-image"),
        serial_keys: flag_value(&args, "--serial-keys"),
        // Default to several cores: the kernel is SMP, and running it on one
        // core by default would leave that path untested.
        pcap: flag_value(&args, "--pcap").map(PathBuf::from),
        cpus: flag_value(&args, "--cpus")
            .and_then(|v| v.parse().ok())
            .unwrap_or(4),
        timeout: flag_value(&args, "--timeout")
            .and_then(|v| v.parse().ok())
            .unwrap_or(5),
    };

    match command {
        "build" => {
            build_kernel(&opts);
            build_user_programs(&opts);
            assemble_esp(&opts);
        }
        "run" => {
            build_kernel(&opts);
            build_user_programs(&opts);
            assemble_esp(&opts);

            let mut iso = None;
            if opts.from_iso {
                iso = Some(build_iso());
            } else if opts.from_image {
                let existing = workspace_root().join("build/kestrel.img").exists();
                if opts.keep_image && existing {
                    // Worth spelling out: the kernel lives on this image, so
                    // keeping it also keeps the kernel that was there before.
                    println!("disk image   : keeping the existing one (kernel included)");
                } else {
                    build_image(&opts);
                }
            }

            run_qemu(&opts, iso.as_deref());
        }
        "image" => {
            build_kernel(&opts);
            build_user_programs(&opts);
            assemble_esp(&opts);
            build_image(&opts);
            build_iso();
        }
        "icon" => {
            let root = workspace_root();
            std::fs::create_dir_all(root.join("assets")).ok();

            for size in [16u32, 32, 64, 128, 256] {
                let path = root.join(format!("assets/kestrel-{size}.png"));
                match icon::write(&path, size) {
                    Ok(()) => println!("icon         : {}", path.display()),
                    Err(e) => eprintln!("failed to write {}: {e}", path.display()),
                }
            }
        }
        "iso" => {
            build_kernel(&opts);
            build_user_programs(&opts);
            assemble_esp(&opts);
            build_iso();
        }
        other => {
            eprintln!("unknown command: {other}");
            eprintln!("usage: cargo xtask [build|run|image] [--release] [--headless] [--image]");
            eprintln!("                   [--screenshot <file.png>] [--keys <text>]");
            eprintln!("                   [--timeout <secs>]");
            std::process::exit(2);
        }
    }
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let idx = args.iter().position(|a| a == flag)?;
    args.get(idx + 1).cloned()
}

fn workspace_root() -> PathBuf {
    // xtask/ lives directly under the workspace root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn profile_dir(opts: &Options) -> &'static str {
    if opts.release { "release" } else { "debug" }
}

fn build_kernel(opts: &Options) {
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.current_dir(workspace_root())
        .args(["build", "--package", "kernel", "--target", TARGET]);
    if opts.release {
        cmd.arg("--release");
    }

    let status = cmd.status().expect("failed to invoke cargo");
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// User programs shipped as bootloader modules.
const USER_PROGRAMS: &[&str] = &["hello", "fault", "selfmod", "edit"];

/// Build the userspace programs.
///
/// These target the same bare-metal triple as the kernel but must *not*
/// inherit its rustflags: `-C code-model=kernel` assumes everything lives in
/// the top 2 GiB, which is exactly where user code does not live. Setting
/// RUSTFLAGS in the environment replaces the config file's flags outright,
/// which is the cleanest way to opt out.
fn build_user_programs(opts: &Options) {
    for program in USER_PROGRAMS {
        let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
        cmd.current_dir(workspace_root())
            .args(["build", "--package", program, "--target", TARGET])
            .env("RUSTFLAGS", "-C relocation-model=static -C code-model=small");
        if opts.release {
            cmd.arg("--release");
        }

        let status = cmd.status().expect("failed to invoke cargo");
        if !status.success() {
            std::process::exit(status.code().unwrap_or(1));
        }
    }
}

/// Lay out the EFI system partition QEMU will boot from:
///
/// ```text
/// build/esp/EFI/BOOT/BOOTX64.EFI   Limine itself
/// build/esp/EFI/BOOT/limine.conf   boot menu config
/// build/esp/boot/kestrel           our kernel
/// ```
fn assemble_esp(opts: &Options) -> PathBuf {
    let root = workspace_root();
    let esp = root.join("build/esp");

    let kernel = root
        .join("target")
        .join(TARGET)
        .join(profile_dir(opts))
        .join("kernel");
    if !kernel.exists() {
        panic!("kernel binary not found at {}", kernel.display());
    }

    // Start clean. QEMU's `fat:rw:` lets the guest firmware write here â€” OVMF
    // drops an NvVars file for its EFI variables â€” and that must not end up
    // inside a distributed image.
    if esp.exists() {
        std::fs::remove_dir_all(&esp).expect("could not clear the staged ESP");
    }
    std::fs::create_dir_all(esp.join("EFI/BOOT")).unwrap();
    std::fs::create_dir_all(esp.join("boot")).unwrap();
    std::fs::create_dir_all(esp.join("bin")).unwrap();

    copy(&root.join("limine/BOOTX64.EFI"), &esp.join("EFI/BOOT/BOOTX64.EFI"));
    copy(&root.join("limine.conf"), &esp.join("EFI/BOOT/limine.conf"));
    copy(&kernel, &esp.join("boot/kestrel"));

    // User programs, which Limine loads as modules and the kernel exposes
    // under /bin.
    for program in USER_PROGRAMS {
        let binary = root
            .join("target")
            .join(TARGET)
            .join(profile_dir(opts))
            .join(program);
        copy(&binary, &esp.join("bin").join(program));
    }

    esp
}

/// Disk image size. Generous: the debug kernel alone is a few MiB.
const IMAGE_MEGABYTES: u64 = 64;

/// Turn the staged ESP directory into a real GPT + FAT32 disk image, plus a
/// VHD for hypervisors that will not take a raw one.
fn build_image(_opts: &Options) {
    let root = workspace_root();
    let esp = root.join("build/esp");
    let raw = root.join("build/kestrel.img");
    let vhd = root.join("build/kestrel.vhd");

    match image::build(&esp, &raw, IMAGE_MEGABYTES) {
        Ok(size) => println!("disk image   : {} ({} MiB)", raw.display(), size / (1024 * 1024)),
        Err(e) => {
            eprintln!("failed to build the disk image: {e}");
            std::process::exit(1);
        }
    }

    match image::write_vhd(&raw, &vhd) {
        Ok(()) => println!("vhd          : {}", vhd.display()),
        Err(e) => eprintln!("failed to write the VHD: {e}"),
    }
}

/// Build a bootable ISO, for attaching to a VM's virtual DVD drive.
///
/// Returns the path actually written, which may differ from the usual one if
/// that file was locked.
fn build_iso() -> PathBuf {
    let root = workspace_root();
    let esp = root.join("build/esp");
    let target = root.join("build/kestrel.iso");

    match iso::build(&esp, &target) {
        Ok(size) => {
            println!("iso          : {} ({} MiB)", target.display(), size / (1024 * 1024));
            return target;
        }
        // Sharing violation: something else holds the file open, most likely a
        // VM with this ISO still attached to its DVD drive. Write alongside it
        // rather than failing the whole build.
        Err(e) if e.raw_os_error() == Some(32) => {
            let fallback = root.join("build/kestrel.new.iso");
            eprintln!(
                "note: {} is in use (a VM still has it attached?), writing {} instead",
                target.display(),
                fallback.display()
            );

            match iso::build(&esp, &fallback) {
                Ok(size) => {
                    println!("iso          : {} ({} MiB)", fallback.display(), size / (1024 * 1024));
                    return fallback;
                }
                Err(e) => eprintln!("failed to build the ISO: {e}"),
            }
        }
        Err(e) => eprintln!("failed to build the ISO: {e}"),
    }

    std::process::exit(1);
}

fn copy(from: &Path, to: &Path) {
    std::fs::copy(from, to)
        .unwrap_or_else(|e| panic!("copy {} -> {}: {e}", from.display(), to.display()));
}

fn qemu_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("QEMU_DIR") {
        return PathBuf::from(dir);
    }
    let default = PathBuf::from(r"C:\Program Files\qemu");
    if default.exists() {
        return default;
    }
    // Fall back to whatever is on PATH.
    PathBuf::new()
}

fn run_qemu(opts: &Options, iso: Option<&Path>) {
    let root = workspace_root();
    let esp = root.join("build/esp");
    let dir = qemu_dir();

    let exe = if dir.as_os_str().is_empty() {
        PathBuf::from("qemu-system-x86_64")
    } else {
        dir.join("qemu-system-x86_64.exe")
    };

    let firmware = dir.join("share/edk2-x86_64-code.fd");

    let mut cmd = Command::new(&exe);
    cmd.current_dir(&root)
        .args(["-M", "q35"])
        .args(["-m", "512M"])
        .args(["-cpu", "qemu64"])
        .args(["-smp", &opts.cpus.to_string()])
        // User-mode networking: the guest gets 10.0.2.15, the gateway is
        // 10.0.2.2 and DNS 10.0.2.3. No host privileges needed.
        .args(["-netdev", "user,id=net0"])
        .args(["-device", "e1000,netdev=net0"])
        // UEFI firmware. Read-only: we don't need to persist EFI variables.
        .arg("-drive")
        .arg(format!(
            "if=pflash,unit=0,format=raw,readonly=on,file={}",
            firmware.display()
        ))
        // The ISO goes in the virtual DVD drive; everything else is a disk.
        .arg(if opts.from_iso { "-cdrom" } else { "-drive" })
        .arg(if let Some(iso) = iso {
            iso.display().to_string()
        } else if opts.from_vhd {
            // `vpc` is QEMU's name for the VHD format.
            format!("format=vpc,file={}", root.join("build/kestrel.vhd").display())
        } else if opts.from_image {
            format!("format=raw,file={}", root.join("build/kestrel.img").display())
        } else {
            format!("format=raw,file=fat:rw:{}", esp.display())
        })
        // A triple fault should stop the machine, not silently reboot into a
        // loop that looks like a hang.
        .arg("-no-reboot")
        .arg("-no-shutdown");

    // Recording every frame the guest sends and receives is the only honest
    // way to check a network stack: the guest's own claims prove nothing.
    if let Some(pcap) = opts.pcap.as_ref() {
        cmd.arg("-object").arg(format!(
            "filter-dump,id=dump0,netdev=net0,file={}",
            pcap.display()
        ));
    }

    let capturing = opts.screenshot.is_some();
    let needs_monitor = capturing || opts.keys.is_some();
    let bounded = opts.headless || needs_monitor || opts.serial_keys.is_some();

    if opts.serial_keys.is_some() {
        // A socket, so the test can both type at the guest and read back what
        // it prints — exactly the shape of a Hyper-V named-pipe COM port.
        cmd.args(["-display", "none"]);
        cmd.arg("-serial")
            .arg(format!("tcp:127.0.0.1:{SERIAL_PORT},server,nowait"));
    } else if opts.headless || capturing {
        cmd.args(["-display", "none"]);
        cmd.arg("-serial").arg(format!(
            "file:{}",
            root.join("build/serial.log").display()
        ));
    } else {
        cmd.args(["-serial", "stdio"]);
    }

    if needs_monitor {
        cmd.arg("-monitor").arg(format!(
            "telnet:127.0.0.1:{MONITOR_PORT},server,nowait"
        ));
    }

    // A kernel that halts never exits QEMU, so any non-interactive run must be
    // bounded by the timeout instead of waiting on the process.
    if bounded {
        let mut child = cmd.stdin(Stdio::null()).spawn().expect("failed to start qemu");

        // Let the guest finish booting before typing at it.
        std::thread::sleep(Duration::from_secs(opts.timeout));

        if let Some(text) = opts.serial_keys.as_deref() {
            match drive_serial(text) {
                Ok(output) => {
                    println!("--- serial console ---");
                    println!("{output}");
                }
                Err(e) => eprintln!("serial console failed: {e}"),
            }
        }

        if let Some(keys) = opts.keys.as_deref() {
            match send_keys(keys) {
                Ok(count) => println!("sent {count} keystrokes"),
                Err(e) => eprintln!("sending keys failed: {e}"),
            }
            // Give the guest a moment to render what it received.
            std::thread::sleep(Duration::from_millis(500));
        }

        if let Some(target) = opts.screenshot.as_ref() {
            match capture(target) {
                Ok(()) => println!("screenshot written to {}", target.display()),
                Err(e) => eprintln!("screenshot failed: {e}"),
            }
        }

        // If `quit` went out on the monitor, give QEMU a moment to exit on its
        // own before forcing it.
        std::thread::sleep(Duration::from_millis(500));
        child.kill().ok();
        child.wait().ok();
        println!("serial log: {}", root.join("build/serial.log").display());
    } else {
        let status = cmd.status().expect("failed to start qemu");
        if !status.success() {
            std::process::exit(status.code().unwrap_or(1));
        }
    }
}

/// Type `text` into the guest using the monitor's `sendkey` command, which
/// injects real PS/2 scancodes â€” so this exercises the kernel's keyboard
/// driver exactly as a human would.
fn send_keys(text: &str) -> std::io::Result<usize> {
    let mut stream = TcpStream::connect(("127.0.0.1", MONITOR_PORT))?;

    // The monitor emits a telnet negotiation and a banner when a client
    // connects, and discards anything sent before it has finished. Without
    // this pause the first few keystrokes vanish.
    std::thread::sleep(Duration::from_millis(500));

    let mut sent = 0;

    // `{up}` and friends name keys that have no character, so they can be
    // written inline: "abc{left}{left}X".
    let mut rest = text;
    while !rest.is_empty() {
        let key = if let Some(tail) = rest.strip_prefix('{') {
            let Some(end) = tail.find('}') else { break };
            let name = &tail[..end];
            rest = &tail[end + 1..];

            // `{mouse:dx,dy}` and `{click}` drive the pointer rather than the
            // keyboard, so a desktop can be exercised the same way.
            if let Some(delta) = name.strip_prefix("mouse:") {
                writeln!(stream, "mouse_move {}", delta.replace(',', " "))?;
                stream.flush()?;
                std::thread::sleep(Duration::from_millis(40));
                sent += 1;
                continue;
            }
            if name == "click" || name == "release" {
                let buttons = if name == "click" { 1 } else { 0 };
                writeln!(stream, "mouse_button {buttons}")?;
                stream.flush()?;
                std::thread::sleep(Duration::from_millis(60));
                sent += 1;
                continue;
            }

            match name {
                "up" | "down" | "left" | "right" | "home" | "end" => Some(name.to_string()),
                "del" => Some("delete".into()),
                "bs" => Some("backspace".into()),
                "esc" => Some("esc".into()),
                // Loudly, rather than silently dropping it: a misspelled key
                // name looks exactly like the guest ignoring the keystroke,
                // and that has already cost a full test run.
                other => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!(
                            "unknown key name '{{{other}}}' in --keys; known: \
                             up down left right home end del bs click release mouse:dx,dy"
                        ),
                    ))
                }
            }
        } else {
            let mut chars = rest.chars();
            let c = chars.next().unwrap();
            rest = chars.as_str();
            qemu_key_name(c)
        };

        let Some(key) = key else { continue };
        writeln!(stream, "sendkey {key}")?;
        stream.flush()?;
        sent += 1;
        // The guest needs to service each keyboard interrupt before the next
        // scancode arrives, or the controller drops it.
        std::thread::sleep(Duration::from_millis(40));
    }

    Ok(sent)
}

/// Type at the guest over its serial port, then read back what it printed.
///
/// This is how a machine with no PS/2 keyboard is driven, and mirrors
/// attaching a terminal to a Hyper-V COM port.
fn drive_serial(text: &str) -> std::io::Result<String> {
    use std::io::Read;

    let mut stream = TcpStream::connect(("127.0.0.1", SERIAL_PORT))?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;

    // Drain the boot log so only the session output comes back.
    let mut discard = [0u8; 8192];
    while let Ok(read) = stream.read(&mut discard) {
        if read == 0 {
            break;
        }
    }

    for byte in text.bytes() {
        stream.write_all(&[byte])?;
        stream.flush()?;
        // Give the shell time to echo before the next character.
        std::thread::sleep(Duration::from_millis(20));
    }

    std::thread::sleep(Duration::from_millis(500));

    let mut output = Vec::new();
    let mut buffer = [0u8; 8192];
    while let Ok(read) = stream.read(&mut buffer) {
        if read == 0 {
            break;
        }
        output.extend_from_slice(&buffer[..read]);
    }

    Ok(String::from_utf8_lossy(&output).into_owned())
}

/// Map a character to QEMU's key name, which is neither ASCII nor a scancode.
fn qemu_key_name(c: char) -> Option<String> {
    let name = match c {
        'a'..='z' | '0'..='9' => c.to_string(),
        'A'..='Z' => return Some(format!("shift-{}", c.to_ascii_lowercase())),
        ' ' => "spc".into(),
        '\n' => "ret".into(),
        '.' => "dot".into(),
        ',' => "comma".into(),
        '-' => "minus".into(),
        '/' => "slash".into(),
        ';' => "semicolon".into(),
        '=' => "equal".into(),
        // Shifted punctuation, which QEMU names by the unshifted key.
        ':' => "shift-semicolon".into(),
        '_' => "shift-minus".into(),
        '?' => "shift-slash".into(),
        '~' => "shift-grave_accent".into(),
        _ => return None,
    };
    Some(name)
}

/// Drive the QEMU monitor over telnet to dump the framebuffer, then convert
/// the result to PNG if that's what was asked for.
fn capture(target: &Path) -> std::io::Result<()> {
    let wants_png = target
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("png"));
    let ppm = if wants_png {
        target.with_extension("ppm")
    } else {
        target.to_path_buf()
    };

    if let Some(parent) = ppm.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::remove_file(&ppm).ok();

    let mut stream = TcpStream::connect(("127.0.0.1", MONITOR_PORT))?;

    // The monitor is a telnet stream carrying option negotiation bytes, so
    // never try to read it as text. Instead, watch the output file settle.
    let path = ppm.display().to_string().replace('\\', "/");
    writeln!(stream, "screendump {path}")?;
    stream.flush()?;

    wait_until_written(&ppm)?;

    writeln!(stream, "quit")?;
    stream.flush()?;

    if wants_png {
        let (w, h) = png::ppm_to_png(&ppm, target)?;
        std::fs::remove_file(&ppm).ok();
        println!("captured {w}x{h}");
    }
    Ok(())
}

/// Poll until the screendump exists and has stopped growing.
fn wait_until_written(path: &Path) -> std::io::Result<()> {
    let mut previous = 0;
    let mut stable = 0;

    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(100));
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

        if size > 0 && size == previous {
            stable += 1;
            if stable >= 3 {
                return Ok(());
            }
        } else {
            stable = 0;
        }
        previous = size;
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "screendump never completed",
    ))
}





