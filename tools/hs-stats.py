"""Decode one halted RV64 boot hart's little-endian PAULI_HS_STATS v1 dump."""

import argparse
from pathlib import Path
import struct


MAGIC = 0x5041_554C_4953_5431
LAYOUT = struct.Struct("<16Q")
FIELDS = (
    "magic", "version", "entries", "guest_ticks", "hs_ticks", "idle_ticks", "slices",
    "exit_timer", "exit_external", "exit_software", "exit_sbi", "exit_mmio", "exit_wfi",
    "exit_other", "last_tick", "sstc_active",
)
CAPTURE = """Halt the boot hart between trace callbacks for consistent cross-field totals.
With GDB, the matching ELF, and your JTAG connection:
  (gdb) set language c
  (gdb) set $stats = (unsigned char *)&PAULI_HS_STATS
  (gdb) dump binary memory hs-stats.bin $stats $stats+128

If using the Valence banner, replace &PAULI_HS_STATS with its stats=0x... address.
Then run: python tools/hs-stats.py hs-stats.bin"""


def decode(data):
    if len(data) != LAYOUT.size:
        raise ValueError(f"expected exactly {LAYOUT.size} bytes, got {len(data)}")
    stats = dict(zip(FIELDS, LAYOUT.unpack(data)))
    if stats["magic"] != MAGIC:
        raise ValueError(f"invalid PAULI_HS_STATS magic 0x{stats['magic']:016x}")
    if stats["version"] != 1:
        raise ValueError(f"unsupported PAULI_HS_STATS version {stats['version']}")
    return stats


def main():
    parser = argparse.ArgumentParser(description=__doc__, epilog=CAPTURE,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("dump", type=Path, help="128-byte GDB binary memory dump")
    args = parser.parse_args()
    try:
        stats = decode(args.dump.read_bytes())
    except (OSError, ValueError) as error:
        parser.error(str(error))

    print("PAULI_HS_STATS v1 (boot hart)")
    print(f"  sstc_active   {stats['sstc_active']}  (native boot-hart timer, 0 = disabled, 1 = enabled)")
    print(f"  slices        {stats['slices']}")
    print(f"  entries       {stats['entries']}")
    print("Cumulative timebase ticks")
    print(f"  guest_ticks   {stats['guest_ticks']}  (includes the assembly switch)")
    print(f"  hs_ticks      {stats['hs_ticks']}  (gaps between measured intervals)")
    print(f"  idle_ticks    {stats['idle_ticks']}  (measured idle intervals)")
    print(f"  last_tick     {stats['last_tick']}  (absolute tick at end of last recorded interval)")
    print("Cumulative exit counts (timer, external and software are native interrupts)")
    for name in FIELDS[7:14]:
        print(f"  {name:13} {stats[name]}")


if __name__ == "__main__":
    main()
