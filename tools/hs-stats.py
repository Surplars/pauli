"""Decode halted RV64 PAULI_HS_STATS v1 or per-owner v2 counters."""

import argparse
from pathlib import Path
import struct


MAGIC = 0x5041_554C_4953_5431
PREFIX = struct.Struct("<2Q")
RECORD = struct.Struct("<16Q")
V1_FIELDS = (
    "magic", "version", "entries", "guest_ticks", "hs_ticks", "idle_ticks", "slices",
    "exit_timer", "exit_external", "exit_software", "exit_sbi", "exit_mmio", "exit_wfi",
    "exit_other", "last_tick", "sstc_active",
)
HEADER = struct.Struct("<8Q")
HEADER_FIELDS = (
    "magic", "version", "header_bytes", "owner_stride", "host_cpus",
    "sstc_owners", "exclusive_owners", "reserved",
)
OWNER_FIELDS = (
    "hart_id", "vm_exits", "wfi_exits", "sbi_timer_exits", "sstc_timer_exits",
    "mmio_exits", "other_exits", "vtimer_irqs", "ext_irqs", "last_scause",
    "last_sepc", "last_stval", "idle_wfi", "mail_sends", "mail_recvs", "reserved",
)
COUNTERS = OWNER_FIELDS[1:9] + OWNER_FIELDS[12:15]
MAX_OWNERS = 64
UNREGISTERED = (1 << 64) - 1
CAPTURE = """Halt all harts between trace callbacks for consistent cross-field totals.
With GDB, the matching ELF, and your JTAG connection:
  (gdb) set language c
  (gdb) set $stats = (unsigned char *)&PAULI_HS_STATS
  (gdb) dump binary memory hs-stats.bin $stats $stats+8256

For v2 you can also capture 64 + 128 * host_cpus bytes. For v1 capture 128 bytes.
The Valence banner's stats= address can be used in place of &PAULI_HS_STATS."""


def decode(data):
    if len(data) < PREFIX.size:
        raise ValueError("truncated PAULI_HS_STATS prefix")
    magic, version = PREFIX.unpack_from(data)
    if magic != MAGIC:
        raise ValueError(f"invalid PAULI_HS_STATS magic 0x{magic:016x}")
    if version == 1:
        if len(data) != RECORD.size:
            raise ValueError(f"v1 expected exactly 128 bytes, got {len(data)}")
        return dict(zip(V1_FIELDS, RECORD.unpack(data)))
    if version != 2:
        raise ValueError(f"unsupported PAULI_HS_STATS version {version}")
    if len(data) < HEADER.size:
        raise ValueError("truncated v2 header")
    stats = dict(zip(HEADER_FIELDS, HEADER.unpack_from(data)))
    if stats["header_bytes"] != HEADER.size or stats["owner_stride"] != RECORD.size:
        raise ValueError("invalid v2 header size or owner stride")
    cpus = stats["host_cpus"]
    if not 1 <= cpus <= MAX_OWNERS:
        raise ValueError(f"invalid host_cpus={cpus} (collection must be initialized)")
    if (stats["sstc_owners"] | stats["exclusive_owners"]) >> cpus:
        raise ValueError("owner mask exceeds host_cpus")
    if stats["reserved"]:
        raise ValueError("nonzero reserved v2 header word")
    trimmed = HEADER.size + cpus * RECORD.size
    full = HEADER.size + MAX_OWNERS * RECORD.size
    if len(data) not in (trimmed, full):
        raise ValueError(f"v2 expected {trimmed} or {full} bytes, got {len(data)}")
    stats["owners"] = [dict(zip(OWNER_FIELDS, words))
                       for words in RECORD.iter_unpack(data[HEADER.size:trimmed])]
    return stats


def print_v1(stats):
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
    for name in V1_FIELDS[7:14]:
        print(f"  {name:13} {stats[name]}")


def print_v2(stats):
    print("PAULI_HS_STATS v2")
    print(f"host_cpus={stats['host_cpus']} sstc_owners={stats['sstc_owners']:#x} "
          f"exclusive_owners={stats['exclusive_owners']:#x}")
    totals = dict.fromkeys(COUNTERS, 0)
    for index, owner in enumerate(stats["owners"]):
        if owner["hart_id"] == UNREGISTERED:
            continue
        print(f"owner={index} hart_id={owner['hart_id']}")
        for name in COUNTERS:
            print(f"  {name:16} {owner[name]}")
            totals[name] += owner[name]
        for name in ("last_scause", "last_sepc", "last_stval"):
            print(f"  {name:16} {owner[name]:#018x}")
    print("Aggregate totals")
    for name, value in totals.items():
        print(f"  {name:16} {value}")


def main():
    parser = argparse.ArgumentParser(description=__doc__, epilog=CAPTURE,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("dump", type=Path, help="little-endian GDB or JTAG binary memory dump")
    args = parser.parse_args()
    try:
        stats = decode(args.dump.read_bytes())
    except (OSError, ValueError) as error:
        parser.error(str(error))
    if stats["version"] == 1:
        print_v1(stats)
    else:
        print_v2(stats)


if __name__ == "__main__":
    main()
