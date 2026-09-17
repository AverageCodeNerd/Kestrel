//! Stage an update payload and serve it to a running Kestrel.
//!
//! Kestrel can replace its own kernel and packages from inside itself:
//!
//! ```text
//! update from http://host/path    where to look
//! update                          what is on offer
//! update install                  take it
//! ```
//!
//! The kernel asks for three things, all relative to the source it was given:
//!
//! * `manifest` — plain text: one directive per line, `version`, `kernel` and
//!   one `package` line per offered program;
//! * `kestrel` — the kernel ELF itself;
//! * `repo/<name>` — each package the manifest lists.
//!
//! The update writes those into `/disk/boot` and `/disk/repo` on the boot
//! media. The catalogue that describes the packages is one of them: it lives
//! in the same directory on the ESP, so a system that picked the update up
//! halfway through a rebuild would otherwise boot with the new kernel and the
//! old description of what it can do.
//!
//! This stages exactly what `assemble_esp` puts on the boot media, writes the
//! manifest from the sizes those files actually have, and then serves the
//! directory over plain HTTP. It is not a release server and does not want to
//! be one; it is the "where to look" for a machine that boots from a build
//! directory or a local VM.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

/// Default port, high and arbitrary so nothing is expected to be here. The
/// kernel's HTTP client takes the port from the URL, so it can be anything.
pub const DEFAULT_PORT: u16 = 8088;

/// This must agree with the parser in `kernel/src/update.rs`. There are only
/// three directives, and they are simple on purpose: the parser has to live in
/// a kernel.
const MANIFEST_PATH: &str = "/manifest";
const KERNEL_PATH: &str = "/kestrel";
const REPO_PREFIX: &str = "/repo/";

/// The catalogue is not a program, but it is a file the boot media carries in
/// the same directory the packages live in, and an update that forgot it would
/// silently keep the old description while shipping a new kernel.
const CATALOGUE: &str = "catalogue";

/// Stage one directory containing everything a running Kestrel needs to bring
/// itself up to this build, and write the manifest that describes it.
///
/// The payload must be byte-identical to what an image build ships, so it is
/// copied out of the staged ESP rather than assembled a second time: a file
/// that drifted between the image and the update would be an update that
/// installed something other than what was tested.
pub fn stage(root: &Path, out: &Path, packages: &[&str]) -> Result<Staged, String> {
    let version = version(root)?;

    let esp = root.join("build/esp");
    let source = esp.join("boot/kestrel");
    if !source.exists() {
        return Err(format!(
            "nothing to serve: {} is missing (build first, or pass --no-build after one).",
            source.display()
        ));
    }

    fs::create_dir_all(out).map_err(|e| format!("could not create {}: {e}", out.display()))?;
    fs::create_dir_all(out.join("repo")).map_err(|e| format!("could not create {}: {e}", out.display()))?;

    // The whole point of the manifest is that its sizes match the files that
    // will arrive, so the sizes come from the copies that were just made.
    let kernel_size = copy(&source, &out.join("kestrel"))?;
    check_kernel_elf(&out.join("kestrel"))?;

    let mut manifest = format!("version {version}\nkernel {kernel_size}\n");

    for name in packages.iter().copied().chain(std::iter::once(CATALOGUE)) {
        let source = esp.join("repo").join(name);
        if !source.exists() {
            return Err(format!(
                "{} is listed as a package but was not staged in the ESP",
                source.display()
            ));
        }
        let size = copy(&source, &out.join("repo").join(name))?;
        manifest.push_str(&format!("package {name} {size}\n"));
    }

    fs::write(out.join("manifest"), manifest)
        .map_err(|e| format!("could not write the manifest: {e}"))?;

    Ok(staged(version, kernel_size, packages))
}

pub struct Staged {
    pub version: String,
    pub kernel_size: u64,
    pub package_count: usize,
}

fn staged(version: String, kernel_size: u64, packages: &[&str]) -> Staged {
    Staged {
        version,
        kernel_size,
        package_count: packages.len() + 1,
    }
}

/// The version this build was made as, from `kernel/Cargo.toml` — the same
/// place `release.ps1` and the kernel's banner read it, so a bump in one place
/// still means an update everywhere. Version is the version, and the manifest
/// has to say the same thing the kernel prints, or the machine compares it
/// against its own banner and concludes there is nothing to take.
fn version(root: &Path) -> Result<String, String> {
    let manifest = fs::read_to_string(root.join("kernel/Cargo.toml"))
        .map_err(|e| format!("could not read kernel/Cargo.toml: {e}"))?;
    // A `version = "x"` line, in the [package] table. Line-scoped so a stray
    // mention of the word elsewhere cannot be mistaken for the version.
    for line in manifest.lines() {
        let mut parts = line.split('=');
        if !parts.next().unwrap_or("").trim().starts_with("version") {
            continue;
        }
        let Some(quoted) = parts.next() else { continue };
        let quoted = quoted.trim();
        let Some(value) = quoted.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
            continue;
        };
        if !value.is_empty() {
            return Ok(value.to_string());
        }
    }
    Err("kernel/Cargo.toml has no version".into())
}

/// The same check the running kernel applies before it lets a downloaded
/// kernel replace anything: it has to be an executable ELF, or a page served
/// by mistake was about to become an unbootable machine.
fn check_kernel_elf(path: &Path) -> Result<(), String> {
    let image = fs::read(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    if image.len() < 64 || &image[..4] != b"\x7fELF" {
        return Err(format!("{} is not an ELF executable", path.display()));
    }
    if u16::from_le_bytes([image[16], image[17]]) != 2 {
        return Err(format!(
            "{} is an ELF but not an executable one (type {}); refusing to serve it",
            path.display(),
            u16::from_le_bytes([image[16], image[17]])
        ));
    }
    Ok(())
}

fn copy(from: &Path, to: &Path) -> Result<u64, String> {
    fs::copy(from, to).map_err(|e| format!("copy {} -> {}: {e}", from.display(), to.display()))
}

/// Serve a staged payload until the process is stopped.
///
/// Bound to all interfaces: QEMU's user-mode network reaches the host as
/// 10.0.2.2, while a machine on the local network comes in through whichever
/// address it actually has, so 127.0.0.1 would only serve the first of those.
pub fn serve(dir: &Path, port: u16) -> std::io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port))?;

    println!();
    println!("update server  : listening on port {port}");
    println!();
    println!("what is offered: manifest, kestrel and the repo, staged in {}", dir.display());
    println!();
    println!("in the guest   : update from http://10.0.2.2:{port}/");
    println!("                 (QEMU forwards the guest to the host)");
    println!("                 update");
    println!("                 update install");
    println!("on this machine: http://127.0.0.1:{port}/");
    println!("from the LAN   : http://<this-host's-ip>:{port}/");
    println!();
    println!("press Ctrl+C to stop");

    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                let dir = dir.to_path_buf();
                std::thread::spawn(move || handle(stream, &dir));
            }
            Err(_) => continue,
        }
    }
    Ok(())
}

enum Route {
    Manifest,
    Kernel,
    Package(String),
    NotFound,
}

fn route(path: &str) -> Route {
    match path {
        MANIFEST_PATH => Route::Manifest,
        KERNEL_PATH => Route::Kernel,
        _ => match path.strip_prefix(REPO_PREFIX) {
            Some(name) if !name.is_empty() && !name.contains('/') && !name.contains('\\') => {
                // `..` needs a slash to escape, and it does not have one here,
                // so `dir/repo/..` resolves to `dir` (a directory, which the
                // read below refuses) — but refusing it outright is cheaper to
                // reason about than relying on that refusal.
                if name == "." || name == ".." {
                    Route::NotFound
                } else {
                    Route::Package(name.to_string())
                }
            }
            _ => Route::NotFound,
        },
    }
}

fn handle(mut stream: TcpStream, dir: &Path) {
    let target = match read_request(&mut stream) {
        Some(target) => target,
        None => return,
    };

    // The kernel is HTTP/1.0 with Connection: close, and knows the body is
    // complete from the closed connection — so every reply closes, whatever
    // the request said.
    let content_type = content_type_for(&target);
    let (status, body) = match serve_file(dir, target) {
        Ok(bytes) => ("200 OK", bytes),
        Err(ServeError::NotFound) => ("404 Not Found", b"not found\n".as_slice().to_vec()),
        Err(e) => ("500 Internal Server Error", format!("{e}\n").into_bytes()),
    };

    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(&body);
}

enum ServeError {
    NotFound,
    Io(std::io::Error),
}

impl core::fmt::Display for ServeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ServeError::NotFound => write!(f, "not found"),
            ServeError::Io(e) => write!(f, "{e}"),
        }
    }
}

/// Read the request line. The whole header block is drained first so a keep
/// alive client does not get its next request answered with this response.
fn read_request(stream: &mut TcpStream) -> Option<String> {
    let mut buffer = [0u8; 8192];
    let read = stream.read(&mut buffer).ok()?;
    if read == 0 {
        return None;
    }

    // The request line is everything up to the first CR or LF.
    let head = &buffer[..read];
    let end = head.iter().position(|&b| b == b'\r' || b == b'\n')?;
    let line = core::str::from_utf8(&head[..end]).ok()?;

    // "GET <path> HTTP/1.x"
    let mut parts = line.split_whitespace();
    if parts.next()? != "GET" {
        return None;
    }
    parts.next().map(String::from)
}

fn serve_file(dir: &Path, target: String) -> Result<Vec<u8>, ServeError> {
    let path: PathBuf = match route(&target) {
        Route::Manifest => dir.join("manifest"),
        Route::Kernel => dir.join("kestrel"),
        Route::Package(name) => dir.join("repo").join(name),
        Route::NotFound => return Err(ServeError::NotFound),
    };
    fs::read(&path).map_err(|e| match e.kind() {
        // A name the route accepted but no file backs is a miss, not a server
        // failure. Without this a stale manifest would answer 500 for every
        // file it lists, which reads as a server crash when the real problem
        // is a mismatch between the manifest and the staged payload.
        std::io::ErrorKind::NotFound => ServeError::NotFound,
        _ => ServeError::Io(e),
    })
}

fn content_type_for(target: &str) -> &'static str {
    if target == MANIFEST_PATH {
        "text/plain"
    } else if target == KERNEL_PATH || target.starts_with(REPO_PREFIX) {
        "application/octet-stream"
    } else {
        "text/plain"
    }
}