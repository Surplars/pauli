//! Link the Wave8 HS payload without root workspace flags.
//! Ownership/Lock Ordering: build script owns link options; no target access.
//! Invariants: the full load, BSS and stack are tested by linker ASSERT.
fn main() {
    let root = env!("CARGO_MANIFEST_DIR");
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=startup.S");
    println!("cargo:rerun-if-changed=src/guest.S");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("none") {
        println!("cargo:rustc-link-arg=-T{root}/linker.ld");
    }
}
