# Port a board in five steps

Keep physical addresses and board policy under `boards/`. Reuse `pauli-core`,
the ISA backend and native drivers; do not fork the run loop or change the
guest MMIO layout just because the physical layout differs.

1. **Freeze the firmware contract.** Record boot privilege, ABI and entry,
   physical RAM and firmware reservations, accessible ISA extensions and
   translation mode, hart/timebase topology, and required SBI capabilities.
   Check those against the actual firmware handoff. On the first board
   boot, use one hart and a UART; H/TIME are mandatory for the current
   RISC-V hypervisor. HS must be able to read and write `senvcfg`: it has
   no separate VS copy, so the world switch exchanges it on entry and exit.
   HSM and IPI are needed for native SMP.
2. **Define image and startup.** Add a package under `boards/` and a
   matching checked board profile under `configs/boards/`. Add the
   linker/startup contract, preserve OpenSBI arguments, initialize BSS and
   the native stack, then create static `BootStorage`. Publish the trap
   vector and IRQ routes before enabling interrupts. Check the linked RAM
   span including BSS and stack; inspect a tight board's boot stack frame.
3. **Assign resources.** Implement `Board::info`, `load_boot_config` and
   `take_resources`. Reserve firmware, payload, FDT and immutable sources
   before assigning pairwise-disjoint guest RAM and table arenas. Prove
   alignment, extent and ownership before creating references. Budget
   `Vm::<Arch>::vcpu_arena_bytes(config.vcpus)` for private CPU contexts
   in addition to the architecture's page tables and any device buffers. For a
   Linux payload pass the same native `timebase_hz` into the guest DTB
   generator. A trusted embedded payload may use the small RX-page path.
   To support managed creation, hand the residual physical allocator to
   `DomainPool::from_boot` only after all host and DMA reservations are final,
   and expose it with `Board::domain_pool`. Every full unreserved page must be
   writable and identity-mapped: the runtime stores its free-list headers there.
   Partially reserved pages never transfer. Otherwise leave that hook absent.
4. **Wire native I/O.** Use `DeviceResource` and HAL traits for UART, IRQ,
   RTC and optional block/net. Resolve disks by logical slot, not a guest
   MMIO address. Validate bus translation and DMA coherency, reserve
   private bounce memory, and keep native servicing bounded. Leave an
   absent device unsupported. Never infer F/D, guest IMSIC or available
   storage from H support. Set `Board::IRQ_DRIVEN_CONSOLE` only when RX can
   wake the boot hart. If the IRQ handler buffers bytes, `console_rx_queued`
   can report them without consuming input; it must be safe with local IRQs
   masked. The core retains a bounded poll even when the hint is absent.
5. **Verify in stages.** Start with host tests and a cross build. On the
   target validate UART and timebase, guest entry/exit and timer preemption,
   then fault isolation. Only after that test extra harts, I/O, flush,
   stop and cold restart. Keep a reproducible firmware revision and
   report actual logs separately from offline build results.

## Existing board contracts

| Board | Payload entry | Timebase | Native topology |
| --- | --- | --- | --- |
| QEMU virt | `0x80200000` | 10 MHz in the launch profile, checked against FDT | 1-64 harts; FDT PLIC or APLIC/IMSIC, UART, optional VirtIO/RTC |
| Valence Wave8 OoO | `0x80200000` | 100 MHz | Hart 0; S-APLIC/IMSIC and NS16550A, 14 MiB payload window |

QEMU reads the native FDT from OpenSBI. Optional `/chosen` cells
`openion,boot-config` and `openion,guest-dtbs` are big-endian 64-bit host
addresses; the standard launcher instead uses the offsets in
`pauli_config::loader` relative to physical RAM. The board must reserve the
whole template span and the exact nonoverlapping guest assignments.

### Valence Wave8 OoO firmware handoff

Target: Wave8 OoO RTL implementing `senvcfg` (CSR 0x10a), with the
`wave8-cmu-20261123-r1` loader and memory layout. Do not use the original
r1 bitstream that lacked this CSR, or the InOrder bitstream. The OS/integration
task owns the outer image, its CRCs and the loader. Build the standalone HS
payload from `hypervisor/`:

```sh
cargo xtask image --board valence-wave8-ooo
```

This builds the symbol-bearing `target/riscv64imac-unknown-none-elf/release/valence-wave8-ooo`
and writes `valence-wave8-ooo.bin` alongside it. Use the **flat `.bin`** as the
RV payload, not the ELF or a replacement for `fw_dynamic.bin`. An existing
`.bin` is never overwritten: use `--output <new-path>` to keep another build.

| Integration field | Contract |
| --- | --- |
| RV load **and** execution address | `0x80200000` (offset `0x00200000` in this revision's RV DDR flat image, **not** a PS/JTAG address) |
| Firmware ABI | Existing preboot -> OpenSBI `FW_DYNAMIC` at `0x80100000` -> S-mode, V=0, paging off; `a0=0`, `a1=` physical OoO DTB (delivery relocates it to `0x8fe00000`) |
| Memory ownership | Reserve `[0x80200000, 0x81000000)` for HS code/data, BSS, stack, tables and the embedded fixture. The flat file need not contain zero-fill regions. Optional external guest RAM is a separate explicit reservation. |
| Native console and clock | NS16550A `0x10000000`, 24 MHz reference, 250000 8N1, S-APLIC source 3. Use the **100 MHz** timebase, not the 125 MHz CPU or 165 MHz system clock. |

No host allocator claims the rest of DDR. Preboot, OpenSBI, the native DTB,
host image sources, RPU/GEM DMA and the physical interrupt files are never
mapped into a guest. Use the native S-file for host interrupts; no hardware
guest-file fast path is required. There is no physical disk or NIC backend.

Without an external guest declaration the embedded fixture ignores the native
initrd. For ordinary Linux or Linux as a management VM, see the [external guest ABI](wave8-external-guest.md):
the same HS binary copies the explicitly staged Image and optional initrd into
private guest RAM and supplies a **virtual** DTB. The stock **preboot** checks
`/chosen/linux,initrd-end` before OpenSBI; preserve its input contract, or
change preboot deliberately in the OS/image task.

To preflight the package's DTBs without the board, point `WAVE8_BUNDLE` at the
delivery directory and run `cargo test -p valence-wave8-ooo packaged_handoff -- --ignored`.
This runs the same validator used at entry; it does not execute any OpenSBI or HS code.

First board acceptance: capture the host banner, the embedded guest's
`Wave8 guest: BASE/TIME/WFI PASS`, and `vm status wave8` at `hv>` after the
guest's local shutdown. `vm start wave8 --attach` should repeat the message in
a new epoch. Use 8N1 with no flow control; a raw terminal may show monitor/guest
framing bytes. This exercises HS -> VS, guest TIME/WFI, UART and stop/restart;
it does not prove guest fault isolation, physical networking or Linux boot.
