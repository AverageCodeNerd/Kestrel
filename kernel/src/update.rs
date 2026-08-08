//! Replacing the running system with a newer one, from inside it.
//!
//! Everything Kestrel boots from lives on a FAT32 partition it can write:
//! the kernel at `/disk/boot/kestrel`, the repository at `/disk/repo`. An
//! update is therefore just careful file replacement followed by a reboot.
//!
//! The care is the interesting part. Overwriting the kernel is the one
//! operation on this system that can leave a machine unable to boot, so:
//!
//! * the outgoing kernel is kept as `kestrel.old`, and `limine.conf` carries a
//!   second entry pointing at it, so a bad update is a menu choice away from
//!   being undone rather than a brick;
//! * nothing is written until every file has been downloaded and checked, so a
//!   connection that dies halfway leaves the installed system untouched;
//! * the new kernel must parse as an ELF executable of the size the manifest
//!   claimed before it is allowed to replace anything.
//!
//! That last check is **integrity, not authenticity**. There is no TLS and no
//! signing here: anyone who can answer the HTTP request can hand this machine
//! a kernel, and it will run it. Point it only at a source you trust.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::net::http;

/// Where the running system boots from.
const KERNEL_PATH: &str = "/disk/boot/kestrel";
const PREVIOUS_PATH: &str = "/disk/boot/kestrel.old";
const REPOSITORY: &str = "/disk/repo";
/// Where the update source is remembered between boots.
const SOURCE_PATH: &str = "/disk/update.conf";

/// Generous on purpose. This stack has no reassembly queue, so a lost segment
/// costs a full retransmission timeout and throughput is poor — a kernel is
/// close to a megabyte and takes minutes rather than seconds. Twenty seconds
/// was enough to make every real update fail before it started.
const TIMEOUT_MS: u64 = 240_000;

pub struct Manifest {
    pub version: String,
    pub kernel_size: usize,
    /// Package name and its size, including the catalogue.
    pub packages: Vec<(String, usize)>,
}

impl Manifest {
    /// Whether this describes something other than what is running.
    ///
    /// Compared as text rather than parsed and ordered: the point is "is this
    /// the same build", and an update that moves backwards is still a change
    /// the user asked for.
    pub fn differs(&self) -> bool {
        self.version != running_version()
    }
}

/// The version this kernel was built as, without the decoration `VERSION` adds.
pub fn running_version() -> String {
    crate::VERSION
        .trim_start_matches('v')
        .split_whitespace()
        .next()
        .unwrap_or("unknown")
        .to_string()
}

/// Where updates come from. Remembered on disk so it survives a reboot.
pub fn source() -> Option<String> {
    let bytes = crate::vfs::read(SOURCE_PATH).ok()?;
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    (!text.is_empty()).then_some(text)
}

pub fn set_source(url: &str) -> Result<(), String> {
    crate::vfs::write(SOURCE_PATH, url.trim().as_bytes())
        .map_err(|e| format!("could not remember the source: {}", e.as_str()))
}

/// Read the manifest at `<base>/manifest`.
///
/// The format is one directive per line, because the parser has to live in a
/// kernel and anything richer would be a liability:
///
/// ```text
/// version 0.9.2
/// kernel 900832
/// package edit 32928
/// ```
pub fn check(base: &str) -> Result<Manifest, String> {
    let url = format!("{}/manifest", base.trim_end_matches('/'));
    let response = http::get(&url, TIMEOUT_MS).map_err(|e| format!("{url}: {e}"))?;

    if !response.status.contains("200") {
        return Err(format!("{url}: {}", response.status));
    }

    let text = String::from_utf8_lossy(&response.body).into_owned();
    let mut version = None;
    let mut kernel_size = None;
    let mut packages = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let mut words = line.split_whitespace();
        match (words.next(), words.next(), words.next()) {
            (Some("version"), Some(value), _) => version = Some(value.to_string()),
            (Some("kernel"), Some(size), _) => kernel_size = size.parse().ok(),
            (Some("package"), Some(name), Some(size)) => {
                if let Ok(size) = size.parse() {
                    packages.push((name.to_string(), size));
                }
            }
            _ => {}
        }
    }

    Ok(Manifest {
        version: version.ok_or("the manifest has no version")?,
        kernel_size: kernel_size.ok_or("the manifest has no kernel")?,
        packages,
    })
}

/// Does this look like a kernel?
///
/// Not a signature check and not pretending to be one. It catches the failure
/// that actually happens — a truncated download, or a server answering with an
/// error page — before that gets written over the only bootable kernel.
fn looks_like_a_kernel(image: &[u8], expected: usize) -> Result<(), String> {
    if image.len() != expected {
        return Err(format!(
            "expected {expected} bytes but received {}",
            image.len()
        ));
    }
    if image.len() < 64 || &image[..4] != b"\x7fELF" {
        return Err("what arrived is not an ELF executable".to_string());
    }
    // 2 is ET_EXEC. Anything else would not be bootable.
    if u16::from_le_bytes([image[16], image[17]]) != 2 {
        return Err("what arrived is not an executable ELF".to_string());
    }
    Ok(())
}

pub struct Report {
    pub version: String,
    pub kernel_bytes: usize,
    pub packages: usize,
}

/// Download everything, check it, then write it.
///
/// Downloading completely before writing anything is what makes a failed
/// update harmless: a connection that dies partway leaves the installed system
/// exactly as it was.
pub fn apply(base: &str, manifest: &Manifest) -> Result<Report, String> {
    let base = base.trim_end_matches('/');

    let kernel = fetch(&format!("{base}/kestrel"))?;
    looks_like_a_kernel(&kernel, manifest.kernel_size)?;

    let mut downloaded = Vec::new();
    for (name, size) in &manifest.packages {
        let body = fetch(&format!("{base}/repo/{name}"))?;
        if body.len() != *size {
            return Err(format!(
                "{name}: expected {size} bytes but received {}",
                body.len()
            ));
        }
        downloaded.push((name.clone(), body));
    }

    // From here on we are writing. The old kernel goes first so that if the
    // machine loses power midway, the fallback entry still points at something.
    if let Ok(current) = crate::vfs::read(KERNEL_PATH) {
        crate::vfs::write(PREVIOUS_PATH, &current)
            .map_err(|e| format!("could not keep the old kernel: {}", e.as_str()))?;
    }

    crate::vfs::write(KERNEL_PATH, &kernel)
        .map_err(|e| format!("could not write the kernel: {}", e.as_str()))?;

    let mut written = 0;
    for (name, body) in &downloaded {
        let path = format!("{REPOSITORY}/{name}");
        match crate::vfs::write(&path, body) {
            Ok(()) => written += 1,
            // A package that fails to write is worth reporting but not worth
            // abandoning an already-replaced kernel over.
            Err(e) => crate::println!("update: {name}: {}", e.as_str()),
        }
    }

    Ok(Report {
        version: manifest.version.clone(),
        kernel_bytes: kernel.len(),
        packages: written,
    })
}

fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let response = http::get(url, TIMEOUT_MS).map_err(|e| format!("{url}: {e}"))?;
    if !response.status.contains("200") {
        return Err(format!("{url}: {}", response.status));
    }
    Ok(response.body)
}

/// Whether there is a kernel to fall back to.
pub fn rollback_available() -> bool {
    crate::vfs::exists(PREVIOUS_PATH)
}
