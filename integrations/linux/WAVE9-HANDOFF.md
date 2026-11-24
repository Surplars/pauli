# Wave9 headless control: OS handoff

This is an integration kit, not a replacement boot disk. Verify its
`SHA256SUMS` from the extracted kit directory. There are three parts:

| File in the kit | Where it goes |
| --- | --- |
| `firmware/valence-wave8-ooo-250k.bin` | HS payload slot, RV load/entry `0x80200000`, **outside** the rootfs |
| `rootfs/usr/sbin/paulictl` | Linux rootfs, executable (0755); statically linked RV64 |
| `rootfs/lib/modules/7.4.0-rc1-valence-smp-debian-r2+/extra/pauli.ko` | Only the currently shipped Wave9 Image; for a new kernel rebuild from `source/driver` or build in |

The ELF in `firmware/` is for symbols, not for the flat loader. The binary
was built with the Wave9 distribution's existing guest 250000-baud/Orbital
A2 overlay applied to the updated hypervisor. The original external boot
package was not modified. Recalculate loader lengths, bundle offsets and
checksums after updating both HS and initramfs. In particular **update
linux,initrd-end** to the actual packed file end and keep the archive below
the staged kernel. Do not simply overwrite a file in a checksummed package.
Select the native **manager** tree so VM 1 gets the limited grant on VM 2.
On this profile the physical UART displays the HS banner then the primary
Linux console (250000 8N1, no flow control). No Ctrl+] or host shell.

To test with the matching original kernel, load the module after devtmpfs
is mounted (from init for unattended boot). In the manager guest as root:

```sh
insmod "/lib/modules/$(uname -r)/extra/pauli.ko"  # not needed with built-in driver
/usr/sbin/paulictl info
/usr/sbin/paulictl status service
/usr/sbin/paulictl start service
/usr/sbin/paulictl status service
/usr/sbin/paulictl stop service
/usr/sbin/paulictl status service
```

Install kmod or enable the BusyBox insmod applet for a module-based image.
Start/stop target the last observed incarnation. Stop acknowledgement can
precede retirement; query until stopped. Booting the ordinary guest profile
should still give Linux login but never peer control rights. For the new
kernel build use `source/README.md` and `source/driver/README.md`; keep the
existing Valence board patches/config when updating its upstream base.
No userspace syscall, serial command, or KVM device substitutes for this driver.
