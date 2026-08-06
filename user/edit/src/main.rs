//! A line editor for Kestrel.
//!
//! Deliberately line-based rather than full-screen: there is no way for a
//! program to move the cursor or clear the screen yet, only to write text, so
//! a full-screen editor would have nothing to redraw with. Working within that
//! is what makes this the first program that is actually useful rather than a
//! demonstration.
//!
//! It exercises every system call the kernel gained for programs: reading the
//! keyboard, loading a file, and storing one.

#![no_std]
#![no_main]

/// Lines held at once. Each is a fixed slot, so there is no allocator.
const MAX_LINES: usize = 64;
const MAX_LINE: usize = 128;
/// Big enough for a file of MAX_LINES x MAX_LINE plus its newlines.
const MAX_FILE: usize = MAX_LINES * (MAX_LINE + 1);

struct Buffer {
    lines: [[u8; MAX_LINE]; MAX_LINES],
    lengths: [usize; MAX_LINES],
    count: usize,
}

impl Buffer {
    const fn new() -> Self {
        Self {
            lines: [[0; MAX_LINE]; MAX_LINES],
            lengths: [0; MAX_LINES],
            count: 0,
        }
    }

    fn line(&self, index: usize) -> &str {
        // Only ASCII is ever stored, so this cannot split a character.
        unsafe { core::str::from_utf8_unchecked(&self.lines[index][..self.lengths[index]]) }
    }

    fn push(&mut self, text: &[u8]) -> bool {
        if self.count >= MAX_LINES {
            return false;
        }
        let length = text.len().min(MAX_LINE);
        self.lines[self.count][..length].copy_from_slice(&text[..length]);
        self.lengths[self.count] = length;
        self.count += 1;
        true
    }

    fn remove(&mut self, index: usize) {
        if index >= self.count {
            return;
        }
        for slot in index..self.count - 1 {
            self.lines[slot] = self.lines[slot + 1];
            self.lengths[slot] = self.lengths[slot + 1];
        }
        self.count -= 1;
    }
}

static mut BUFFER: Buffer = Buffer::new();
static mut PATH: [u8; 128] = [0; 128];
static mut PATH_LENGTH: usize = 0;

fn path() -> &'static str {
    unsafe {
        let bytes = &*core::ptr::addr_of!(PATH);
        core::str::from_utf8_unchecked(&bytes[..PATH_LENGTH])
    }
}

fn set_path(text: &[u8]) {
    unsafe {
        let length = text.len().min(128);
        let slot = &mut *core::ptr::addr_of_mut!(PATH);
        slot[..length].copy_from_slice(&text[..length]);
        PATH_LENGTH = length;
    }
}

fn buffer() -> &'static mut Buffer {
    unsafe { &mut *core::ptr::addr_of_mut!(BUFFER) }
}

fn load_file() {
    if unsafe { PATH_LENGTH } == 0 {
        return;
    }

    let mut raw = [0u8; MAX_FILE];
    let count = kestrel::load(path(), &mut raw);
    if count == kestrel::FAILED {
        kestrel::write_line("(new file)");
        return;
    }

    for line in raw[..count as usize].split(|byte| *byte == b'\n') {
        // A trailing newline produces a final empty piece that is not a line.
        if line.is_empty() && buffer().count > 0 {
            continue;
        }
        if !buffer().push(line) {
            kestrel::write_line("(truncated: too many lines)");
            break;
        }
    }

    kestrel::write("loaded ");
    kestrel::write_number(buffer().count as u64);
    kestrel::write_line(" lines");
}

fn save_file() {
    if unsafe { PATH_LENGTH } == 0 {
        kestrel::write_line("no filename; use: edit <path>");
        return;
    }

    let mut raw = [0u8; MAX_FILE];
    let mut at = 0;

    for index in 0..buffer().count {
        let line = buffer().line(index).as_bytes();
        raw[at..at + line.len()].copy_from_slice(line);
        at += line.len();
        raw[at] = b'\n';
        at += 1;
    }

    match kestrel::store(path(), &raw[..at]) {
        kestrel::FAILED => kestrel::write_line("could not write the file"),
        written => {
            kestrel::write("wrote ");
            kestrel::write_number(written);
            kestrel::write(" bytes to ");
            kestrel::write_line(path());
        }
    }
}

fn list() {
    if buffer().count == 0 {
        kestrel::write_line("(empty)");
        return;
    }
    for index in 0..buffer().count {
        kestrel::write("  ");
        kestrel::write_number(index as u64 + 1);
        kestrel::write("  ");
        kestrel::write_line(buffer().line(index));
    }
}

fn help() {
    kestrel::write_line("  a <text>   add a line");
    kestrel::write_line("  d <n>      delete line n");
    kestrel::write_line("  l          list the file");
    kestrel::write_line("  w          write it out");
    kestrel::write_line("  q          quit");
}

/// Parse a small positive number. Anything else is not a line number.
fn number(text: &str) -> Option<usize> {
    if text.is_empty() {
        return None;
    }
    let mut value = 0usize;
    for byte in text.bytes() {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((byte - b'0') as usize)?;
    }
    Some(value)
}

fn run() {
    kestrel::write_line("kestrel edit - 'h' for help, 'q' to quit");

    // The shell does not pass arguments yet, so the file is asked for here.
    kestrel::write("file: ");
    let mut name = [0u8; 128];
    let length = kestrel::read_line(&mut name);
    set_path(&name[..length]);
    load_file();

    let mut input = [0u8; MAX_LINE + 8];
    loop {
        kestrel::write("> ");
        let length = kestrel::read_line(&mut input);
        if length == 0 {
            continue;
        }

        let line = unsafe { core::str::from_utf8_unchecked(&input[..length]) };
        let (command, rest) = match line.split_once(' ') {
            Some((command, rest)) => (command, rest),
            None => (line, ""),
        };

        match command {
            "a" => {
                if !buffer().push(rest.as_bytes()) {
                    kestrel::write_line("the buffer is full");
                }
            }
            "d" => match number(rest) {
                Some(n) if n >= 1 && n <= buffer().count => buffer().remove(n - 1),
                _ => kestrel::write_line("which line?"),
            },
            "l" => list(),
            "w" => save_file(),
            "h" => help(),
            "q" => return,
            _ => kestrel::write_line("unknown command; 'h' for help"),
        }
    }
}

kestrel::main!(run);
