use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let script = manifest_dir.join("linker.ld");
    let path = script.display().to_string().replace('\\', "/");

    println!("cargo:rustc-link-arg=-T{path}");
    println!("cargo:rustc-link-arg=-zmax-page-size=0x1000");
    println!("cargo:rerun-if-changed=linker.ld");
}
