# Wave8 OoO: handoff to the OS packager

The **same HV binary** boots the embedded probe with the stock native DTB.
With an external Image, `/chosen/openion,guest-role` selects ordinary
Linux (`"guest"`, also the default) or a Linux management VM (`"manager"`).
Both run in **VS** under Pauli in **HS**; this is not Linux hosting KVM.
The Linux build need not disable KVM, but no nested H extension is exposed.
The packager owns the flat boot image, loader metadata and physical upload.
Use an OoO bitstream that implements `senvcfg` in HS. The r1 paths below
identify the loader layout and Linux assets, not a compatible FPGA bitstream.
The addresses below are RISC-V addresses, **not** PS-side DDR aliases.

## Build and stage in a separate package

1. From the `pauli` repository root, build the OpenSBI next-stage payload:
   `cargo xtask image --board valence --output target/wave8-hv.bin`.
   Choose a fresh output name. Put this **flat binary, not its ELF**, in
   the old Linux payload slot at HPA `0x80200000`. Keep the whole range
   `[0x80200000,0x81000000)` for HS (file, BSS, stack, host arenas).
   Keep preboot, the native OpenSBI FW_DYNAMIC stage at `0x80100000`,
   hart 0 and the native DTB handoff at `0x8fe00000`.
2. Stage the **uncompressed RV64 Linux Image** at HPA `0x89000000`,
   up to 64 MiB. For `wave8-cmu-20261123-r1/Image` the actual file is
   **7,282,696 bytes** (`0x006f2008`, SHA-256
   `7ed74c5171aacddc6cc9d0b50bf9f74959e7a2fb56dafdaa73aad69c67b70ef1`).
   Its little-endian Image header specifies a 2 MiB text offset and
   **7,605,976 bytes** of runtime image, including BSS. Supply the **file**
   length, not the runtime length, to `openion,guest-image`. No ELF, ISO
   or compressed kernel decoder is involved.
3. Keep the optional initrd at HPA `0x81000000`, ending no later than
   `0x89000000`. The r1 BusyBox package already places its LZ4 initramfs at
   `[0x81000000,0x884a970b)` (122,328,843 bytes); no second source copy
   is needed. Sources remain immutable across VM stops/cold starts.
4. Append [the native DTS fragment](../configs/boards/valence-linux.dtsi)
   to **ooo-busybox.dts**, not the in-order tree. In the source package's DT
   bundle replace the OoO slot with this compiled tree. From the `pauli` repository root,
   with `WAVE8` naming the original release and `OUT` a separate OS staging
   directory, a local `dtc` can generate it without touching the originals:

   ```sh
   cat "$WAVE8/ooo-busybox.dts" configs/boards/valence-linux.dtsi |
     dtc -I dts -O dtb -p 4096 -o "$OUT/ooo-hv-linux.dtb" -
   ```

   For the management domain, append [one further fragment](../configs/boards/valence-manager.dtsi);
   this replaces **only** the role. Use this tree *instead of* the guest tree
   in the OoO slot:

   ```sh
   cat "$WAVE8/ooo-busybox.dts" configs/boards/valence-linux.dtsi \
       configs/boards/valence-manager.dtsi |
     dtc -I dts -O dtb -p 4096 -o "$OUT/ooo-hv-manager.dtb" -
   ```

   In the native DTB, `openion,guest-image` is **two big-endian u64s**
   (source, byte count). `linux,initrd-start` and `linux,initrd-end` are
   each a big-endian u64 (end exclusive); supply both or neither.
   `openion,guest-bootargs` is one NUL-terminated UTF-8 string, up to
   2,048 bytes before the NUL; the default is `console=ttyS0,115200 rdinit=/init`.
   Native `chosen/bootargs` is not inherited. The unique
   `compatible = "openion,wave8-guest-ram"` child of `/reserved-memory`
   has empty `no-map` and four big-endian 32-bit `reg` cells. They grant
   the entire `[0xa0000000,0xc0000000)` (512 MiB) **to the HV exclusively**
   for guest RAM: no loader, firmware, RPU or DMA user may retain or
   overwrite it. Both roles accept this legacy grant. The optional larger grants are described below.
5. Rebuild the package's DT bundle offsets, flat layout/size, checksums,
   and any RPU/JTAG CRC or image-length metadata for the changed bytes.
   With flat RV base `0x80000000`, the HV, archive and kernel source are
   at offsets `0x00200000`, `0x01000000`, `0x09000000`, respectively.
   The old CRC helper and original flat image cannot be used unmodified.
   Keep the distinct preboot/OpenSBI, DT handoff and RPU carveouts intact.
   The stock preboot itself requires a valid `chosen/linux,initrd-end`
   before it calls OpenSBI. For a truly archive-free package adapt preboot
   too, and supply a Linux image with a built-in init; the HV does not
   require an initrd or use the Debian rootfs as its own filesystem.

Before packaging an r1 profile, verify its generated native DTB and the
real Image header inside the Pauli workspace (no target or QEMU).
Set `NATIVE_DTB` to the chosen output, e.g. `"$OUT/ooo-hv-manager.dtb"`:

```sh
WAVE8_BUNDLE="$WAVE8" WAVE8_EXTERNAL_DTB="$NATIVE_DTB" \
  cargo test --offline --locked -p valence packaged_external_guest_plan -- --ignored --nocapture
```

## Optional guest interrupt controller

With the default Valence build, omitting `openion,guest-irq` or setting it to
`"plic"` retains the original Linux interrupt topology. For a Linux kernel with APLIC and IMSIC
drivers, append the [AIA fragment](../configs/boards/valence-aia.dtsi) to the
external Linux or manager DTS. The choice applies to Linux. The manager's
firmware service continues to receive a virtual PLIC.

On Wave9 A2 the supervisor IMSIC at `0x28000000` has 127 interrupt IDs.
Two guest-index bits cover four 4 KiB pages for this hart, including the HS
file at index zero. The guest-file ID count defaults to the supervisor count
when the native DT omits `riscv,num-guest-ids`. Pauli uses the same virtual
ID range for the Linux DTB, software MSI delivery and CSR emulation. The
Wave9 default is 1–127. An explicit 255-ID guest file selects IDs 1–255.

`aia-guest-file` enables owner-local caching in physical guest file 1 and
is included in Valence's default build. The HS banner's `aia_file=true`
reports that file 1 was admitted for the selected AIA guest. PLIC and
embedded boots leave this physical file untouched. The guest MSI page remains
virtual. A software-only build is available with
`cargo xtask build --board valence --no-default-features --features aia`.
An explicit AIA topology requires the `aia` build feature. Add
`guest-control` when building a management image.

## Optional 1 GiB Linux RAM grant

Append the [1 GiB RAM fragment](../configs/boards/valence-1g-ram.dtsi)
after `valence-linux.dtsi` to replace its legacy reservation. The example
starts at HPA `0x1_0000_0000`, above the Wave9 `rpu@d0000000` carveout.
The complete grant must fit inside one available DDR bank and be disjoint
from the handoff, image sources and other reservations.

| Role | Native grant size | Backing at the example HPA |
| --- | --- | --- |
| Ordinary Linux | `0x40000000` (1,024 MiB) | Linux uses the entire grant |
| Manager | `0x40200000` (1,026 MiB) | Linux uses the first GiB. Service uses the first 64 KiB of the remaining 2 MiB |

For a manager append `valence-manager.dtsi` and set the final size cell in
the RAM fragment to `0x40200000`. Pauli rejects an exact 1 GiB manager grant.
This leaves Linux with the same aligned guest interval
`[0x80000000,0xc0000000)` for either role and permits a single Sv39x4
1 GiB RAM leaf. Any alternate HPA must be at least `0xc0000000`, aligned
to 1 GiB and available exclusively to Pauli.

Inspect the effective OpenSBI domain/PMP regions for the boot hart before
using these grants. HS needs access to the complete Linux range and, for a
manager, the service range starting at the next GiB boundary. For example,
separate naturally aligned regions of orders 30 and 21 describe the 1 GiB
region and its 2 MiB tail. Check permissions and higher-priority overlapping
regions against firmware and RPU reservations. The native `no-map` grant
declares ownership to Pauli. OpenSBI programs the physical permissions.
[OpenSBI domain memory regions](https://github.com/riscv-software-src/opensbi/blob/master/docs/domain_support.md)

## Optional pool for service children

For a headless manager that needs to create and destroy firmware guests,
append `valence-domain-pool.dtsi` after `valence-linux.dtsi` and
`valence-manager.dtsi`. With the example fragment the HS banner reports
`pool=8192K`. Omitting the fragment keeps the two fixed VMs and reports
`pool=none`. [Domain pool fragment](../configs/boards/valence-domain-pool.dtsi)

The fragment grants HS the exclusive physical interval
`[0xc0000000,0xc0800000)`, immediately after the legacy Linux/service
reservation and below Wave9's RPU carveout. The native tree must contain
one available `openion,wave8-domain-pool` child of `/reserved-memory`
with a single `reg` tuple, an empty `no-map` property and no `reusable`
property. Pool base and size are multiples of 2 MiB. The base must be at
least `0x80000000`, size must be 2 MiB through 1 GiB, and the exclusive
end must fit within the 56-bit HPA limit. The complete interval must fit
in a single available DDR bank and be disjoint from firmware, HS, the
handoff, Image, initrd, boot guests and other reservations. Verify that
OpenSBI permits HS writes throughout and that loader and DMA users keep
the range free. A pool declaration requires the external manager role.

The manager receives `CREATE` on its fixed `service` template. Each child
inherits one vCPU, 64 KiB of private guest RAM and the service firmware.
The sponsor quota caps allocated children at **8 guests, 8 vCPUs and
512 KiB of aggregate guest RAM**. A stopped child still counts until
destroyed. Host page tables, vCPU state and registry metadata also consume
physical pool space, so available memory can limit creation earlier.
The manager receives neither `SHAPE` nor any `diskN` grant.

Run `paulictl create service worker` in management Linux. Use the returned
VM ID and epoch with `paulictl start ID --epoch E`, then
`paulictl stop ID --epoch E --wait` and `paulictl destroy ID --epoch E`.
Destroy releases the child's allocation and its quota charge.

## Optional Ethernet link to the fixed service

For a manager and a network-capable firmware service, append
`valence-peer-net.dtsi` after `valence-linux.dtsi` and
`valence-manager.dtsi`. The native `/chosen/openion,peer-net` property is a
single empty boolean. It requires an external manager and excludes every
`openion,wave8-domain-pool` declaration, including a disabled one. A fixed
peer port belongs to its boot slot and cannot be cloned.
[Peer-net fragment](../configs/boards/valence-peer-net.dtsi)

Each guest gets a VirtIO-MMIO Ethernet port at GPA `0x10002000`; frames
travel between manager and service through HS-owned buffers. The board
retains its GEM controller. The Wave9 Pauli A/B delivery kernel has
`CONFIG_VIRTIO_MENU` disabled. To bind the guest port in Linux, enable
`CONFIG_VIRTIO_MENU`, `CONFIG_VIRTIO_MMIO` and `CONFIG_VIRTIO_NET`.
Service firmware must post receive buffers in its writable RAM. The bundled
heartbeat firmware only uses UART.
[Transport contract and QEMU measurements](inter-vm-transport.md)

## What Linux receives

The hypervisor copies private VM RAM from validated sources and **generates**
the guest DTB. Do not pass the native board DTB, native DDR aliases or the
native initrd addresses to Linux. For the above r1 images:

| Guest-visible item (GPA) | Ordinary Linux | Management Linux |
| --- | --- | --- |
| Private Linux RAM | `[0x80000000,0xa0000000)` (512 MiB) | `[0x80000000,0x9fe00000)` (510 MiB) |
| Linux entry (one VS hart, `a0=0`) | `0x80200000` | `0x80200000` |
| Initrd in the guest DTB | `[0x98800000,0x9fca970b)` | `[0x98600000,0x9faa970b)` |
| Generated guest DTB supplied in `a1` | `0x9fe00000` | `0x9fc00000` |

In both cases the Linux entry is backed by HPA `0xa0200000`, **not**
the HS payload at HPA `0x80200000`. In manager mode the top 2 MiB of the
physical grant is excluded from Linux. The separate integer `service` VM
gets only its first 64 KiB (`[0xbfe00000,0xbfe10000)`); the rest is
unassigned. The two VMs do not share RAM, the Linux initrd, or an address
space. They time-share the one native hart.

The initrd GPA is recomputed for other archive sizes. With the default interrupt topology Linux receives a
virtual PLIC and 16550 (`ttyS0`, 115200); the **physical** terminal remains
**250000 8N1**, no flow control. This port has no native block or network
backend and does not pass through GEM, RPU, storage or physical IMSIC files.
Use initramfs userspace, not `root=/dev/vda1` or a network root. A stock
kernel may need different drivers/init scripts when moving from the
physical board to the virtual one. An offline Image/DTB validation does not
establish that PID1 or a serial login works on the physical board.

## Linux console and authorized service

After the HS banner the external-Linux profile forwards the first Linux VM's
console as plain serial bytes, without a host prompt, framing or an escape
key. The embedded bring-up profile retains the native debug monitor.
Ordinary Linux has no cross-VM rights. Manager mode boots `manager`,
leaves `service` stopped and grants inspect, start, stop, log and mail on that fixed
peer. When both guests are ready the service receives only mail on the
manager. Log and mailbox access bind both incarnations. On service cold restart, the
board renews the service's outgoing mail grant when the manager is ready.
The manager receives its own boot grant when the manager enters Ready.
The optional pool adds permission to clone the fixed service.

Use the [Linux control kit](../integrations/linux/README.md) from the
management VM. Its kernel bridge calls the [scalar SBI ABI](management-linux.md),
not the serial console. Read the service's latest 4096 UART bytes with
`paulictl logs service`. Its output stays off the physical UART unless Linux
explicitly prints it. The same command works after a service stop or fault,
up to the next cold restart.

A service payload that implements scalar operations 9 and 10 can exchange
tagged records of up to six bytes with the manager. Use `paulictl send service`
and `paulictl recv service` on Linux. In the service, peer ID 1 and epoch zero
use its mail-only grant. The supplied heartbeat firmware continues to use UART.
