//! Cargo host tests and isolated target builds; linker choice belongs to each board.
//! Ownership/Lock Ordering: Cargo serializes artifacts under this workspace's target.
//! Invariants: do not inherit the enclosing RTOS's RISC-V link flags.
use crate::{config::Board, elf, work};
use anyhow::{Context, Result, ensure};
use std::{
    path::{Path, PathBuf},
    process::Command,
};
pub fn test(root: &Path) -> Result<()> {
    let status = cargo(root)
        .args(["test", "--workspace", "--all-features", "--locked"])
        .status()?;
    ensure!(status.success(), "workspace tests failed ({status})");
    Ok(())
}
pub fn board(root: &Path, board: &Board, features: Option<&str>) -> Result<PathBuf> {
    for name in features.into_iter().flat_map(|set| set.split(',')) {
        ensure!(
            matches!(
                name,
                "aia" | "aia-guest-file" | "guest-control" | "selftest"
            ),
            "unsupported board feature {name}"
        );
    }
    let image = binary(root, &board.target, &board.name, features)?;
    let layout = elf::inspect(&image, board)?;
    println!(
        "{}: entry={:#x}, load={} B, RAM span={} B / {} B",
        image.display(),
        layout.entry,
        layout.file_span,
        layout.ram_span,
        board.limit
    );
    Ok(image)
}
pub fn binary(root: &Path, target: &str, package: &str, features: Option<&str>) -> Result<PathBuf> {
    let mut command = target_build(root, target, package);
    if let Some(features) = features {
        command.args(["--features", features]);
    }
    let status = command.status().context("run cargo build")?;
    ensure!(status.success(), "build {package} failed ({status})");
    Ok(work(root).join(target).join("release").join(package))
}
/// The harness names a checked-in example with this board's link address.
pub fn board_example(root: &Path, board: &Board, example: &str) -> Result<PathBuf> {
    let status = target_build(root, &board.target, &board.name)
        .args(["--example", example])
        .status()
        .with_context(|| format!("build {example} reference"))?;
    ensure!(
        status.success(),
        "build {example} reference failed ({status})"
    );
    let path = work(root)
        .join(&board.target)
        .join("release")
        .join("examples")
        .join(example);
    elf::inspect(&path, board)?;
    Ok(path)
}
fn target_build(root: &Path, target: &str, package: &str) -> Command {
    let mut command = cargo(root);
    command
        .args([
            "build",
            "--locked",
            "--release",
            "--target",
            target,
            "-p",
            package,
        ])
        .env("CARGO_ENCODED_RUSTFLAGS", "-C\x1fcode-model=medium");
    command
}
fn cargo(root: &Path) -> Command {
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.current_dir(root)
        .env("CARGO_TARGET_DIR", work(root))
        .env("CARGO_INCREMENTAL", "0");
    cmd
}
