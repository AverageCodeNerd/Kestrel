//! A small language you can write Kestrel programs in.
//!
//! Write a script with `edit`, run it with this. That pairing is the point:
//! until now you could run programs on Kestrel but only make them somewhere
//! else, on another machine, with a cross compiler.
//!
//! It is BASIC-shaped because BASIC is what fits. There is no allocator for
//! user programs here, so the interpreter is fixed arrays throughout — a
//! bounded number of lines, of variables, and of nesting. A language with
//! closures or growable data structures would need a heap first.
//!
//! Jumps are by line number, counting the script's own lines from one, which
//! is both the easiest thing to implement and the easiest thing to reason
//! about when the only debugger is `print`.

#![no_std]
#![no_main]

const MAX_LINES: usize = 200;
const MAX_LINE: usize = 128;
const MAX_VARS: usize = 32;
const NAME_LEN: usize = 12;
/// Enough for a long-running loop, few enough that a runaway script stops
/// rather than wedging the machine.
const MAX_STEPS: u64 = 2_000_000;

struct Program {
    lines: [[u8; MAX_LINE]; MAX_LINES],
    lengths: [usize; MAX_LINES],
    count: usize,
}

struct Variables {
    names: [[u8; NAME_LEN]; MAX_VARS],
    name_lengths: [usize; MAX_VARS],
    values: [i64; MAX_VARS],
    count: usize,
}

impl Variables {
    fn slot(&mut self, name: &str) -> Option<usize> {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.len() > NAME_LEN {
            return None;
        }

        for index in 0..self.count {
            if &self.names[index][..self.name_lengths[index]] == bytes {
                return Some(index);
            }
        }

        if self.count >= MAX_VARS {
            return None;
        }
        let index = self.count;
        self.names[index][..bytes.len()].copy_from_slice(bytes);
        self.name_lengths[index] = bytes.len();
        self.values[index] = 0;
        self.count += 1;
        Some(index)
    }

    fn get(&mut self, name: &str) -> Option<i64> {
        self.slot(name).map(|index| self.values[index])
    }

    fn set(&mut self, name: &str, value: i64) -> bool {
        match self.slot(name) {
            Some(index) => {
                self.values[index] = value;
                true
            }
            None => false,
        }
    }
}

static mut PROGRAM: Program = Program {
    lines: [[0; MAX_LINE]; MAX_LINES],
    lengths: [0; MAX_LINES],
    count: 0,
};

static mut VARIABLES: Variables = Variables {
    names: [[0; NAME_LEN]; MAX_VARS],
    name_lengths: [0; MAX_VARS],
    values: [0; MAX_VARS],
    count: 0,
};

fn program() -> &'static mut Program {
    unsafe { &mut *core::ptr::addr_of_mut!(PROGRAM) }
}

fn variables() -> &'static mut Variables {
    unsafe { &mut *core::ptr::addr_of_mut!(VARIABLES) }
}

fn line(index: usize) -> &'static str {
    let p = program();
    // Scripts are ASCII; anything else was rejected when it was loaded.
    unsafe { core::str::from_utf8_unchecked(&p.lines[index][..p.lengths[index]]) }
}

// ------------------------------------------------------------ expressions ---

/// A term: a number, a variable, or a bracketed expression.
///
/// The parser is the usual recursive pair — `expression` handles `+` and `-`,
/// `product` handles `*`, `/` and `%` — which is what gives multiplication its
/// precedence without a table.
fn term(text: &str) -> Option<(i64, &str)> {
    let text = text.trim_start();

    if let Some(rest) = text.strip_prefix('(') {
        let (value, rest) = expression(rest)?;
        let rest = rest.trim_start();
        return rest.strip_prefix(')').map(|rest| (value, rest));
    }

    if let Some(rest) = text.strip_prefix('-') {
        let (value, rest) = term(rest)?;
        return Some((-value, rest));
    }

    let end = text
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(text.len());
    if end == 0 {
        return None;
    }
    let (word, rest) = text.split_at(end);

    if word.bytes().all(|b| b.is_ascii_digit()) {
        return word.parse().ok().map(|value| (value, rest));
    }
    variables().get(word).map(|value| (value, rest))
}

fn product(text: &str) -> Option<(i64, &str)> {
    let (mut value, mut rest) = term(text)?;

    loop {
        let trimmed = rest.trim_start();
        let (operator, tail) = match trimmed.chars().next() {
            Some(c @ ('*' | '/' | '%')) => (c, &trimmed[1..]),
            _ => return Some((value, rest)),
        };

        let (right, tail) = term(tail)?;
        value = match operator {
            '*' => value.wrapping_mul(right),
            // Dividing by zero would fault the whole program; nought is a
            // poor answer but a better one than dying.
            '/' if right != 0 => value / right,
            '%' if right != 0 => value % right,
            _ => 0,
        };
        rest = tail;
    }
}

fn expression(text: &str) -> Option<(i64, &str)> {
    let (mut value, mut rest) = product(text)?;

    loop {
        let trimmed = rest.trim_start();
        let (operator, tail) = match trimmed.chars().next() {
            Some(c @ ('+' | '-')) => (c, &trimmed[1..]),
            _ => return Some((value, rest)),
        };

        let (right, tail) = product(tail)?;
        value = if operator == '+' {
            value.wrapping_add(right)
        } else {
            value.wrapping_sub(right)
        };
        rest = tail;
    }
}

fn evaluate(text: &str) -> Option<i64> {
    let (value, rest) = expression(text)?;
    rest.trim().is_empty().then_some(value)
}

/// A comparison, for `if`. Returns the answer and what follows `then`.
fn condition(text: &str) -> Option<(bool, &str)> {
    let (left, rest) = expression(text)?;
    let rest = rest.trim_start();

    // Two-character operators first, or `<` would match the start of `<=`.
    let (operator, rest) = if let Some(tail) = rest.strip_prefix("<=") {
        ("<=", tail)
    } else if let Some(tail) = rest.strip_prefix(">=") {
        (">=", tail)
    } else if let Some(tail) = rest.strip_prefix("<>") {
        ("<>", tail)
    } else if let Some(tail) = rest.strip_prefix('<') {
        ("<", tail)
    } else if let Some(tail) = rest.strip_prefix('>') {
        (">", tail)
    } else if let Some(tail) = rest.strip_prefix('=') {
        ("=", tail)
    } else {
        return None;
    };

    let (right, rest) = expression(rest)?;
    let answer = match operator {
        "=" => left == right,
        "<>" => left != right,
        "<" => left < right,
        ">" => left > right,
        "<=" => left <= right,
        _ => left >= right,
    };
    Some((answer, rest))
}

// -------------------------------------------------------------- execution ---

enum Step {
    Next,
    Goto(usize),
    Stop,
}

fn print_argument(rest: &str) {
    let rest = rest.trim();

    if let Some(text) = rest.strip_prefix('"') {
        match text.split_once('"') {
            Some((literal, _)) => kestrel::write_line(literal),
            None => kestrel::write_line(text),
        }
        return;
    }

    if rest.is_empty() {
        kestrel::write_line("");
        return;
    }

    match evaluate(rest) {
        Some(value) => {
            if value < 0 {
                kestrel::write("-");
                kestrel::write_number(value.unsigned_abs());
            } else {
                kestrel::write_number(value as u64);
            }
            kestrel::write_line("");
        }
        None => kestrel::write_line("?"),
    }
}

fn run_statement(statement: &str, number: usize) -> Step {
    let statement = statement.trim();
    if statement.is_empty() || statement.starts_with('#') {
        return Step::Next;
    }

    let (keyword, rest) = match statement.split_once(' ') {
        Some((keyword, rest)) => (keyword, rest),
        None => (statement, ""),
    };

    match keyword {
        "print" => print_argument(rest),

        "let" => match rest.split_once('=') {
            Some((name, value)) => match evaluate(value) {
                Some(value) if variables().set(name.trim(), value) => {}
                Some(_) => complain(number, "too many variables"),
                None => complain(number, "that is not an expression"),
            },
            None => complain(number, "let needs a name and a value"),
        },

        "input" => {
            let name = rest.trim();
            kestrel::write("? ");
            let mut buffer = [0u8; 32];
            let length = kestrel::read_line(&mut buffer);
            let typed = unsafe { core::str::from_utf8_unchecked(&buffer[..length]) };

            let value = typed.trim().parse().unwrap_or(0);
            if !variables().set(name, value) {
                complain(number, "too many variables");
            }
        }

        "if" => match rest.split_once(" then ") {
            Some((test, body)) => match condition(test) {
                Some((true, _)) => return run_statement(body, number),
                Some((false, _)) => {}
                None => complain(number, "that is not a comparison"),
            },
            None => complain(number, "if needs a then"),
        },

        "goto" => match evaluate(rest) {
            // Lines are counted from one, the way they are shown.
            Some(target) if target >= 1 && (target as usize) <= program().count => {
                return Step::Goto(target as usize - 1)
            }
            _ => complain(number, "no such line"),
        },

        "end" => return Step::Stop,

        _ => complain(number, "unknown statement"),
    }

    Step::Next
}

fn complain(number: usize, what: &str) {
    kestrel::write("line ");
    kestrel::write_number(number as u64);
    kestrel::write(": ");
    kestrel::write_line(what);
}

fn run_program() {
    let mut at = 0;
    let mut steps = 0u64;

    while at < program().count {
        steps += 1;
        if steps > MAX_STEPS {
            kestrel::write_line("stopped: this has run too long");
            return;
        }

        match run_statement(line(at), at + 1) {
            Step::Next => at += 1,
            Step::Goto(target) => at = target,
            Step::Stop => return,
        }
    }
}

fn load(path: &str) -> bool {
    let mut raw = [0u8; MAX_LINES * MAX_LINE];
    let count = kestrel::load(path, &mut raw);
    if count == kestrel::FAILED {
        kestrel::write("cannot read ");
        kestrel::write_line(path);
        return false;
    }

    let p = program();
    p.count = 0;

    for source in raw[..count as usize].split(|byte| *byte == b'\n') {
        if p.count >= MAX_LINES {
            kestrel::write_line("the script is too long; the rest was ignored");
            break;
        }

        // Carriage returns from a file written elsewhere would otherwise end
        // up inside the last token on every line.
        let source = match source.split_last() {
            Some((b'\r', head)) => head,
            _ => source,
        };

        let length = source.len().min(MAX_LINE);
        p.lines[p.count][..length].copy_from_slice(&source[..length]);
        p.lengths[p.count] = length;
        p.count += 1;
    }
    true
}

fn help() {
    kestrel::write_line("kestrel basic");
    kestrel::write_line("  print \"text\"  |  print <expression>");
    kestrel::write_line("  let <name> = <expression>");
    kestrel::write_line("  input <name>");
    kestrel::write_line("  if <a> <op> <b> then <statement>     = <> < > <= >=");
    kestrel::write_line("  goto <line>        lines count from 1");
    kestrel::write_line("  end                # starts a comment");
    kestrel::write_line("");
    kestrel::write_line("write a script with 'edit', then run it here");
}

fn run() {
    help();
    kestrel::write("script: ");

    let mut name = [0u8; 128];
    let length = kestrel::read_line(&mut name);
    let path = unsafe { core::str::from_utf8_unchecked(&name[..length]) };

    if path.trim().is_empty() {
        kestrel::write_line("nothing to run");
        return;
    }

    if !load(path.trim()) {
        return;
    }

    kestrel::write("running ");
    kestrel::write_number(program().count as u64);
    kestrel::write_line(" lines");
    kestrel::write_line("");

    run_program();
}

kestrel::main!(run);
