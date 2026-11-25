#![cfg_attr(windows, feature(windows_by_handle))]
//! Standalone host CLI: build, image, run, and bounded smoke.
//! Ownership/Lock Ordering: one command owns its scratch files and QEMU child.
//! Invariants: no Kconfig, parent paths, firmware flashing or implicit image downloads.
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
mod args;
mod build;
mod config;
mod disk;
mod dtb;
mod elf;
mod firmware;
mod launch;
mod media;
mod process;
mod smoke;
mod tty;
fn main() -> Result<()> {
    let options = args::Options::parse()?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = root.parent().context("xtask is outside the workspace")?;
    let command = options.command.as_str();
    if command == "test" {
        options.only(&[])?;
        return build::test(root);
    }
    let board = config::Board::load(root, options.value("board").unwrap_or("qemu-virt-riscv"))?;
    match command {
        "build" | "image" => {
            let allowed = if command == "image" {
                &[
                    "board",
                    "features",
                    "no-default-features",
                    "release",
                    "output",
                ][..]
            } else {
                &["board", "features", "no-default-features", "release"]
            };
            options.only(allowed)?;
            let path = build::board(
                root,
                &board,
                options.value("features"),
                options.flag("no-default-features"),
            )?;
            if command == "image" {
                let output = options
                    .value("output")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| path.with_extension("bin"));
                elf::image(&path, &output, &board)?;
                println!("image: {}", output.display());
            }
            Ok(())
        }
        "run" => launch::run(root, &board, &options),
        "smoke" => smoke::run(root, &board, &options),
        _ => bail!("unknown command {command}; use cargo xtask --help"),
    }
}
fn work(root: &Path) -> PathBuf {
    root.join("target")
}
