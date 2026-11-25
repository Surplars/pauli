"""Offline tests for the little-endian PAULI_HS_STATS v1 layout."""

import importlib.util
from pathlib import Path
import struct
import unittest


SPEC = importlib.util.spec_from_file_location("hs_stats", Path(__file__).with_name("hs-stats.py"))
STATS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(STATS)


class HsStatsDump(unittest.TestCase):
    def test_v1_field_offsets(self):
        values = [0x5041554C49535431, 1, *range(2, 15), 1]
        data = struct.pack("<16Q", *values)
        self.assertEqual(STATS.decode(data), {
            "magic": values[0], "version": 1, "entries": 2, "guest_ticks": 3, "hs_ticks": 4,
            "idle_ticks": 5, "slices": 6, "exit_timer": 7, "exit_external": 8,
            "exit_software": 9, "exit_sbi": 10, "exit_mmio": 11, "exit_wfi": 12,
            "exit_other": 13, "last_tick": 14, "sstc_active": 1,
        })

    def test_rejects_bad_size_magic_endianness_and_version(self):
        for length in (0, 127, 129, 256):
            with self.subTest(size=length), self.assertRaisesRegex(ValueError, "128 bytes"):
                STATS.decode(bytes(length))
        for order, magic, version, message in (
            ("<", 0, 1, "magic"), (">", STATS.MAGIC, 1, "magic"), ("<", STATS.MAGIC, 2, "version"),
        ):
            with self.subTest(order=order, magic=magic, version=version):
                with self.assertRaisesRegex(ValueError, message):
                    STATS.decode(struct.pack(f"{order}16Q", magic, version, *([0] * 14)))


if __name__ == "__main__":
    unittest.main()
