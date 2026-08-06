//! The package manager behind the Software app.
//!
//! Nothing ships installed. The boot media carries a read-only repository at
//! `/repo`, and installing copies a package out of it into wherever installed
//! programs live. That distinction is the point: a fresh system has a
//! catalogue and no applications, and what is on it afterwards is what someone
//! chose to put there.
//!
//! There is no dependency resolution, no signing and no versioning yet. A
//! package is one static ELF and a line of description, which is honest about
//! what this can carry: programs here have no shared libraries to depend on.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Where the boot media's packages live, in RAM, put there by the bootloader.
pub const REPOSITORY: &str = "/repo";

/// Where installed programs go when there is a writable disk. Surviving a
/// reboot is most of what installing means.
pub const INSTALLED_DISK: &str = "/disk/apps";

/// Where they go when there is not — booting the ISO, which has no writable
/// disk at all. Installing still works; it just does not outlive the session.
pub const INSTALLED_RAM: &str = "/apps";

pub struct Package {
    pub name: String,
    pub summary: String,
    pub size: usize,
    pub installed: bool,
}

/// Whether installs will survive a reboot.
///
/// Decided by whether the disk is actually there rather than by how the system
/// was booted, so it stays right if the disk arrives some other way later.
pub fn persistent() -> bool {
    crate::vfs::list(INSTALLED_DISK).is_ok() || crate::vfs::list("/disk").is_ok()
}

/// Where installed programs live on this machine.
pub fn install_root() -> &'static str {
    if persistent() {
        INSTALLED_DISK
    } else {
        INSTALLED_RAM
    }
}

fn ensure_root() -> Result<&'static str, String> {
    let root = install_root();
    if crate::vfs::list(root).is_ok() {
        return Ok(root);
    }

    // `mkdir` on a path that already exists is not an error worth reporting,
    // and the listing above may simply have raced a previous install.
    match crate::vfs::mkdir(root) {
        Ok(()) => Ok(root),
        Err(_) if crate::vfs::list(root).is_ok() => Ok(root),
        Err(e) => Err(format!("could not create {root}: {}", e.as_str())),
    }
}

pub fn installed_path(name: &str) -> String {
    format!("{}/{name}", install_root())
}

pub fn is_installed(name: &str) -> bool {
    crate::vfs::read(&installed_path(name)).is_ok()
}

/// Everything the repository offers.
///
/// Reads the catalogue for the descriptions and the directory for the sizes,
/// so a package listed but not shipped is skipped rather than offered and then
/// failing to install.
pub fn catalogue() -> Vec<Package> {
    let mut packages = Vec::new();

    let Ok(raw) = crate::vfs::read(&format!("{REPOSITORY}/catalogue")) else {
        return packages;
    };
    let text = String::from_utf8_lossy(&raw).into_owned();

    let entries = crate::vfs::list(REPOSITORY).unwrap_or_default();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let (name, summary) = match line.split_once('|') {
            Some((name, summary)) => (name.trim(), summary.trim()),
            None => (line, ""),
        };

        let Some(entry) = entries.iter().find(|entry| entry.name == name) else {
            continue;
        };

        packages.push(Package {
            name: name.to_string(),
            summary: summary.to_string(),
            size: entry.size,
            installed: is_installed(name),
        });
    }

    packages
}

/// Copy a package out of the repository. Returns its size.
pub fn install(name: &str) -> Result<usize, String> {
    if name.is_empty() || name.contains('/') {
        return Err(format!("{name:?} is not a package name"));
    }

    let source = format!("{REPOSITORY}/{name}");
    let Ok(image) = crate::vfs::read(&source) else {
        return Err(format!("no package called {name}"));
    };

    let root = ensure_root()?;
    let target = format!("{root}/{name}");

    match crate::vfs::write(&target, &image) {
        Ok(()) => Ok(image.len()),
        Err(e) => Err(format!("could not install {name}: {}", e.as_str())),
    }
}

pub fn remove(name: &str) -> Result<(), String> {
    let path = installed_path(name);
    if crate::vfs::read(&path).is_err() {
        return Err(format!("{name} is not installed"));
    }

    crate::vfs::remove(&path).map_err(|e| format!("could not remove {name}: {}", e.as_str()))
}

/// Find an installed program by bare name, so `exec snake` works.
///
/// Only the install directory is searched. The repository deliberately is not:
/// a package that has not been installed should not run, or installing would
/// mean nothing.
pub fn resolve(name: &str) -> Option<String> {
    let path = installed_path(name);
    crate::vfs::read(&path).ok().map(|_| path)
}
