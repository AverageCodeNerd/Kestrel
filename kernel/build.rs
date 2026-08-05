use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let linker_script = manifest_dir.join("linker.ld");

    // lld wants forward slashes even on Windows.
    let path = linker_script.display().to_string().replace('\\', "/");

    println!("cargo:rustc-link-arg=-T{path}");
    // Limine loads at 4 KiB granularity; without this lld aligns segments to
    // 2 MiB and pads the executable out to a ridiculous size.
    println!("cargo:rustc-link-arg=-zmax-page-size=0x1000");

    println!("cargo:rerun-if-changed=linker.ld");
}
