#!/usr/bin/env python3
"""Package built Wave9 artifacts for the OS task; never writes to a live rootfs."""
import argparse
import hashlib
import io
from pathlib import Path
import re
import tarfile


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ("firmware", "elf", "paulictl", "module", "output"):
        p.add_argument("--" + name, type=Path, required=True)
    args = p.parse_args()
    source = Path(__file__).resolve().parent
    module = args.module.read_bytes()
    # Use the module's actual kernel release, not the packaging host's uname.
    release = re.search(rb"\0vermagic=([a-zA-Z0-9._+\-]+) ", module)
    if not release:
        p.error("missing kernel release in module vermagic")
    files = {
        "README.md": (source / "WAVE9-HANDOFF.md", 0o644),
        "firmware/valence-250k.bin": (args.firmware, 0o644),
        "firmware/valence-250k.elf": (args.elf, 0o644),
        "rootfs/usr/sbin/paulictl": (args.paulictl, 0o755),
        f"rootfs/lib/modules/{release[1].decode()}/extra/pauli.ko": (args.module, 0o644),
    }
    # Explicit source inputs: local build outputs must not leak into a release.
    for name in ("README.md", "driver/README.md", "driver/pauli.c", "driver/Makefile",
                 "driver/Kconfig", "driver/build-image-module.sh", "driver/test-pauli.c",
                 "driver/pauli-events.h", "driver/test-events.h",
                 "include/uapi/linux/pauli.h",
                 "tool/paulictl.c", "tool/test-paulictl.c", "tool/Makefile"):
        files["source/" + name] = (source / name, 0o755 if name.endswith(".sh") else 0o644)

    def add(tar, name, data, mode):
        entry = tarfile.TarInfo(name)
        entry.mode, entry.size = mode, len(data)
        tar.addfile(entry, io.BytesIO(data))

    with args.output.open("xb") as output, tarfile.open(fileobj=output, mode="w:gz") as tar:
        sums = []
        for name, (path, mode) in sorted(files.items()):
            data = path.read_bytes()
            add(tar, name, data, mode)
            sums.append(f"{hashlib.sha256(data).hexdigest()}  {name}\n")
        add(tar, "SHA256SUMS", "".join(sums).encode(), 0o644)


if __name__ == "__main__":
    main()
