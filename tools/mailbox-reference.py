"""Build two offline host ELFs differing only in the target mailbox lock scope.

From hypervisor/: python tools/mailbox-reference.py target/mailbox-ab-01
Never starts QEMU or a guest. The caller must supply a NEW output directory.
"""

import argparse
import difflib
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[1]
CONTROL = Path("crates/pauli-core/src/runtime/control.rs")
TARGET = "riscv64imac-unknown-none-elf"
BOARD = "qemu-virt-riscv"
FLAGS = "-C\x1fcode-model=medium"  # same as xtask::build::target_build
BUILD = ["build", "--offline", "--locked", "--release", "--target", TARGET, "-p", BOARD]

# Fail rather than applying a stale reference transformation to a new control path.
# Drop the guard BEFORE error cleanup, which may acquire this same mailbox.
REPLACEMENTS = [
    ("""                slot.mailbox.lock().post_ipi();
                if Self::kick_posted(vm, slot).is_err() {""",
     """                let mailbox = slot.mailbox.lock();
                mailbox.post_ipi();
                let notified = Self::kick_posted(vm, slot);
                drop(mailbox);
                if notified.is_err() {"""),
    ("""                    let ticket = target.mailbox.lock().post_fence(instruction, current.index);
                    A::Smp::kick(target.owner).map(|()| ticket)""",
     """                    let mailbox = target.mailbox.lock();
                    let ticket = mailbox.post_fence(instruction, current.index);
                    let notified = A::Smp::kick(target.owner);
                    drop(mailbox);
                    notified.map(|()| ticket)"""),
]


def guarded(original):
    """Preserve original bytes and line endings outside two verified anchors."""
    result = original
    eol = b"\r\n" if b"\r\n" in original else b"\n"
    for outside, inside in REPLACEMENTS:
        old, new = (s.encode().replace(b"\n", eol) for s in (outside, inside))
        if result.count(old) != 1:
            raise ValueError("mailbox scope anchor missing or ambiguous in " + str(CONTROL))
        result = result.replace(old, new, 1)
    return result


def sources(root):
    """Only build inputs; never copy targets, repository metadata or linked trees."""
    files = {Path(p) for p in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/config.toml")}
    for tree in ("boards", "crates", "xtask"):
        for base, dirs, names in os.walk(root / tree):
            base = Path(base)
            dirs[:] = [d for d in dirs if d != "target" and not d.startswith(".")
                       and (base / d).resolve() == base / d]
            files.update((base / f).relative_to(root) for f in names
                         if Path(f).suffix in {".toml", ".rs", ".S", ".s", ".ld"})
    return sorted(files, key=Path.as_posix)


def sha(data):
    return hashlib.sha256(data).hexdigest()


def snapshot(root, work):
    # The manifest digest covers path + exact file bytes (also for untracked sources).
    hashes = {}
    for rel in sources(root):
        src = root / rel
        if src.resolve() != src:
            raise ValueError(f"linked build input: {rel}")
        data = src.read_bytes()
        dest = work / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(data)
        hashes[rel.as_posix()] = sha(data)
    return hashes


def tree_digest(hashes):
    return sha("".join(f"{h}  {p}\n" for p, h in sorted(hashes.items())).encode())


def build_pair(work, env):
    hashes = snapshot(ROOT, work)
    control = work / CONTROL
    original = control.read_bytes()
    reference = guarded(original)
    diff = "".join(difflib.unified_diff(
        original.decode().splitlines(keepends=True), reference.decode().splitlines(keepends=True),
        fromfile="a/" + CONTROL.as_posix(), tofile="b/" + CONTROL.as_posix()))
    def version(*args):
        return subprocess.check_output(args, cwd=work, env=env, text=True).strip()

    manifest = {
        "comparison": "reconstructed guard lifetime, not a historical binary",
        "board": BOARD, "target": TARGET, "features": "default", "cargo_args": BUILD,
        "rustflags": FLAGS.split("\x1f"), "rustc": version("rustc", "-vV"),
        "cargo": version("cargo", "-V"), "lock_sha256": hashes["Cargo.lock"],
        "snapshot_files": len(hashes), "guard_diff_sha256": sha(diff.encode()), "variants": {},
    }
    artifacts = {"guard-scope.patch": diff.encode()}
    for label, data in (("outside", original), ("inside", reference)):
        if label == "inside":
            control.write_bytes(data)
        subprocess.run(["cargo", *BUILD], cwd=work, env=env, check=True)
        elf = Path(env["CARGO_TARGET_DIR"]) / TARGET / "release" / BOARD
        binary = elf.read_bytes()
        hashes[CONTROL.as_posix()] = sha(data)
        manifest["variants"][label] = {
            "source_tree_sha256": tree_digest(hashes), "control_sha256": sha(data),
            "elf_sha256": sha(binary), "elf_bytes": len(binary),
        }
        artifacts[label + ".elf"] = binary
    if artifacts["outside.elf"] == artifacts["inside.elf"]:
        raise ValueError("both host ELFs are identical; reference build not demonstrated")
    artifacts["manifest.json"] = (json.dumps(manifest, indent=2) + "\n").encode()
    return artifacts


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="new results directory in an existing parent")
    out = parser.parse_args().output.absolute()
    if not out.parent.is_dir() or os.path.lexists(out):
        parser.error("output must be a NEW directory under an existing parent")
    # Same absolute source/target paths, toolchain and flags for both compilations.
    # The private target is reused, not copied or left in the original workspace.
    with tempfile.TemporaryDirectory(prefix="pauli-mailbox-ab-") as tmp:
        work = Path(tmp).resolve() / "src"
        env = dict(os.environ, CARGO_TARGET_DIR=str(Path(tmp) / "target"),
                   # Cargo template syntax: a Windows sandbox TEMP path can
                   # itself contain braces. Expand the root as one variable.
                   CARGO_BUILD_BUILD_DIR="{workspace-root}/../build",
                   CARGO_ENCODED_RUSTFLAGS=FLAGS, CARGO_NET_OFFLINE="true",
                   CARGO_INCREMENTAL="0", RUSTUP_AUTO_INSTALL="0")
        artifacts = build_pair(work, env)
    out.mkdir()  # Exclusive: a concurrent invocation cannot clobber the result.
    for name, data in artifacts.items():
        with (out / name).open("xb") as file:
            file.write(data)
        print(f"{sha(data)}  {out / name}")


if __name__ == "__main__":
    main()
