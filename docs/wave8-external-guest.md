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
   `cargo xtask image --board valence-wave8-ooo --output target/wave8-hv.bin`.
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
4. Append [the native DTS fragment](../configs/boards/valence-wave8-ooo-linux.dtsi)
   to **ooo-busybox.dts**, not the in-order tree. In the source package's DT
   bundle replace the OoO slot with this compiled tree. From the `pauli` repository root,
   with `WAVE8` naming the original release and `OUT` a separate OS staging
   directory, a local `dtc` can generate it without touching the originals:

   ```sh
   cat "$WAVE8/ooo-busybox.dts" configs/boards/valence-wave8-ooo-linux.dtsi |
     dtc -I dts -O dtb -p 4096 -o "$OUT/ooo-hv-linux.dtb" -
   ```

   For the management domain, append [one further fragment](../configs/boards/valence-wave8-ooo-manager.dtsi);
   this replaces **only** the role. Use this tree *instead of* the guest tree
   in the OoO slot:

   ```sh
   cat "$WAVE8/ooo-busybox.dts" configs/boards/valence-wave8-ooo-linux.dtsi \
       configs/boards/valence-wave8-ooo-manager.dtsi |
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
   overwrite it. Do not change this physical grant when selecting manager.
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
  cargo test --offline --locked -p valence-wave8-ooo packaged_external_guest_plan -- --ignored --nocapture
```

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

The initrd GPA is recomputed for other archive sizes. The guest receives a
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
leaves `service` stopped and grants only inspect/start/stop on that fixed
peer. The trusted board grants these rights to each new manager incarnation;
old requests and old grants do not survive a cold restart. There is no
dynamic VM pool or grant of physical devices.

Use the [Linux control kit](../integrations/linux/README.md) from the
management VM. Its kernel bridge calls the [scalar SBI ABI](management-linux.md),
not the serial console. A second VM's output stays off the Linux terminal.
