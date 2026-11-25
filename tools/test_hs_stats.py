"""Offline wire and report tests for both PAULI_HS_STATS versions."""

import importlib.util
import io
from contextlib import redirect_stdout
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
        for length in (16, 127, 129, 256):
            with self.subTest(size=length), self.assertRaisesRegex(ValueError, "128 bytes"):
                STATS.decode(struct.pack("<2Q", STATS.MAGIC, 1) + bytes(length - 16))
        with self.assertRaisesRegex(ValueError, "prefix"):
            STATS.decode(bytes(15))
        for order, magic, version, message in (
            ("<", 0, 1, "magic"), (">", STATS.MAGIC, 1, "magic"), ("<", STATS.MAGIC, 3, "version"),
        ):
            with self.subTest(order=order, magic=magic, version=version):
                with self.assertRaisesRegex(ValueError, message):
                    STATS.decode(struct.pack(f"{order}16Q", magic, version, *([0] * 14)))


class PerOwnerDump(unittest.TestCase):
    # Build these from the documented wire offsets, independently of the decoder.
    header = (0x5041554C49535431, 2, 64, 128, 3, 0b100, 0b010, 0)
    first = (41, *range(1, 15), 0)
    idle = (0, *([0] * 15))  # Hart 0 may have a nonzero logical owner index.
    last = (1024, *range(101, 115), 0)
    missing = ((1 << 64) - 1, *([0] * 15))

    @staticmethod
    def dump(header, *owners):
        return struct.pack("<8Q", *header) + b"".join(struct.pack("<16Q", *o) for o in owners)

    def test_full_and_trimmed_have_identical_active_records(self):
        records = (self.first, self.idle, self.last)
        short = self.dump(self.header, *records)
        full = self.dump(self.header, *records, *([self.missing] * 61))
        self.assertEqual((len(short), len(full)), (448, 8256))
        stats = STATS.decode(full)
        self.assertEqual(STATS.decode(short), stats)
        self.assertEqual(stats["host_cpus"], 3)
        self.assertEqual(stats["sstc_owners"], 4)
        self.assertEqual(stats["exclusive_owners"], 2)
        self.assertEqual([o["hart_id"] for o in stats["owners"]], [41, 0, 1024])
        self.assertEqual(stats["owners"][2], {
            "hart_id": 1024, "vm_exits": 101, "wfi_exits": 102, "sbi_timer_exits": 103,
            "sstc_timer_exits": 104, "mmio_exits": 105, "other_exits": 106, "vtimer_irqs": 107,
            "ext_irqs": 108, "last_scause": 109, "last_sepc": 110, "last_stval": 111,
            "idle_wfi": 112, "mail_sends": 113, "mail_recvs": 114, "reserved": 0,
        })

    def test_totals_exclude_registers_and_distinguish_unregistered_from_hart_zero(self):
        data = self.dump(self.header, self.first, self.idle, self.last)
        output = io.StringIO()
        with redirect_stdout(output):
            STATS.print_v2(STATS.decode(data))
        by_owner, totals = output.getvalue().split("Aggregate totals\n")
        self.assertIn("owner=1 hart_id=0", by_owner)
        self.assertIn("last_scause      0x000000000000006d", by_owner)
        self.assertEqual(dict(line.split() for line in totals.splitlines()), {
            "vm_exits": "102", "wfi_exits": "104", "sbi_timer_exits": "106",
            "sstc_timer_exits": "108", "mmio_exits": "110", "other_exits": "112",
            "vtimer_irqs": "114", "ext_irqs": "116", "idle_wfi": "124",
            "mail_sends": "126", "mail_recvs": "128",
        })
        data = self.dump(self.header, self.first, self.missing, self.last)
        output = io.StringIO()
        with redirect_stdout(output):
            STATS.print_v2(STATS.decode(data))
        self.assertNotIn("owner=1 ", output.getvalue())
        self.assertIn("owner=2 hart_id=1024", output.getvalue())

    def test_v2_accepts_boundary_owner_counts_and_high_mask_bits(self):
        for cpus in (1, 64):
            header = (*self.header[:4], cpus, 1 << (cpus - 1), 1 << (cpus - 1), 0)
            dump = self.dump(header, *([self.missing] * (cpus - 1)), self.idle)
            self.assertEqual(STATS.decode(dump)["owners"][-1]["hart_id"], 0)

    def test_v2_rejects_inconsistent_headers_and_dump_lengths(self):
        for field, bad in ((2, 128), (3, 64), (4, 0), (4, 65), (5, 8), (6, 8), (7, 1)):
            header = list(self.header)
            header[field] = bad
            with self.subTest(field=field, value=bad), self.assertRaises(ValueError):
                STATS.decode(self.dump(header, self.first, self.idle, self.last))
        dump = self.dump(self.header, self.first, self.idle, self.last)
        for data in (dump[:63], dump[:64], dump[:-128], dump[:-1], dump + b"\0",
                     dump + bytes(128), dump + bytes(8257 - len(dump))):
            with self.subTest(length=len(data)), self.assertRaises(ValueError):
                STATS.decode(data)


if __name__ == "__main__":
    unittest.main()
