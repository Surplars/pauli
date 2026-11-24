# Pauli

OpenIon's standalone, bare-metal Type-1 hypervisor in Rust for isolated Linux
and real-time firmware domains, built for multi-architecture portability
(starting with RISC-V RV64 with the H extension). It starts in HS mode after
OpenSBI, owns its vCPU scheduler and console, and does not use the OpenIon RTOS
as its host or depend on its scheduler, VFS, Kconfig or tools. Linux is
optional: the host monitor can manage a small RTOS VM and a bare service
with neither a management OS nor a disk. See [the diskless example](docs/firmware-guests.md).

The [Valence firmware](docs/porting-board.md#valence-firmware-handoff)
is an OpenSBI HS payload. It boots an embedded integer guest by default, or a
[packaged external Linux guest](docs/wave8-external-guest.md) without relinking.
On a physical external-Linux boot the UART goes directly to Linux. The
[Linux driver and paulictl](integrations/linux/README.md) let the management
guest control its authorized service VM over SBI without a host prompt.
It ships separately from OpenSBI, the OS image and the bitstream. An offline
build is not a board boot.

## Five layers

| Layer | Responsibility |
| --- | --- |
| 5. Boards | Firmware handoff, physical RAM reservations, native IRQ and device wiring |
| 4. ISA / native drivers | RV64 world switch, G-stage, timer/SMP; UART, VirtIO and RTC |
| 3. VMM core | VM ownership, vCPU dispatch, device routing, epoch lifecycle, monitor |
| 2. Protocols | Boot manifest, management permissions, pure virtual device models |
| 1. HAL | Bounded physical ranges, FDT, synchronization and native device traits |

## Quick start

Run from this directory. Use the nightly in [rust-toolchain.toml](rust-toolchain.toml)
with its RV64 bare-metal target installed; QEMU is required only for run/smoke.

```sh
cargo xtask test
cargo xtask build --board qemu-virt-riscv
cargo xtask smoke --suite hv
```

The last command is a local QEMU test, not an offline check. For the Linux
runtime gate, use an existing raw RV64 Linux Image with the capabilities
listed in [VM configuration and commands](docs/vm-config.md):

```sh
cargo xtask smoke --suite linux --kernel /path/to/Image
cargo xtask smoke --suite smp --kernel /path/to/Image
cargo xtask smoke --suite smp --kernel /path/to/Image --host-cpus 8
cargo xtask smoke --suite smp-irq --kernel /path/to/Image --timeout-secs 60
cargo xtask smoke --suite smp-irq --kernel /path/to/Image --aia-host --features aia-guest-file --timeout-secs 60
cargo xtask smoke --suite instances --kernel /path/to/Image
cargo xtask smoke --suite provision --kernel /path/to/Image --timeout-secs 60
cargo xtask smoke --suite volumes --kernel /path/to/Image --timeout-secs 120
```

Add `--dry-run` to validate the inputs and print the prepared command without
launching QEMU. The CLI does not download a kernel or overwrite a root disk.
The [Linux IRQ routing gate](docs/vm-config.md#linux-smp-irq-routing-gate)
checks delivery to both vCPUs and repeats it after a cold restart.
For a one-hart scheduling comparison without Linux, use the existing
[firmware smoke](docs/firmware-guests.md#shared-hart-scheduling-gate): the same
periodic VM runs beside an idle and then a CPU-bound peer.
For interrupt delivery, [the firmware IRQ gates](docs/firmware-guests.md#interrupt-only-uart-service)
use a timerless UART guest on either the management hart or a separate worker.
To measure guest SBI calls directly, use the [two-vCPU mailbox benchmark](docs/mailbox-bench.md).
For guest IPI delivery versus return to the sender, use the [handoff comparison](docs/ipi-handoff.md).
`provision --domains 70` exercises a high VM ID (about 5 GiB of host RAM);
the default five-domain gate keeps its original memory budget.
`run` is resident and interactive; `smoke` is bounded and noninteractive.

## Documentation

- [Architecture, ownership and isolation](docs/architecture.md)
- [Port a board in five steps](docs/porting-board.md)
- [Port an instruction-set backend](docs/porting-arch.md)
- [VM configuration and commands](docs/vm-config.md)
- [Bring up a Linux management guest](docs/management-linux.md)
