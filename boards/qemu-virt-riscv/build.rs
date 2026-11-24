//! Link the board payload with its own script, independent of the caller cwd.
//! Ownership/Lock Ordering: build-only environment, no target memory access.
//! Invariants: absolute script path; native host tests do not use a target linker.
fn main() {
    let root = env!("CARGO_MANIFEST_DIR");
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=startup.S");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("none") {
        println!("cargo:rustc-link-arg=-T{root}/linker.ld");
    }
}
