"""Offline invariants of the mailbox reference builder; does not invoke cargo."""
import importlib.util
from pathlib import Path
import tempfile
import unittest


SPEC = importlib.util.spec_from_file_location("mailbox_reference", Path(__file__).with_name("mailbox-reference.py"))
REF = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(REF)


class GuardScope(unittest.TestCase):
    def test_both_guards_end_after_kick_and_before_cleanup(self):
        original = (REF.ROOT / REF.CONTROL).read_bytes()
        for eol in (b"\n", b"\r\n"):
            source = original.replace(b"\r\n", b"\n").replace(b"\n", eol)
            restored = REF.guarded(source)
            calls = ("Self::kick_posted(vm, slot)", "A::Smp::kick(target.owner)")
            self.assertEqual(len(calls), len(REF.REPLACEMENTS))
            for call, (outside, inside) in zip(calls, REF.REPLACEMENTS):
                self.assertEqual(outside.count(call), 1)
                self.assertEqual(inside.count(call), 1)
                self.assertLess(inside.index(call), inside.index("drop(mailbox)"))
                tail = inside.split("drop(mailbox);", 1)[1].strip()
                self.assertTrue(tail.startswith(("if notified", "notified.map(")))
                old, new = (part.encode().replace(b"\n", eol) for part in (outside, inside))
                self.assertEqual(restored.count(new), 1)
                restored = restored.replace(new, old)
            self.assertEqual(restored, source)
        for invalid in (b"empty", original + original, REF.guarded(original)):
            with self.assertRaises(ValueError):
                REF.guarded(invalid)

    def test_snapshot_hashes_dirty_sources_and_excludes_artifacts(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp).resolve() / "fixture"
            work = root.parent / "copy"
            names = ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/config.toml",
                     "boards/example/startup.S", "crates/foo/src/local.rs", "xtask/src/main.rs")
            for p in (*names, ".git/config", "target/old.rs", "crates/foo/target/junk.rs",
                      "boards/example/disk.raw", "xtask/.history/old.rs"):
                path = root / p
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(p.encode())
            hashes = REF.snapshot(root, work)
            self.assertEqual(set(hashes), set(names))
            self.assertEqual(set(names), {p.relative_to(work).as_posix() for p in work.rglob("*") if p.is_file()})
            original = REF.tree_digest(hashes)
            (root / "crates/foo/src/local.rs").write_bytes(b"edited in worktree")
            self.assertNotEqual(original, REF.tree_digest(REF.snapshot(root, work)))


if __name__ == "__main__":
    unittest.main()
