//! An in-memory filesystem.
//!
//! There is no disk driver yet, so this is a ramdisk: a tree of directories
//! and byte-vectors living on the kernel heap. The path handling and the
//! interface are the parts worth getting right — a real block-backed
//! filesystem can be slotted in underneath later.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use spin::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NotFound,
    NotADirectory,
    IsADirectory,
    AlreadyExists,
    InvalidPath,
    ReadOnly,
    Io(&'static str),
}

impl Error {
    pub fn as_str(self) -> &'static str {
        match self {
            Error::NotFound => "no such file or directory",
            Error::NotADirectory => "not a directory",
            Error::IsADirectory => "is a directory",
            Error::AlreadyExists => "already exists",
            Error::InvalidPath => "invalid path",
            Error::ReadOnly => "read-only filesystem",
            Error::Io(message) => message,
        }
    }
}

pub enum Node {
    File(Vec<u8>),
    Directory(BTreeMap<String, Node>),
}

impl Node {
    fn children(&self) -> Result<&BTreeMap<String, Node>, Error> {
        match self {
            Node::Directory(map) => Ok(map),
            Node::File(_) => Err(Error::NotADirectory),
        }
    }

    fn children_mut(&mut self) -> Result<&mut BTreeMap<String, Node>, Error> {
        match self {
            Node::Directory(map) => Ok(map),
            Node::File(_) => Err(Error::NotADirectory),
        }
    }
}

/// One entry as reported by `list`.
pub struct Entry {
    pub name: String,
    pub is_directory: bool,
    pub size: usize,
}

pub struct Vfs {
    root: Node,
}

/// Collapse a path into its components, resolving `.` and `..`.
///
/// Only absolute paths reach here; the shell joins relative ones against its
/// working directory first.
fn components(path: &str) -> Result<Vec<&str>, Error> {
    let mut parts: Vec<&str> = Vec::new();

    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                // Refusing to walk above the root is what stops `../../..`
                // from escaping the filesystem.
                parts.pop();
            }
            name => parts.push(name),
        }
    }

    Ok(parts)
}

/// Split a path into its parent directory and final component.
fn split_parent(path: &str) -> Result<(Vec<&str>, &str), Error> {
    let mut parts = components(path)?;
    let name = parts.pop().ok_or(Error::InvalidPath)?;
    Ok((parts, name))
}

impl Vfs {
    pub fn new() -> Self {
        Self {
            root: Node::Directory(BTreeMap::new()),
        }
    }

    fn walk(&self, parts: &[&str]) -> Result<&Node, Error> {
        let mut node = &self.root;
        for part in parts {
            node = node.children()?.get(*part).ok_or(Error::NotFound)?;
        }
        Ok(node)
    }

    fn walk_mut(&mut self, parts: &[&str]) -> Result<&mut Node, Error> {
        let mut node = &mut self.root;
        for part in parts {
            node = node.children_mut()?.get_mut(*part).ok_or(Error::NotFound)?;
        }
        Ok(node)
    }

    pub fn exists(&self, path: &str) -> bool {
        components(path).is_ok_and(|parts| self.walk(&parts).is_ok())
    }

    pub fn is_directory(&self, path: &str) -> bool {
        components(path)
            .ok()
            .and_then(|parts| self.walk(&parts).ok())
            .is_some_and(|node| matches!(node, Node::Directory(_)))
    }

    pub fn list(&self, path: &str) -> Result<Vec<Entry>, Error> {
        let parts = components(path)?;
        let node = self.walk(&parts)?;

        Ok(node
            .children()?
            .iter()
            .map(|(name, child)| Entry {
                name: name.clone(),
                is_directory: matches!(child, Node::Directory(_)),
                size: match child {
                    Node::File(data) => data.len(),
                    Node::Directory(map) => map.len(),
                },
            })
            .collect())
    }

    pub fn read(&self, path: &str) -> Result<&[u8], Error> {
        let parts = components(path)?;
        match self.walk(&parts)? {
            Node::File(data) => Ok(data),
            Node::Directory(_) => Err(Error::IsADirectory),
        }
    }

    /// Create or overwrite a file.
    pub fn write(&mut self, path: &str, data: &[u8]) -> Result<(), Error> {
        let (parent, name) = split_parent(path)?;
        let directory = self.walk_mut(&parent)?.children_mut()?;

        match directory.get_mut(name) {
            Some(Node::File(existing)) => {
                existing.clear();
                existing.extend_from_slice(data);
            }
            Some(Node::Directory(_)) => return Err(Error::IsADirectory),
            None => {
                directory.insert(name.to_string(), Node::File(data.to_vec()));
            }
        }

        Ok(())
    }

    /// Append to a file, creating it if needed.
    pub fn append(&mut self, path: &str, data: &[u8]) -> Result<(), Error> {
        let (parent, name) = split_parent(path)?;
        let directory = self.walk_mut(&parent)?.children_mut()?;

        match directory.get_mut(name) {
            Some(Node::File(existing)) => existing.extend_from_slice(data),
            Some(Node::Directory(_)) => return Err(Error::IsADirectory),
            None => {
                directory.insert(name.to_string(), Node::File(data.to_vec()));
            }
        }

        Ok(())
    }

    pub fn mkdir(&mut self, path: &str) -> Result<(), Error> {
        let (parent, name) = split_parent(path)?;
        let directory = self.walk_mut(&parent)?.children_mut()?;

        if directory.contains_key(name) {
            return Err(Error::AlreadyExists);
        }

        directory.insert(name.to_string(), Node::Directory(BTreeMap::new()));
        Ok(())
    }

    pub fn remove(&mut self, path: &str) -> Result<(), Error> {
        let (parent, name) = split_parent(path)?;
        let directory = self.walk_mut(&parent)?.children_mut()?;

        directory.remove(name).ok_or(Error::NotFound)?;
        Ok(())
    }

    /// Total bytes held by all files.
    pub fn used_bytes(&self) -> usize {
        fn sum(node: &Node) -> usize {
            match node {
                Node::File(data) => data.len(),
                Node::Directory(map) => map.values().map(sum).sum(),
            }
        }
        sum(&self.root)
    }
}

pub static VFS: Mutex<Option<Vfs>> = Mutex::new(None);

/// Build the initial ramdisk.
pub fn init() {
    let mut vfs = Vfs::new();

    vfs.mkdir("/etc").ok();
    vfs.mkdir("/home").ok();
    vfs.mkdir("/tmp").ok();

    // Built rather than a literal so the release stamped into the banner and
    // the one greeting a user at the prompt can never disagree.
    let motd = alloc::format!(
        "Kestrel {} - an experimental x86-64 OS.\n\
         Type 'help' for commands, 'version' for this build, 'desktop' for windows.\n",
        crate::VERSION
    );
    vfs.write("/etc/motd", motd.as_bytes()).ok();

    vfs.write(
        "/README",
        b"This filesystem lives entirely in RAM.\n\
          Anything written here is lost on reboot.\n",
    )
    .ok();

    *VFS.lock() = Some(vfs);
}

/// Run `body` against the in-memory filesystem.
pub fn with<T>(body: impl FnOnce(&mut Vfs) -> T) -> Option<T> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        VFS.lock().as_mut().map(body)
    })
}

/// Where the boot disk's EFI system partition appears.
pub const DISK_MOUNT: &str = "/disk";

/// Split a path into the mounted disk, if it points there.
///
/// Returns the path relative to the mount point, so `/disk/bin/hello` becomes
/// `bin/hello` — which is what the FAT driver expects.
fn on_disk(path: &str) -> Option<&str> {
    if path == DISK_MOUNT {
        return Some("");
    }
    path.strip_prefix("/disk/")
}

/// Read a file from whichever filesystem owns the path.
pub fn read(path: &str) -> Result<Vec<u8>, Error> {
    if let Some(relative) = on_disk(path) {
        return match crate::fat::with(|fs| fs.read_file(relative)) {
            Some(Ok(Some(data))) => Ok(data),
            Some(Ok(None)) => Err(Error::NotFound),
            Some(Err(message)) => Err(Error::Io(message)),
            None => Err(Error::NotFound),
        };
    }

    with(|fs| fs.read(path).map(|data| data.to_vec())).unwrap_or(Err(Error::NotFound))
}

/// List a directory from whichever filesystem owns the path.
pub fn list(path: &str) -> Result<Vec<Entry>, Error> {
    if let Some(relative) = on_disk(path) {
        return match crate::fat::with(|fs| fs.list(relative)) {
            Some(Ok(Some(entries))) => Ok(entries
                .into_iter()
                .map(|entry| Entry {
                    name: entry.name,
                    is_directory: entry.is_directory,
                    size: entry.size as usize,
                })
                .collect()),
            Some(Ok(None)) => Err(Error::NotADirectory),
            Some(Err(message)) => Err(Error::Io(message)),
            None => Err(Error::NotFound),
        };
    }

    let mut entries = with(|fs| fs.list(path)).unwrap_or(Err(Error::NotFound))?;

    // The disk is a mount point rather than a real directory, so advertise it
    // in the root listing by hand.
    if path == "/" && crate::fat::mounted() {
        entries.push(Entry {
            name: String::from("disk"),
            is_directory: true,
            size: 0,
        });
        entries.sort_by(|a, b| a.name.cmp(&b.name));
    }

    Ok(entries)
}

pub fn exists(path: &str) -> bool {
    if let Some(relative) = on_disk(path) {
        return matches!(crate::fat::with(|fs| fs.exists(relative)), Some(Ok(true)));
    }
    with(|fs| fs.exists(path)).unwrap_or(false)
}

pub fn is_directory(path: &str) -> bool {
    if let Some(relative) = on_disk(path) {
        return matches!(
            crate::fat::with(|fs| fs.is_directory(relative)),
            Some(Ok(true))
        );
    }
    with(|fs| fs.is_directory(path)).unwrap_or(false)
}

/// Create or overwrite a file on whichever filesystem owns the path.
pub fn write(path: &str, data: &[u8]) -> Result<(), Error> {
    if let Some(relative) = on_disk(path) {
        return match crate::fat::with(|fs| fs.write_file(relative, data)) {
            Some(Ok(())) => Ok(()),
            Some(Err(message)) => Err(Error::Io(message)),
            None => Err(Error::NotFound),
        };
    }

    with(|fs| fs.write(path, data)).unwrap_or(Err(Error::NotFound))
}

/// Append to a file. On disk this is a read-modify-write, since the FAT driver
/// only knows how to replace a file wholesale.
pub fn append(path: &str, data: &[u8]) -> Result<(), Error> {
    if on_disk(path).is_some() {
        let mut combined = read(path).unwrap_or_default();
        combined.extend_from_slice(data);
        return write(path, &combined);
    }

    with(|fs| fs.append(path, data)).unwrap_or(Err(Error::NotFound))
}

pub fn mkdir(path: &str) -> Result<(), Error> {
    if let Some(relative) = on_disk(path) {
        return match crate::fat::with(|fs| fs.create_directory(relative)) {
            Some(Ok(())) => Ok(()),
            Some(Err(message)) => Err(Error::Io(message)),
            None => Err(Error::NotFound),
        };
    }

    with(|fs| fs.mkdir(path)).unwrap_or(Err(Error::NotFound))
}

pub fn remove(path: &str) -> Result<(), Error> {
    if let Some(relative) = on_disk(path) {
        // Files and directories are removed differently on FAT, so pick based
        // on what is actually there.
        let directory = is_directory(path);

        return match crate::fat::with(|fs| {
            if directory {
                fs.remove_directory(relative)
            } else {
                fs.remove_file(relative)
            }
        }) {
            Some(Ok(())) => Ok(()),
            Some(Err(message)) => Err(Error::Io(message)),
            None => Err(Error::NotFound),
        };
    }

    with(|fs| fs.remove(path)).unwrap_or(Err(Error::NotFound))
}

/// The mount point itself is not a real directory and cannot be replaced.
pub fn writable(path: &str) -> Result<(), Error> {
    if path == DISK_MOUNT {
        return Err(Error::ReadOnly);
    }
    Ok(())
}
