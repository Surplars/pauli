# Port an instruction-set backend

Implement `pauli_core::arch::HvArch` in a separate ISA crate. Board code owns the
native bus, firmware contract and resource discovery. Guest instruction
decoding, trap entry, architectural register banks and page-table formats
stay in the ISA crate. `pauli-core` must not import it.

| Contract | Must provide |
| --- | --- |
| `ArchVcpu` | One pinned-owner `run(RunOptions)` and validated secondary boot; scalar exit/control results |
| `Stage2Space` | Retained private root/arena, checked mappings and executable entry validation |
| `VirtualIrqChip` | Construct per-VM controller state from `GuestIrqConfig`, deliver pending interrupts per vCPU and quiesce on retirement |
| `ArchTimer`, `ArchSmp`, `LocalIrq` | Absolute native deadlines, physical CPU owners, safe wake/idle and interrupt restoration |
| `HvArch::write_linux_dtb` | Canonical bounded guest description shared with the launcher, or explicitly unsupported; advertise admitted CPU/IRQ topology, RAM, initrd and board timebase |
| `Resident`, `Mailbox` | Put native banks before handoff; epoch-bound remote request/completion and translation retirement |

## Order of implementation

1. Implement the native timer, IRQ mask and single-CPU trap entry. Check a
   minimal guest round trip and a native deadline while a guest is running.
2. Enforce private stage-2 authority and fail-closed MMIO. Decide instruction
   origin, width, alignment and permissions before calling `GuestIo`. Do
   not change the guest PC or destination register when decoding or device
   access fails. Physical DMA is never part of guest MMIO emulation.
3. Define one context layout, complete bank ownership and VM-local guest
   firmware semantics. In RV64, HCSRs and HS/VS swap stay under
   `pauli-arch-riscv`. Preserve F/D even if guest status claims it is clean;
   expose only guest TIME in counter delegation and trap guest WFI.
4. Implement second-stage and guest-stage retirement, then remote workers.
   Each native owner acknowledges only its own completed fences. Do not
   recycle VM IDs, RAM, mailbox epochs or hardware IRQ files before every
   relevant owner is out, saved, and retired. Publish guest code before
   initial entry and secondary starts.
   A kick arriving after the worker's final scan must survive until its
   next scan, even if a host interrupt handler runs before native idle.
   Test that window with the local timer disarmed: a periodic tick can
   otherwise hide a lost wake. An IPI wakes the physical owner; it does
   not by itself authorize injecting an interrupt into any guest.
5. Validate the ISA in isolation, then add a board backend and boot fixture.
   Keep algorithmic decode/IRQ/mailbox tests host-executable. Put
   hardware-only probes behind `selftest`, out of production boot.

The contract carries neutral exit/lifecycle outcomes. It is a seam for
another architecture such as AArch64, not an implementation of that
architecture. Adding a port requires an actual native world switch and
board-level acceptance; substituting new register names is not enough.
