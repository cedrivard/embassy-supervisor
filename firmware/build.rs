//! Places `memory.x` on the linker search path and passes the RP2350 link args.
use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    let out = &PathBuf::from(env::var_os("OUT_DIR").unwrap());
    // `--features ota` runs the firmware from the ACTIVE partition under the
    // bootloader; the default build is a standalone single-image layout.
    let layout = if env::var_os("CARGO_FEATURE_OTA").is_some() {
        "memory-ota.x"
    } else {
        "memory.x"
    };
    let bytes = std::fs::read(layout).expect("memory layout file");
    File::create(out.join("memory.x")).unwrap().write_all(&bytes).unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=memory-ota.x");

    // cortex-m-rt's link.x (consumes our memory.x), defmt's RTT sections, and
    // --nmagic (RP2350 flash needs unpadded segments). The memory.x SECTIONS
    // add the RP2350 boot blocks; no separate link-rp.x is required.
    println!("cargo:rustc-link-arg-bins=--nmagic");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");
    println!("cargo:rustc-link-arg-bins=-Tdefmt.x");
}
