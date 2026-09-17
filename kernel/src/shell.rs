//! An interactive shell running as a kernel task.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::{allocator, apic, cpu, keyboard, memory, print, println, task, terminal, vfs};

const PROMPT: &str = "kestrel:";


pub struct Shell {
    line: String,
    /// Insert position within `line`. Input is ASCII, so bytes and characters
    /// are interchangeable here.
    cursor: usize,
    cwd: String,
    history: Vec<String>,
    /// Which history entry is being shown, if the user is browsing.
    browsing: Option<usize>,
    /// The half-typed line set aside while browsing history.
    stashed: String,
    /// Length last drawn, so a shorter line can be padded over.
    drawn: usize,
    /// Set while the shell is running inside the desktop's terminal window.
    in_desktop: bool,
    /// The terminal window's contents, while the desktop is up.
    ///
    /// Held here rather than as a local of `desktop` because waiting on a
    /// program has to keep drawing: the desktop loop and the shell are one
    /// task, so a blocking wait inside `on_key` would otherwise freeze the
    /// screen until the program exited.
    terminal: Option<terminal::Terminal>,
}

impl Shell {
    pub fn new() -> Self {
        Self {
            line: String::new(),
            cursor: 0,
            cwd: String::from("/"),
            history: Vec::new(),
            browsing: None,
            stashed: String::new(),
            drawn: 0,
            in_desktop: false,
            terminal: None,
        }
    }

    /// Resolve a user-supplied path against the working directory.
    fn resolve(&self, path: &str) -> String {
        if path.starts_with('/') {
            path.to_string()
        } else if self.cwd == "/" {
            alloc::format!("/{path}")
        } else {
            alloc::format!("{}/{path}", self.cwd)
        }
    }

    fn prompt(&self) {
        print!("{PROMPT}{}$ ", self.cwd);
    }

    /// Repaint the current line and put the cursor back where it belongs.
    ///
    /// The console has no cursor addressing, so this rewrites the line from
    /// the start, pads over anything left by a longer previous line, and then
    /// walks the cursor back.
    fn redraw(&mut self) {
        print!("\r");
        self.prompt();
        print!("{}", self.line);

        let padding = self.drawn.saturating_sub(self.line.len());
        for _ in 0..padding {
            print!(" ");
        }

        let back = padding + (self.line.len() - self.cursor);
        if back > 0 {
            print::move_left(back);
        }

        self.drawn = self.line.len();
    }

    fn replace_line(&mut self, text: String) {
        self.line = text;
        self.cursor = self.line.len();
        self.redraw();
    }

    /// Step back and forward through previously entered commands.
    fn recall(&mut self, back: bool) {
        if self.history.is_empty() {
            return;
        }

        match (self.browsing, back) {
            // Entering history: keep whatever was half-typed.
            (None, true) => {
                self.stashed = self.line.clone();
                self.browsing = Some(self.history.len() - 1);
            }
            (Some(index), true) if index > 0 => self.browsing = Some(index - 1),
            (Some(index), false) if index + 1 < self.history.len() => {
                self.browsing = Some(index + 1)
            }
            // Past the newest entry: restore what the user was typing.
            (Some(_), false) => {
                self.browsing = None;
                let stashed = core::mem::take(&mut self.stashed);
                self.replace_line(stashed);
                return;
            }
            _ => return,
        }

        let entry = self.history[self.browsing.unwrap()].clone();
        self.replace_line(entry);
    }

    pub fn run(&mut self) -> ! {
        if let Some(Ok(motd)) = vfs::with(|fs| fs.read("/etc/motd").map(|d| d.to_vec())) {
            print!("{}", String::from_utf8_lossy(&motd));
        }
        println!();

        // The default experience is the desktop; the console is one Escape
        // away, or forever away with `set desktop.boot off` and a reboot.
        if crate::theme::current().boot_to_desktop {
            self.desktop();
        }
        self.prompt();

        loop {
            while let Some(byte) = keyboard::read() {
                self.on_key(byte);
            }

            // The serial console is an equal input source, and the only one on
            // machines with no PS/2 controller.
            while let Some(byte) = crate::serial::read() {
                self.on_key(byte);
            }

            // Collect anything that has exited since the last pass.
            task::reap();

            // The network card is polled, so frames only arrive when asked for.
            crate::net::poll();

            // Nothing to do until the next interrupt; let other tasks run.
            task::yield_now();
            x86_64::instructions::hlt();
        }
    }

    fn on_key(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                println!();

                let line = core::mem::take(&mut self.line);
                let trimmed = line.trim();

                // Remember it, skipping blanks and immediate repeats.
                if !trimmed.is_empty() && self.history.last().map(String::as_str) != Some(trimmed) {
                    self.history.push(trimmed.to_string());
                }

                self.cursor = 0;
                self.drawn = 0;
                self.browsing = None;

                self.execute(trimmed);
                self.prompt();
            }

            0x08 => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.line.remove(self.cursor);
                    self.redraw();
                }
            }
            keyboard::KEY_DELETE => {
                if self.cursor < self.line.len() {
                    self.line.remove(self.cursor);
                    self.redraw();
                }
            }

            keyboard::KEY_LEFT => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    print::move_left(1);
                }
            }
            keyboard::KEY_RIGHT => {
                if self.cursor < self.line.len() {
                    // Reprinting the character is how the cursor moves right.
                    let next = self.line.as_bytes()[self.cursor] as char;
                    print!("{next}");
                    self.cursor += 1;
                }
            }
            keyboard::KEY_HOME => {
                self.cursor = 0;
                self.redraw();
            }
            keyboard::KEY_END => {
                self.cursor = self.line.len();
                self.redraw();
            }

            keyboard::KEY_UP => self.recall(true),
            keyboard::KEY_DOWN => self.recall(false),

            byte if byte.is_ascii_graphic() || byte == b' ' => {
                self.line.insert(self.cursor, byte as char);
                self.cursor += 1;

                // Typing at the end is the common case and needs no repaint.
                if self.cursor == self.line.len() {
                    print!("{}", byte as char);
                    self.drawn = self.line.len();
                } else {
                    self.redraw();
                }
            }

            _ => {}
        }
    }

    fn execute(&mut self, line: &str) {
        if line.is_empty() {
            return;
        }

        let mut words = line.split_whitespace();
        let Some(command) = words.next() else { return };
        let args: Vec<&str> = words.collect();

        match command {
            "help" => self.help(),
            "ls" => self.ls(&args),
            "cat" => self.cat(&args),
            "write" => self.write(&args, false),
            "append" => self.write(&args, true),
            "mkdir" => self.mkdir(&args),
            "rm" => self.rm(&args),
            "cd" => self.cd(&args),
            "pwd" => println!("{}", self.cwd),
            "echo" => println!("{}", args.join(" ")),
            "ps" => self.ps(),
            "cpus" => self.cpus(),
            "desktop" => self.desktop(),
            "net" => self.net(),
            "nic" => self.nic(),
            "open" => self.open_command(&args),
            "store" => self.store_command(&args),
            "update" => self.update_command(&args),
            "theme" => self.theme_command(&args),
            "set" => self.set_command(&args),
            "arp" => self.arp(&args),
            "ping" => self.ping(&args),
            "fetch" => self.fetch(&args),
            "resolve" => self.dns_lookup(&args),
            "spin" => self.spin(),
            "exec" => self.exec(&args),
            "kill" => self.kill(&args),
            "history" => {
                for (index, entry) in self.history.iter().enumerate() {
                    println!("  {:>3}  {entry}", index + 1);
                }
            }
            "reboot" => crate::power::reboot(),
            "shutdown" | "poweroff" => crate::power::shutdown(),
            "mem" => self.mem(),
            "uptime" => self.uptime(),
            "date" | "time" => self.date(),
            "notify" => self.notify_command(&args),
            "uname" => println!("Kestrel {} x86_64", crate::VERSION),
            "version" => self.version(),
            "clear" => print::clear(),
            other => println!("{other}: command not found (try 'help')"),
        }
    }

    /// What this build is, and what a beta tester should expect of it.
    fn version(&self) {
        println!(
            "Kestrel {} \"{}\" - an experimental x86-64 operating system",
            crate::VERSION,
            crate::CODENAME
        );
        println!();
        println!("  built     {} profile", if cfg!(debug_assertions) { "debug" } else { "release" });
        println!("  cores     {} of {} online", cpu::online(), cpu::count());
        println!("  memory    {} MiB usable", memory::usable_bytes() / (1024 * 1024));
        match crate::net::config(|c| c.ip) {
            Some(ip) => println!("  network   {ip}"),
            None => println!("  network   not configured"),
        }
        println!();
        println!("This is beta software written from scratch. It has no security");
        println!("model, no filesystem checks, and no data recovery. Run it in a");
        println!("virtual machine, and do not point it at a disk you care about.");
    }

    fn help(&self) {
        println!("commands:");
        println!("  ls [path]           list a directory");
        println!("  cat <file>          print a file");
        println!("  write <file> <text> create or overwrite a file");
        println!("  append <file> <text>  append a line to a file");
        println!("  mkdir <path>        create a directory");
        println!("  rm <path>           remove a file or directory");
        println!("  cd <path>           change directory");
        println!("  pwd                 print working directory");
        println!("  echo <text>         print text");
        println!("  ps                  list tasks");
        println!("  cpus                list processor cores");
        println!("  net                 network interface status");
        println!("  nic                 network card registers, for diagnosis");
        println!("  open <window>       terminal, monitor, settings or software");
        println!("  theme               show, preset, save, load or reset the look");
        println!("  set <name> <value>  change one appearance setting");
        println!("  arp [ip]            show or request address resolution");
        println!("  kill <id>           stop a task");
        println!("  history             previously entered commands");
        println!("  reboot / shutdown   stop the machine");
        println!("  spin                spawn a busy task, to show preemption");
        println!("  store               browse, install and remove programs");
        println!("  update              replace the system with a newer one");
        println!("                      (/bin is in RAM, /disk is the real disk)");
        println!("  mem                 memory statistics");
        println!("  uptime              time since boot");
        println!("  date                the wall clock, from the RTC");
        println!("  notify <message>    raise a desktop notification");
        println!("  uname               kernel version");
        println!("  version             release, build and what to expect of it");
        println!("  clear               clear the screen");
    }

    fn ls(&self, args: &[&str]) {
        let path = self.resolve(args.first().copied().unwrap_or("."));

        match vfs::list(&path) {
            Ok(entries) => {
                for entry in entries {
                    if entry.is_directory {
                        println!("  {:<20} <dir>", entry.name);
                    } else {
                        println!("  {:<20} {:>8} bytes", entry.name, entry.size);
                    }
                }
            }
            Err(e) => println!("ls: {path}: {}", e.as_str()),
        }
    }

    fn cat(&self, args: &[&str]) {
        let Some(name) = args.first() else {
            println!("usage: cat <file>");
            return;
        };
        let path = self.resolve(name);

        match vfs::read(&path) {
            Ok(data) => print!("{}", String::from_utf8_lossy(&data)),
            Err(e) => println!("cat: {path}: {}", e.as_str()),
        }
    }

    fn write(&self, args: &[&str], append: bool) {
        let Some(name) = args.first() else {
            println!("usage: write <file> <text>");
            return;
        };
        let path = self.resolve(name);

        if let Err(e) = vfs::writable(&path) {
            println!("write: {path}: {}", e.as_str());
            return;
        }

        let mut text = args[1..].join(" ");
        text.push('\n');

        let result = if append {
            vfs::append(&path, text.as_bytes())
        } else {
            vfs::write(&path, text.as_bytes())
        };

        if let Err(e) = result {
            println!("write: {path}: {}", e.as_str());
        }
    }

    fn mkdir(&self, args: &[&str]) {
        let Some(name) = args.first() else {
            println!("usage: mkdir <path>");
            return;
        };
        let path = self.resolve(name);

        if let Err(e) = vfs::mkdir(&path) {
            println!("mkdir: {path}: {}", e.as_str());
        }
    }

    fn rm(&self, args: &[&str]) {
        let Some(name) = args.first() else {
            println!("usage: rm <path>");
            return;
        };
        let path = self.resolve(name);

        if let Err(e) = vfs::remove(&path) {
            println!("rm: {path}: {}", e.as_str());
        }
    }

    fn cd(&mut self, args: &[&str]) {
        let target = self.resolve(args.first().copied().unwrap_or("/"));

        // Normalise through the VFS so `..` and `.` are collapsed the same way
        // path lookups collapse them.
        if !vfs::exists(&target) {
            println!("cd: {target}: no such file or directory");
        } else if !vfs::is_directory(&target) {
            println!("cd: {target}: not a directory");
        } else {
            self.cwd = normalise(&target);
        }
    }

    fn ps(&self) {
        let (tasks, switches) = x86_64::instructions::interrupts::without_interrupts(|| {
            let scheduler = task::SCHEDULER.lock();
            (scheduler.describe(), scheduler.switches())
        });

        println!(
            "  {:<4} {:<12} {:<4} {:<5} {:<10} {:>8}",
            "ID", "NAME", "CPU", "RING", "STATE", "QUANTA"
        );
        for info in tasks {
            let state = if info.state == task::State::Finished {
                alloc::format!("exit {}", info.exit_code)
            } else {
                info.state.as_str().to_string()
            };

            println!(
                "  {:<4} {:<12} {:<4} {:<5} {:<10} {:>8}{}",
                info.id,
                info.name,
                info.cpu,
                if info.user { 3 } else { 0 },
                state,
                info.quanta,
                if info.running { "  <- running" } else { "" }
            );
        }
        println!(
            "  {switches} switches from {} calls ({} sole task, {} lock busy, {} no candidate)",
            task::SCHEDULE_CALLS.load(Ordering::Relaxed),
            task::NOTHING_ELSE.load(Ordering::Relaxed),
            task::LOCK_FAILURES.load(Ordering::Relaxed),
            task::NO_CANDIDATE.load(Ordering::Relaxed),
        );
    }

    fn exec(&mut self, args: &[&str]) {
        let Some(name) = args.first() else {
            println!("usage: exec <program>   ('store' lists what can be installed)");
            return;
        };

        // A bare name means an installed program, so `exec snake` works
        // without anyone having to know where installing put it. A name with a
        // slash is a path and is taken literally.
        let path = match name.contains('/') {
            true => self.resolve(name),
            false => match crate::store::resolve(name) {
                Some(path) => path,
                None => {
                    if crate::store::catalogue().iter().any(|p| p.name == *name) {
                        println!("exec: {name} is not installed - try 'store install {name}'");
                    } else {
                        println!("exec: no program called {name}");
                    }
                    return;
                }
            },
        };

        let image = match vfs::read(&path) {
            Ok(data) => data,
            Err(e) => {
                println!("exec: {path}: {}", e.as_str());
                return;
            }
        };

        let program = path.rsplit('/').next().unwrap_or(&path).to_string();

        let id = match crate::process::spawn(&program, &image) {
            Ok(id) => id,
            Err(e) => {
                println!("exec: {path}: {}", e.as_str());
                return;
            }
        };

        self.wait_for(id);
    }

    /// Wait for a program to finish, leaving its input alone.
    ///
    /// This is what makes an interactive program possible at all. Without it
    /// the shell keeps reading the keyboard while the program runs, and since
    /// the shell is the one drawing a prompt, every keystroke meant for the
    /// program is eaten by the shell and reported as an unknown command.
    ///
    /// A background form would need job control to hand the input back and
    /// forth; a foreground-only shell needs only this.
    fn wait_for(&mut self, id: u64) {
        loop {
            let outcome = x86_64::instructions::interrupts::without_interrupts(|| {
                task::SCHEDULER.lock().outcome(id)
            });

            if let Some(code) = outcome {
                // Silent on success, like a shell: a program that worked has
                // already said whatever it had to say.
                if code != 0 {
                    println!("[exit {code}]");
                }
                return;
            }

            // Keep the desktop drawing. Without this a program run from the
            // terminal window would freeze the whole screen until it exited,
            // because the compositor is driven by this very task.
            self.pump_desktop();

            // Collect whatever else has finished, but not the task being
            // waited on: reaping it here would throw away the exit code this
            // loop is still polling for, and a program killed by a fault would
            // then be reported as a silent success.
            task::reap_keeping(id);
            task::yield_now();
            x86_64::instructions::hlt();
        }
    }

    /// One pass of the desktop: service the launcher, drain output into the
    /// terminal window, sample the mouse, and repaint.
    ///
    /// Does nothing on the console, so callers do not have to check.
    fn pump_desktop(&mut self) {
        if !self.in_desktop {
            return;
        }

        // Drained outside the lock: a command printing while this task held
        // the DESKTOP lock would deadlock against the compositor.
        let output = terminal::drain();
        let notice = crate::desktop::with(|d| d.take_notice()).flatten();

        let Some(terminal) = self.terminal.as_mut() else {
            return;
        };

        crate::desktop::with(|desktop| {
            // Anything a drawing program asked for or drew, collected here
            // where the desktop is already held rather than from its syscall.
            desktop.service_surface();

            if let Some(kind) = desktop.take_open_request() {
                let (width, height) = desktop.size();
                desktop.open(build_window(kind, width, height));
            }

            // A resized terminal shows a different number of rows, and the
            // scrollback that fills them lives here rather than in the
            // compositor. Without this the window keeps whatever it had until
            // the next command prints something.
            if desktop.take_terminal_resized() {
                if let Some(index) = desktop.find(crate::desktop::Kind::Terminal) {
                    let rows = desktop.windows[index].rows();
                    desktop.windows[index].lines = terminal.visible(rows);
                    desktop.invalidate_window(index);
                }
            }

            if let Some(bytes) = output {
                terminal.write(&bytes);
                // By role, not by index: closing a window shifts every index
                // after it, and the terminal is not always first.
                if let Some(index) = desktop.find(crate::desktop::Kind::Terminal) {
                    let rows = desktop.windows[index].rows();
                    desktop.windows[index].lines = terminal.visible(rows);
                    desktop.invalidate_window(index);
                }
            }

            // Sample the mouse several times per frame. A full redraw takes
            // long enough that polling once per frame misses short clicks and
            // makes dragging feel like it is snapping.
            for _ in 0..8 {
                desktop.handle_mouse();
            }
            desktop.render();
        });

        if let Some(message) = notice {
            println!("{message}");
        }
    }

    fn cpus(&self) {
        println!(
            "  {:<5} {:<10} {:<9} {:>12}",
            "CPU", "LAPIC ID", "STATE", "TIMER TICKS"
        );

        for index in 0..cpu::count() {
            // A core with no ticks never reached its timer.
            let ticks = cpu::ticks(index);
            println!(
                "  {:<5} {:<10} {:<9} {:>12}",
                index,
                cpu::lapic_id(index),
                if ticks > 0 { "running" } else { "idle" },
                ticks
            );
        }

        println!("  {} of {} cores online", cpu::online(), cpu::count());
    }

    /// Switch to the windowed desktop until Escape is pressed.
    ///
    /// The console and the desktop both own the framebuffer, so only one can
    /// be drawing at a time; this takes it and gives it back on exit.
    fn desktop(&mut self) {
        // `desktop` is dispatched from `on_key`, and the loop below feeds keys
        // straight back into `on_key` — so typing it again inside the desktop
        // would nest a second desktop inside the first.
        if self.in_desktop {
            println!("desktop: already running");
            return;
        }

        let Some(response) = crate::FRAMEBUFFER.response() else {
            println!("desktop: no framebuffer");
            return;
        };
        let Some(fb) = response.framebuffers().first() else {
            println!("desktop: no framebuffer");
            return;
        };

        let mut desktop = unsafe { crate::desktop::Desktop::new(fb) };
        let (width, height) = desktop.size();

        // Settings is available from the launcher but not open at the start;
        // a window nobody asked for is clutter.
        for kind in [crate::desktop::Kind::Terminal, crate::desktop::Kind::Monitor] {
            let window = build_window(kind, width, height);
            desktop.windows.push(window);
        }

        let terminal_rows = desktop
            .find(crate::desktop::Kind::Terminal)
            .map(|index| desktop.windows[index].rows())
            .unwrap_or(24);
        *crate::desktop::DESKTOP.lock() = Some(desktop);

        // From here on, everything the shell prints is staged for the terminal
        // window instead of the framebuffer console.
        self.terminal = Some(terminal::Terminal::new(terminal_rows.max(1) + 200));
        terminal::start_capture();
        self.in_desktop = true;
        crate::desktop::ACTIVE.store(true, core::sync::atomic::Ordering::Relaxed);

        println!("Kestrel terminal - the shell, in a window.");
        println!("Esc returns to the console.");
        println!();
        self.prompt();

        loop {
            // Input is handled outside the desktop lock: `on_key` runs whole
            // commands, and a command that printed while this task held the
            // DESKTOP lock would deadlock against the compositor.
            let mut leaving = false;
            while let Some(byte) = keyboard::read() {
                // The desktop gets first refusal: Escape closes an open menu
                // rather than leaving the desktop out from under it.
                if crate::desktop::take_key(byte) {
                    continue;
                }
                if byte == keyboard::KEY_ESCAPE {
                    leaving = true;
                } else {
                    self.on_key(byte);
                }
            }
            while let Some(byte) = crate::serial::read() {
                if crate::desktop::take_key(byte) {
                    continue;
                }
                if byte == 0x1B {
                    leaving = true;
                } else {
                    self.on_key(byte);
                }
            }
            if leaving {
                break;
            }

            task::reap();
            crate::net::poll();

            // The same pass a waiting `exec` runs, so the screen behaves
            // identically whether or not a program is in the foreground.
            self.pump_desktop();

            // A program picked from the launcher is run here, in the loop,
            // rather than inside `pump_desktop` - `exec` is foreground and
            // pumps the desktop itself while it waits, so starting one from
            // inside the pump would have it call back into itself.
            if let Some(name) = crate::desktop::with(|d| d.take_run_request()).flatten() {
                println!("exec {name}");
                self.execute(&alloc::format!("exec {name}"));
                self.prompt();
            }

            // A program the media offers but nothing has installed yet: the
            // launcher shows it as install-and-run, so `store install` here is
            // answered by `exec` the honest way rather than disguised inside
            // the compositor.
            if let Some(name) = crate::desktop::with(|d| d.take_install_request()).flatten() {
                println!("store install {name}");
                self.execute(&alloc::format!("store install {name}"));
                self.execute(&alloc::format!("exec {name}"));
                self.prompt();
            }

            task::yield_now();
        }

        self.in_desktop = false;
        self.terminal = None;
        crate::desktop::ACTIVE.store(false, core::sync::atomic::Ordering::Relaxed);
        terminal::stop_capture();
        *crate::desktop::DESKTOP.lock() = None;

        // Hand the framebuffer back to the console.
        print::clear();
        println!("Kestrel {} - an experimental x86-64 OS.", crate::VERSION);
        println!("Type 'help' for commands.");
        println!();
        // No prompt here: `on_key` prints one when `execute` returns.
    }

    fn net(&self) {
        let Some((mac, ip, gateway, netmask)) =
            crate::net::config(|c| (c.mac, c.ip, c.gateway, c.netmask))
        else {
            println!("  no network card");
            return;
        };

        println!(
            "  mac      : {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        );
        println!("  address  : {ip}");
        println!("  netmask  : {netmask}");
        println!("  gateway  : {gateway}");

        let link = crate::net::e1000::with(|nic| nic.link_up()).unwrap_or(false);
        println!("  link     : {}", if link { "up" } else { "down" });
    }

    /// Push the live theme at the compositor, if it is running.
    ///
    /// Changing a setting while the desktop is open repaints immediately;
    /// changing it from the console simply takes effect next time. Either way
    /// the caller does not have to care which it is.
    fn refresh_desktop(&self) {
        let theme = crate::theme::current();
        crate::desktop::with(|desktop| desktop.apply_theme(theme));
    }

    /// Replace the running system with a newer one.
    fn update_command(&self, args: &[&str]) {
        use crate::update;

        match (args.first().copied(), args.get(1).copied()) {
            (Some("from"), Some(url)) => match update::set_source(url) {
                Ok(()) => println!("updates will come from {url}"),
                Err(e) => println!("update: {e}"),
            },

            (Some("from"), None) => match update::source() {
                Some(url) => println!("{url}"),
                None => println!("no source set; use: update from http://host/path"),
            },

            (command @ (None | Some("install")), _) => {
                let Some(base) = update::source() else {
                    println!("no update source set.");
                    println!("  update from http://host/path   where to look");
                    println!("  update                         see what is there");
                    println!("  update install                 take it");
                    return;
                };

                println!("checking {base} ...");
                let manifest = match update::check(&base) {
                    Ok(manifest) => manifest,
                    Err(e) => {
                        println!("update: {e}");
                        return;
                    }
                };

                println!("  running   {}", update::running_version());
                println!("  offered   {}", manifest.version);
                println!(
                    "  contents  a kernel of {} bytes and {} packages",
                    manifest.kernel_size,
                    manifest.packages.len()
                );

                if command.is_none() {
                    if manifest.differs() {
                        println!();
                        println!("'update install' to take it");
                    } else {
                        println!();
                        println!("that is what is already running");
                    }
                    return;
                }

                println!();
                println!("downloading; nothing is written until it has all arrived");

                match update::apply(&base, &manifest) {
                    Ok(report) => {
                        println!(
                            "installed {} - a kernel of {} bytes and {} packages",
                            report.version, report.kernel_bytes, report.packages
                        );
                        println!("reboot to start it. the previous kernel is kept, and the");
                        println!("boot menu offers it if the new one does not work.");
                    }
                    Err(e) => {
                        println!("update: {e}");
                        println!("nothing was changed.");
                    }
                }
            }

            (Some(other), _) => println!("update: no such command {other:?}"),
        }
    }

    /// Browse and install what the boot media offers.
    fn store_command(&self, args: &[&str]) {
        use crate::store;

        match (args.first().copied(), args.get(1).copied()) {
            (Some("install"), Some(name)) => match store::install(name) {
                Ok(size) => {
                    println!("installed {name} ({size} bytes) to {}", store::install_root());
                    if !store::persistent() {
                        println!("note: no writable disk, so this lasts until reboot");
                    }
                }
                Err(e) => println!("store: {e}"),
            },

            (Some("remove"), Some(name)) => match store::remove(name) {
                Ok(()) => println!("removed {name}"),
                Err(e) => println!("store: {e}"),
            },

            (Some("install" | "remove"), None) => {
                println!("usage: store install <name> | store remove <name>")
            }

            (Some(other), _) => println!("store: no such command {other:?}"),

            (None, _) => {
                let packages = store::catalogue();
                if packages.is_empty() {
                    println!("  the repository is empty");
                    return;
                }

                for package in &packages {
                    println!(
                        "  {:<10} {:>6}  {:<9} {}",
                        package.name,
                        package.size,
                        if package.installed { "installed" } else { "" },
                        package.summary
                    );
                }

                println!();
                println!("  store install <name>   put one on this system");
                println!("  store remove <name>    take it off again");
                if !store::persistent() {
                    println!("  no writable disk: installs last until reboot");
                }
            }
        }
    }

    /// Open a desktop window by name, for when the mouse is not an option.
    ///
    /// The launcher can do this too, but a keyboard route matters: Hyper-V has
    /// no PS/2 mouse at all, and the serial console has no pointer either.
    fn open_command(&self, args: &[&str]) {
        use crate::desktop::Kind;

        let Some(name) = args.first() else {
            println!("usage: open <terminal|monitor|settings|software>");
            return;
        };

        let kind = match name.to_ascii_lowercase().as_str() {
            "terminal" | "shell" => Kind::Terminal,
            "monitor" | "system" => Kind::Monitor,
            "settings" => Kind::Settings,
            "software" | "store" => Kind::Software,
            other => {
                println!("open: no window called {other:?}");
                return;
            }
        };

        if !self.in_desktop {
            println!("open: only inside the desktop - run 'desktop' first");
            return;
        }

        // Routed through the same request the launcher uses, so there is one
        // path that opens a window rather than two that can disagree.
        crate::desktop::with(|desktop| desktop.request_open(kind));
        println!("opening {}", kind.title());
    }

    fn theme_command(&self, args: &[&str]) {
        match args.first().copied() {
            // No argument: show everything, which doubles as the list of names
            // `set` will accept.
            None => {
                for key in crate::theme::KEYS {
                    if let Some(value) = crate::theme::current().get(key) {
                        println!("  {key:<22} {value}");
                    }
                }
                println!();
                println!("  set <name> <value>   change one of these");
                println!("  theme <preset>       {}", crate::theme::PRESETS.join(", "));
                println!("  theme save / load    keep them across reboots");
            }

            Some("save") => match crate::theme::save_to_disk() {
                Ok(()) => println!("saved to {}", crate::theme::CONFIG_PATH),
                Err(e) => println!("theme: could not save: {e}"),
            },

            Some("load") => match crate::theme::load_from_disk() {
                Some(skipped) => {
                    for complaint in &skipped {
                        println!("  {complaint}");
                    }
                    self.refresh_desktop();
                    println!("loaded {}", crate::theme::CONFIG_PATH);
                }
                None => println!("theme: nothing saved at {}", crate::theme::CONFIG_PATH),
            },

            Some("reset") => {
                crate::theme::replace(crate::theme::Theme::default());
                self.refresh_desktop();
                println!("back to the default look");
            }

            Some(name) => match crate::theme::preset(name) {
                Some(theme) => {
                    crate::theme::replace(theme);
                    self.refresh_desktop();
                    println!("{name} applied - 'theme save' to keep it");
                }
                None => println!(
                    "theme: no preset called {name:?} (try {})",
                    crate::theme::PRESETS.join(", ")
                ),
            },
        }
    }

    fn set_command(&self, args: &[&str]) {
        let Some(key) = args.first() else {
            println!("usage: set <name> <value>   ('theme' lists the names)");
            return;
        };

        // Everything after the name is the value, so status text can have
        // spaces in it without needing quotes.
        let value = args[1..].join(" ");
        if value.is_empty() {
            match crate::theme::current().get(key) {
                Some(current) => println!("  {key} is {current}"),
                None => println!("set: no such setting: {key}"),
            }
            return;
        }

        match crate::theme::with(|theme| theme.set(key, &value)) {
            Ok(()) => {
                self.refresh_desktop();
                println!("  {key} = {value}");
            }
            Err(e) => println!("set: {e}"),
        }
    }

    /// Dump the card's own view of itself.
    ///
    /// Every value here is read back from the hardware rather than from what
    /// the driver believes it wrote, because the question this answers is
    /// precisely where those two disagree.
    fn nic(&self) {
        let Some(d) = crate::net::e1000::with(|nic| nic.diagnostics()) else {
            println!("  no network card");
            return;
        };

        println!("  link     : {}", if d.link_up() { "up" } else { "DOWN" });
        println!(
            "             {} Mb/s, {}{}",
            d.speed(),
            if d.full_duplex() { "full duplex" } else { "half duplex" },
            if d.transmit_paused() { ", PAUSED by flow control" } else { "" }
        );
        println!("  status   : {:#010x}", d.status);
        println!("  ctrl     : {:#010x}", d.ctrl);
        println!(
            "  tctl     : {:#010x} ({})",
            d.tctl,
            if d.transmit_enabled() { "enabled" } else { "DISABLED" }
        );
        println!(
            "  tx ring  : head {} tail {} len {} ({})",
            d.tdh,
            d.tdt,
            d.tdlen,
            if d.ring_drained() { "drained" } else { "CARD IS BEHIND" }
        );
        println!("  rx ring  : head {} tail {}", d.rdh, d.rdt);

        // Answers whether the blocking transmit is what limits throughput.
        let frames = crate::net::e1000::TX_FRAMES.load(Ordering::Relaxed);
        let spins = crate::net::e1000::TX_SPINS.load(Ordering::Relaxed);
        if frames > 0 {
            println!(
                "  tx wait  : {frames} frames, {spins} spins total, {} each",
                spins / frames
            );
        }
        println!(
            "  last tx  : descriptor {} cmd {:#04x} len {} status {:#04x} ({})",
            d.last_descriptor,
            d.descriptor_command,
            d.descriptor_length,
            d.descriptor_status,
            if d.descriptor_done() { "done" } else { "NOT DONE" }
        );
    }

    fn arp(&self, args: &[&str]) {
        // With an argument, ask; without, show what is already known.
        if let Some(text) = args.first() {
            match parse_ipv4(text) {
                Some(ip) => match crate::net::arp::request(ip) {
                    Ok(()) => println!("asked who has {ip}"),
                    Err(e) => println!("arp: {e}"),
                },
                None => println!("arp: {text}: not an IPv4 address"),
            }
            return;
        }

        let entries = crate::net::arp::entries();
        if entries.is_empty() {
            println!("  (nothing resolved yet)");
            return;
        }

        for (ip, mac) in entries {
            println!(
                "  {:<16} {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                alloc::format!("{ip}"),
                mac[0],
                mac[1],
                mac[2],
                mac[3],
                mac[4],
                mac[5]
            );
        }
    }

    fn ping(&self, args: &[&str]) {
        let Some(text) = args.first() else {
            println!("usage: ping <ip>");
            return;
        };
        let Some(destination) = parse_ipv4(text) else {
            println!("ping: {text}: not an IPv4 address");
            return;
        };

        // Resolve first, so the first echo is not lost waiting for ARP.
        for attempt in 1..=4 {
            match crate::net::ping::ping(destination, 1000) {
                Ok(elapsed) => println!("  reply from {destination}: seq {attempt}, {elapsed} ms"),
                Err(e) => println!("  {destination}: {e}"),
            }
        }
    }

    fn dns_lookup(&self, args: &[&str]) {
        let Some(host) = args.first() else {
            println!("usage: resolve <hostname>");
            return;
        };

        match crate::net::dns::resolve(host, 3000) {
            Ok(address) => println!("  {host} is {address}"),
            Err(e) => println!("  {host}: {e}"),
        }
    }

    fn fetch(&self, args: &[&str]) {
        let Some(url) = args.first() else {
            println!("usage: fetch <url>       e.g. fetch http://example.com/");
            return;
        };

        match crate::net::http::get(url, 5000) {
            Ok(response) => {
                println!("  {} ({} bytes)", response.status, response.total);
                println!("  from {}", response.address);
                println!();

                // Print the body as text, capped so a large page cannot
                // scroll the whole session away.
                let text = String::from_utf8_lossy(&response.body);
                for line in text.lines().take(40) {
                    println!("{line}");
                }
                if text.lines().count() > 40 {
                    println!("... {} more lines", text.lines().count() - 40);
                }
            }
            Err(e) => println!("fetch: {url}: {e}"),
        }
    }

    fn kill(&self, args: &[&str]) {
        let Some(id) = args.first().and_then(|text| text.parse::<u64>().ok()) else {
            println!("usage: kill <task id>   (see 'ps')");
            return;
        };

        match task::kill(id) {
            Ok(()) => println!("killed task {id}"),
            Err(e) => println!("kill: {e}"),
        }
    }

    fn spin(&self) {
        let id = task::spawn("spinner", spinner);
        println!("spawned task {id}; run 'ps' to watch its quanta climb");
    }

    fn mem(&self) {
        let used = vfs::with(|fs| fs.used_bytes()).unwrap_or(0);
        println!("  physical : {} MiB usable", memory::usable_bytes() / (1024 * 1024));
        println!(
            "  frames   : {} in use, {} on the free list",
            memory::allocated_frames(),
            memory::free_frames()
        );
        println!("  heap     : {} KiB mapped", allocator::HEAP_SIZE / 1024);
        println!("  ramdisk  : {used} bytes in files");
    }

    /// Raise a notification by hand, which is how the desktop's own
    /// notifications get tested without waiting for something to go wrong.
    fn notify_command(&self, args: &[&str]) {
        let Some((first, rest)) = args.split_first() else {
            println!("usage: notify [info|ok|warning|error] <message>");
            return;
        };

        use crate::notify::Kind;
        let (kind, words) = match *first {
            "info" => (Kind::Info, rest),
            "ok" | "success" => (Kind::Success, rest),
            "warning" | "warn" => (Kind::Warning, rest),
            "error" => (Kind::Error, rest),
            _ => (Kind::Info, args),
        };

        if words.is_empty() {
            println!("usage: notify [info|ok|warning|error] <message>");
            return;
        }

        crate::notify::post("Kestrel", &words.join(" "), kind);
        if !self.in_desktop {
            println!("  notification queued - it will appear in the desktop");
        }
    }

    /// The wall clock, as opposed to `uptime`, which is how long since boot.
    fn date(&self) {
        let Some(now) = crate::clock::now() else {
            println!("date: no real-time clock on this machine");
            return;
        };

        let offset = crate::clock::offset_hours();
        println!(
            "  {} {} {} {} {:02}:{:02}:{:02}",
            crate::clock::weekday(&now),
            now.day,
            crate::clock::MONTHS[(now.month.clamp(1, 12) - 1) as usize],
            now.year,
            now.hour,
            now.minute,
            now.second
        );
        println!(
            "  from the hardware clock{}",
            match offset {
                0 => alloc::string::String::from(" ('set clock.offset 2' to shift it)"),
                hours => alloc::format!(", shifted by {hours:+} hours"),
            }
        );
    }

    fn uptime(&self) {
        let ticks = apic::ticks();
        let seconds = ticks / apic::TIMER_FREQUENCY as u64;
        println!(
            "  up {}m {}s ({ticks} ticks at {} Hz)",
            seconds / 60,
            seconds % 60,
            apic::TIMER_FREQUENCY
        );
    }
}

/// Parse dotted-quad notation.
/// Build a window of the given kind, sized to the screen.
///
/// One place, used both when the desktop opens and when the launcher reopens
/// something that was closed — so a reopened window is identical to the one
/// that was there at the start rather than a second, subtly different version.
fn build_window(kind: crate::desktop::Kind, width: usize, height: usize) -> crate::desktop::Window {
    use crate::desktop::{Kind, Window};

    // Positions and minimum sizes are in scaled pixels for the same reason the
    // rest of the desktop is: a window measured in raw pixels holds half as
    // much text at scale 2, and the Settings controls - which are laid out in
    // character cells - would run straight out of the bottom of it.
    let scale = crate::theme::current().scale;
    let s = |value: usize| value * scale;
    let at = |value: usize| (value * scale) as isize;

    match kind {
        // A program's window is created by the program, through the surface
        // system call, so this is only ever reached if something asks for one
        // by mistake. An empty window is a clearer answer than a panic.
        Kind::Surface => Window::new(kind, at(200), at(200), s(320), s(240)),

        // Clear of the desktop shortcuts on the left, and short enough that
        // its bottom edge stays above the monitor window in the corner.
        Kind::Terminal => Window::new(kind, at(180), at(66), width * 6 / 10, height * 2 / 5),

        // Sized to its contents rather than guessed at: these two windows are
        // lists of controls, they cannot scroll, and a height in round numbers
        // clipped the last few rows away as soon as the text scale changed.
        Kind::Settings | Kind::Software => {
            let theme = crate::theme::current();
            let (cell_w, cell_h) = (theme.cell_width(), theme.cell_height());
            let window_width = (width * 5 / 10).max(s(520));

            let items = if kind == Kind::Software {
                crate::settings::software_layout(0, 0, window_width, cell_w, cell_h)
            } else {
                crate::settings::layout(&theme, 0, 0, window_width, cell_w, cell_h)
            };
            let content = crate::settings::content_height(&items, 0) + s(10);

            // Never taller than the space above the panel: a window that runs
            // off the bottom of the screen hides the very controls that would
            // have made it smaller.
            let room = height.saturating_sub(theme.panel_height + s(24));
            let window_height = (content + theme.title_height).min(room);
            let x = if kind == Kind::Software { at(150) } else { at(120) };
            let y = at(60).min((height - theme.panel_height - window_height - s(12)) as isize);

            Window::new(kind, x, y, window_width, window_height)
        }

        Kind::Monitor => {
            // Placed so it stays on screen and clear of the terminal: the
            // right edge is measured back from the display, and the bottom
            // back from the panel, rather than either being extrapolated from
            // the other window. Both used to be guessed, and at 1920x1080 the
            // guess put this window through the middle of the terminal.
            let panel = crate::theme::current().panel_height;
            let monitor_width = width / 4;
            let monitor_height = height / 3;
            let mut window = Window::new(
                kind,
                (width - monitor_width - s(24)) as isize,
                height.saturating_sub(panel + monitor_height + s(16)) as isize,
                monitor_width,
                monitor_height,
            );

            window.push(&alloc::format!("cores    {}", cpu::online()));
            window.push(&alloc::format!(
                "memory   {} MiB",
                memory::usable_bytes() / (1024 * 1024)
            ));
            if let Some(ip) = crate::net::config(|c| c.ip) {
                window.push(&alloc::format!("address  {ip}"));
            }
            window
        }
    }
}

fn parse_ipv4(text: &str) -> Option<crate::net::Ipv4> {
    let mut octets = [0u8; 4];
    let mut parts = text.split('.');

    for octet in &mut octets {
        *octet = parts.next()?.parse().ok()?;
    }

    // A fifth component means it was never an address.
    if parts.next().is_some() {
        return None;
    }

    Some(crate::net::Ipv4::new(
        octets[0], octets[1], octets[2], octets[3],
    ))
}

/// Collapse `.`/`..` in an absolute path for display purposes.
fn normalise(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            name => parts.push(name),
        }
    }

    if parts.is_empty() {
        String::from("/")
    } else {
        alloc::format!("/{}", parts.join("/"))
    }
}

static SPIN_COUNT: AtomicU64 = AtomicU64::new(0);

/// A task that never yields, so only preemption can take the CPU back.
fn spinner() {
    loop {
        SPIN_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn run() -> ! {
    Shell::new().run()
}









