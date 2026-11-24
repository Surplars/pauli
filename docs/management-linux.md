# Bring up a Linux management guest

OpenSBI starts the HS hypervisor; HS starts the initial Linux VM from the
manifest. Linux does not have to load itself. It manages other VMs through
the optional scalar SBI extension, not by owning HS or mounting a host FS.
This profile is an integration starting point, not a prebuilt management OS.
On [Valence Wave8 OoO](wave8-external-guest.md), the same HS firmware can
start ordinary Linux or a management Linux with a separate firmware service.
External Linux owns the human-facing serial console. See the
[driver, paulictl and OS handoff](../integrations/linux/README.md) for this
board's boot grants and end-to-end acceptance steps.
Neither role exposes nested H: a kernel built with KVM can boot, but
`/dev/kvm` is not the interface for managing Pauli VMs.

## QEMU bring-up

From the standalone workspace, use a local RV64 Linux Image and a shared,
**non-secret** test initramfs with a working `/init`:

```sh
cargo xtask run --vm configs/vm/manager.toml --kernel /path/to/Image --initrd /path/to/public.cpio --host-cpus 8 --host-memory-mib 512 --features guest-control
```

`--dry-run` prepares the same manifest and ELF without launching QEMU. The
resident profile has no disk, networking or default grants. The management VM
autostarts; the second entry is a stopped, diskless workload template. If a
management archive contains keys, privileged tools or local configuration,
set `initramfs` and `initramfs_sha256` on **manager only**; the CLI default is
also inherited by the workload template. No management secret belongs there.

```text
hv> vm grant manager linux inspect
hv> vm attach manager
```

Detach with Ctrl+] without stopping Linux. The Linux
kernel bridge probes vendor EID `0x08004f49` from VS mode. FID 0 takes
six zero words; success returns ABI version 1 in the upper 32 bits and the VM
ID ceiling below (not a list of accessible VMs). For this profile FID 1 with
`[1,0,2,0,0,0]` inspects template ID 2. Confirm the version and successful SBI
return **before** decoding values. Linux userspace ecalls are Linux syscalls,
not this interface. For the QEMU profile, address its template by numeric ID
(`paulictl status 2`); host grants and the framed native terminal are unchanged.

For the second milestone, give the current manager a budget and allow it to
create shaped children. A new grant replaces the old rights on the template:

```text
hv> vm quota manager 3 6 384
hv> vm grant manager linux inspect,create,shape
```

The quota is a ceiling, not reserved RAM. Use the returned child ID and
epoch for start/stop/destroy. A successful stop
is a request, not proof of joined I/O; inspect and handle a busy destroy.
Cold-restarting the manager invalidates its grants. A native shutdown rejects
queued guest creates/starts too. The hypervisor continues to own CPU, memory,
interrupts and final retirement. See [the scalar ABI and permissions](vm-config.md).
