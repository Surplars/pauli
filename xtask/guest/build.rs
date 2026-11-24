//! Static Linux RV64 smoke PID1 link; no Linux rustup target or libc required.
//! Ownership/Lock Ordering: build-only, no native device access.
//! Invariants: ordinary Linux user ELF, with no OpenSBI/board linker script.
fn main() {
    println!("cargo:rerun-if-changed=linker.ld");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("none") {
        println!(
            "cargo:rustc-link-arg=-T{}/linker.ld",
            env!("CARGO_MANIFEST_DIR")
        );
    }
}
