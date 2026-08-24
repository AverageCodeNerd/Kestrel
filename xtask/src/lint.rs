//! Source checks that need no guest.
//!
//! These exist because of a class of bug the smoke suite structurally cannot
//! see. An em dash in the `version` banner shipped in 0.9.1 and rendered as a
//! row of question marks, because the framebuffer console draws from an 8x8
//! ASCII font and emits one `?` per byte it has no glyph for. The obvious
//! regression test — run `version`, reject `??` — does not work: the suite
//! drives the *serial* console, which passes UTF-8 through untouched. The
//! defect is real and invisible from there.
//!
//! Checking the source is the level that actually catches it, so that is what
//! this does. `cargo xtask test` runs it before booting anything, and
//! `release.ps1` runs it as a release gate.

use std::path::{Path, PathBuf};

/// Directories whose Rust sources can end up printing to the console.
const ROOTS: &[&str] = &["kernel/src", "user"];

pub struct Offence {
    pub path: PathBuf,
    pub line: usize,
    pub text: String,
}

/// Find non-ASCII outside comments in code that may reach the console.
///
/// Comments are exempt because they never reach a screen, and the codebase
/// uses em dashes in prose throughout. The test is deliberately a line-level
/// heuristic: `//` and `///` cover every comment in this tree, and the cost of
/// a false positive is one hyphen.
pub fn non_ascii(root: &Path) -> Vec<Offence> {
    let mut found = Vec::new();

    for dir in ROOTS {
        walk(&root.join(dir), &mut |path| {
            let Ok(text) = std::fs::read_to_string(path) else {
                return;
            };

            for (index, line) in text.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if line.chars().any(|c| !c.is_ascii()) {
                    found.push(Offence {
                        path: path.to_path_buf(),
                        line: index + 1,
                        text: line.trim().to_string(),
                    });
                }
            }
        });
    }

    found
}

fn walk(dir: &Path, visit: &mut impl FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();

        // Build output is not source, and it is enormous.
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            walk(&path, visit);
        } else if path.extension().is_some_and(|e| e == "rs") {
            visit(&path);
        }
    }
}

/// Run every source check. Returns the number that failed.
pub fn run(root: &Path) -> usize {
    let offences = non_ascii(root);

    if offences.is_empty() {
        println!("ok    sources printable by the console are ASCII");
        return 0;
    }

    eprintln!("FAIL  non-ASCII in code the console can print");
    eprintln!("  The framebuffer font is 8x8 ASCII; anything else renders as");
    eprintln!("  one question mark per byte. Use a plain hyphen in messages.");
    for offence in &offences {
        eprintln!("  {}:{}: {}", offence.path.display(), offence.line, offence.text);
    }

    1
}
