//! A smoke suite that boots the kernel and checks what it says.
//!
//! Every check runs against a real guest over the serial console: the same
//! path a person on a machine with no PS/2 controller would use. Nothing here
//! inspects the kernel's internals, because a self-consistent bug is invisible
//! to the code that contains it — the suite only reads what the system chose
//! to print.
//!
//! One boot serves every check. Booting per check would be honest but takes
//! about twelve seconds each, and the point of a smoke suite is that it is
//! cheap enough to run before every commit.
//!
//! ## Waiting for the prompt, not for silence
//!
//! `drive_serial` waits for the guest to go quiet for twenty seconds, which is
//! the right answer when you do not know what you asked for — it is what
//! stopped a slow download from looking like a hang. Here we do know: the
//! shell prints its prompt when it is ready for the next command, so each
//! check reads until the prompt comes back. That turns a suite that would take
//! seven minutes into one that takes a few seconds, and it makes a genuine
//! hang show up as a timeout on the guilty command rather than as a long wait
//! with no attribution.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// What the shell prints when it wants a command. `PROMPT` in `shell.rs`.
const PROMPT: &str = "kestrel:";

/// How long any one command may take before it is called hung.
///
/// Generous because `fetch` really does take a while on this stack, and a
/// false failure in a smoke suite is worse than a slow one.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(90);

/// How long to wait for the guest to reach its first prompt.
const BOOT_TIMEOUT: Duration = Duration::from_secs(90);

pub struct Check {
    pub name: &'static str,
    /// Commands to run first, whose output is not examined.
    pub setup: &'static [&'static str],
    pub command: &'static str,
    /// Substrings that must all appear in the output.
    pub expect: &'static [&'static str],
    /// Substrings that must not appear.
    pub reject: &'static [&'static str],
    /// Needs working networking, so it is skipped with `--skip-net`.
    pub network: bool,
}

/// A check with the common fields defaulted, so the table stays readable.
const fn check(name: &'static str, command: &'static str, expect: &'static [&'static str]) -> Check {
    Check { name, setup: &[], command, expect, reject: &[], network: false }
}

/// Every string here was copied from a real run rather than guessed, so a
/// failure means the behaviour changed and not that the expectation was
/// wishful.
pub const CHECKS: &[Check] = &[
    // Note what this does *not* check. The em dash that shipped in 0.9.1
    // rendered as question marks on the framebuffer, and the obvious test —
    // reject `??` here — was tried and does not work: this suite drives the
    // serial console, which passes UTF-8 through untouched, so the defect is
    // invisible from where these checks stand. `lint::non_ascii` catches it at
    // the source level instead. Left as a warning against adding a reject
    // pattern here and believing it covers the font.
    check(
        "version reports itself",
        "version",
        &["Kestrel v", "beta", "experimental x86-64 operating system"],
    ),
    check("uname", "uname", &["Kestrel v", "x86_64"]),

    // Four cores actually running, not merely detected. The kernel is SMP and
    // a single-core regression would otherwise pass everything else here.
    check("all cores online", "cpus", &["4 of 4 cores online"]),
    check("scheduler has per-core idle tasks", "ps", &["kmain", "idle1", "idle2", "idle3"]),

    check("memory reported", "mem", &["physical", "usable", "heap"]),
    check("clock runs", "uptime", &["up ", "ticks at 100 Hz"]),
    check("ramdisk mounted", "ls /", &["etc"]),
    check("motd readable", "cat /etc/motd", &["Kestrel"]),

    // Write then read back through the VFS. Two commands so a filesystem that
    // accepts writes and loses them cannot pass.
    Check {
        name: "files round-trip through the VFS",
        setup: &["write /smoke.txt hello-kestrel"],
        command: "cat /smoke.txt",
        expect: &["hello-kestrel"],
        reject: &["no such file"],
        network: false,
    },

    // The repository is handed to the kernel as boot modules, so an empty
    // catalogue means the ESP was assembled wrong.
    check("repository is populated", "store", &["hello", "basic", "paint", "store install"]),

    // The whole userspace path in one line: install, ELF load, ring 3, exit.
    Check {
        name: "a program loads and runs in ring 3",
        setup: &["store install hello"],
        command: "exec hello",
        expect: &["hello from a real ELF binary in ring 3", "done"],
        reject: &["not installed", "panic"],
        network: false,
    },

    // Ring 3 must not reach kernel memory. If this stops failing, the user
    // half of the address space has lost its protection.
    Check {
        name: "ring 3 cannot read kernel memory",
        setup: &["store install fault"],
        command: "exec fault",
        expect: &["segmentation fault", "0xffffffff80000000", "killed", "exit 139"],
        reject: &[],
        network: false,
    },

    // W^X, enforced by the NX bit, and per-core: enabling it only on the boot
    // processor made this pass on core 0 and nowhere else.
    Check {
        name: "a program cannot rewrite its own code",
        setup: &["store install selfmod"],
        command: "exec selfmod",
        expect: &["segmentation fault", "killed", "exit 139"],
        reject: &[],
        network: false,
    },

    // Settings are one table; `set` and `theme` must agree about it.
    Check {
        name: "settings round-trip",
        setup: &["set text.panel #ff8800"],
        command: "theme",
        expect: &["text.panel", "#ff8800"],
        reject: &[],
        network: false,
    },
    check("bad colours are refused", "set text.panel notacolour", &["is not a colour"]),

    // Networking. The card, then ARP, then DNS: each depends on the last, so
    // running all three attributes a failure to a layer.
    // An 82540 will not transmit while the link is down, and the descriptor
    // is simply never completed — indistinguishable from a broken driver. So
    // assert on the negotiated link, not on the card being present.
    Check {
        name: "network card has negotiated link",
        setup: &[],
        command: "nic",
        expect: &["link", "1000 Mb/s", "(enabled)"],
        reject: &[],
        network: true,
    },
    Check {
        name: "DNS resolves",
        setup: &[],
        command: "resolve example.com",
        expect: &["example.com is "],
        reject: &["timed out", "failed"],
        network: true,
    },
    Check {
        name: "TCP fetches a real page",
        setup: &[],
        command: "fetch http://example.com/",
        expect: &["Example Domain"],
        reject: &["timed out", "failed"],
        network: true,
    },

    // The update loop, short of installing. The suite spins up the same server
    // a person would (`cargo xtask serve`) and asks the guest to describe what
    // it offers; installing is deliberately not tested here because a debug
    // kernel is megabytes and would take the whole suite hostage over the
    // kernel's own slow TCP. This proves the fetch, the manifest parse and the
    // offer listing all work against a real payload.
    Check {
        name: "an update is offered over HTTP",
        setup: &["update from http://10.0.2.2:8088/"],
        command: "update",
        expect: &["offered", "a kernel of ", "packages"],
        reject: &["update: ", "timed out"],
        network: true,
    },
];

/// The URL above hardcodes the port the suite serves on; this is the single
/// real source of it, so changing one and not the other fails to compile
/// rather than failing a test that mentions the port in a confusing way.
const _: () = assert!(super::serve::DEFAULT_PORT == 8088);

/// A live serial console on the running guest.
struct Session {
    stream: TcpStream,
    /// Anything read past the end of the last command's output.
    pending: String,
}

impl Session {
    /// Connect, then read the boot log until the shell offers a prompt.
    ///
    /// QEMU's socket does not exist until QEMU does, so connecting retries.
    /// Waiting for the prompt rather than sleeping a fixed number of seconds
    /// is what keeps the suite correct on a slow host and quick on a fast one.
    fn connect() -> Result<Self, String> {
        let started = Instant::now();

        let stream = loop {
            match TcpStream::connect(("127.0.0.1", super::SERIAL_PORT)) {
                Ok(stream) => break stream,
                Err(e) if started.elapsed() > BOOT_TIMEOUT => {
                    return Err(format!("could not reach the guest's serial port: {e}"))
                }
                Err(_) => std::thread::sleep(Duration::from_millis(200)),
            }
        };

        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .map_err(|e| e.to_string())?;

        let mut session = Session { stream, pending: String::new() };
        session.read_until_prompt(BOOT_TIMEOUT).map_err(|partial| {
            format!(
                "the guest never reached a shell prompt. Boot log:
{}",
                indent(&partial)
            )
        })?;

        Ok(session)
    }

    /// Read until the last line is a prompt, or give up.
    fn read_until_prompt(&mut self, limit: Duration) -> Result<String, String> {
        let started = Instant::now();
        let mut buffer = [0u8; 8192];

        loop {
            if at_prompt(&self.pending) {
                let text = std::mem::take(&mut self.pending);
                return Ok(text);
            }

            if started.elapsed() > limit {
                return Err(std::mem::take(&mut self.pending));
            }

            match self.stream.read(&mut buffer) {
                Ok(0) => return Err(std::mem::take(&mut self.pending)),
                Ok(read) => self
                    .pending
                    .push_str(&String::from_utf8_lossy(&buffer[..read])),
                // A read timeout is silence, not an error: the guest is
                // thinking. Only the overall limit ends the wait.
                Err(_) => continue,
            }
        }
    }

    /// Type a command and return what it printed.
    ///
    /// The echoed command and the trailing prompt are stripped, so a check's
    /// expectations are matched against output rather than against its own
    /// command text — otherwise `expect` would match the echo and every check
    /// would pass.
    fn run(&mut self, command: &str) -> Result<String, String> {
        for byte in command.bytes().chain(std::iter::once(b'\n')) {
            self.stream.write_all(&[byte]).map_err(|e| e.to_string())?;
            self.stream.flush().map_err(|e| e.to_string())?;
            // The shell echoes as it receives; typing faster than it echoes
            // drops characters, including the newline.
            std::thread::sleep(Duration::from_millis(20));
        }

        let raw = self
            .read_until_prompt(COMMAND_TIMEOUT)
            .map_err(|partial| format!("timed out after {COMMAND_TIMEOUT:?}; got:\n{}", indent(&partial)))?;

        Ok(strip(&raw, command))
    }
}

/// Is the last line of this text a shell prompt waiting for input?
fn at_prompt(text: &str) -> bool {
    match text.trim_end_matches([' ', '\r']).lines().last() {
        Some(line) => line.starts_with(PROMPT) && line.ends_with('$'),
        None => false,
    }
}

/// Drop the echoed command and the trailing prompt.
fn strip(raw: &str, command: &str) -> String {
    let mut lines: Vec<&str> = raw.lines().collect();

    if lines.first().map(|l| l.trim() == command).unwrap_or(false) {
        lines.remove(0);
    }
    while lines.last().map(|l| l.trim_start().starts_with(PROMPT)).unwrap_or(false) {
        lines.pop();
    }

    lines.join("\n").trim_matches(['\r', '\n']).to_string()
}

fn indent(text: &str) -> String {
    text.lines().map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n")
}

/// Run every check against a guest that is already booting.
///
/// Returns the number that failed.
pub fn run(skip_network: bool) -> usize {
    let mut session = match Session::connect() {
        Ok(session) => session,
        Err(e) => {
            eprintln!("FAIL  could not start a session: {e}");
            return 1;
        }
    };

    let mut failed = 0;
    let mut skipped = 0;

    for case in CHECKS {
        if case.network && skip_network {
            println!("skip  {}", case.name);
            skipped += 1;
            continue;
        }

        for setup in case.setup {
            if let Err(e) = session.run(setup) {
                eprintln!("FAIL  {}\n  setup `{setup}` failed: {e}", case.name);
                failed += 1;
                continue;
            }
        }

        let output = match session.run(case.command) {
            Ok(output) => output,
            Err(e) => {
                eprintln!("FAIL  {}\n  `{}`: {e}", case.name, case.command);
                failed += 1;
                continue;
            }
        };

        let missing: Vec<_> = case.expect.iter().filter(|e| !output.contains(**e)).collect();
        let present: Vec<_> = case.reject.iter().filter(|r| output.contains(**r)).collect();

        if missing.is_empty() && present.is_empty() {
            println!("ok    {}", case.name);
            continue;
        }

        failed += 1;
        eprintln!("FAIL  {}", case.name);
        eprintln!("  command: {}", case.command);
        for expected in missing {
            eprintln!("  expected but missing: {expected:?}");
        }
        for rejected in present {
            eprintln!("  present but rejected: {rejected:?}");
        }
        eprintln!("  output:\n{}", indent(&output));
    }

    let total = CHECKS.len();
    println!();
    if failed == 0 {
        println!("{} passed, {skipped} skipped", total - skipped);
    } else {
        println!("{failed} failed, {} passed, {skipped} skipped", total - failed - skipped);
    }

    failed
}
