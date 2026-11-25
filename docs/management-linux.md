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

In the manager guest, `paulictl list` shows the IDs it can inspect.
`paulictl create 2 worker --memory-mib 64` reports the child's ID and
initial epoch. Use that pair with `paulictl memory ID --epoch E` to read
its exact size in bytes, or with `paulictl start ID --epoch E` to boot it.
Use the epoch returned by start for `paulictl stop ID --epoch E --wait`.
After a terminal status, `paulictl destroy ID --epoch E` requests
retirement. If the backend is still busy, inspect again before deciding
whether to submit another destroy. The quota sets a ceiling on allocation.
For a disk-backed child the host must also grant the selected logical
slot before the manager passes `--disk 0` through `--disk 3` to create.

The creator receives `log` on the child's initial epoch. Run
`paulictl logs ID --epoch E > child-console.bin` during execution or after
stop or fault. On QEMU the host can grant read-only log access to another
peer with `vm grant manager PEER log` and give the manager its epoch. Include
`inspect` in the grant to allow automatic epoch discovery. The Valence
manager profile supplies both rights for the fixed service at manager boot.
Each VM retains its latest 4096 console bytes. A target cold restart clears
that history and requires fresh log authorization for the new epoch.

To exchange small records between guests, grant each caller `mail` on its
peer. On QEMU, a host grant replaces the existing rights, so include any
rights the application still needs. For example:

```text
hv> vm grant manager linux inspect,mail
hv> vm grant linux manager mail
```

With both guests ready, the manager can run `paulictl send 2 --tag 7 --hex 00ff`.
The Linux peer can run `paulictl recv 1 --epoch 0 --wait --timeout-ms 1000`
with its mail-only grant. This pops one message. A reply requires an
explicit send by the peer. Each VM can hold sixteen messages, up to eight
from any one sender incarnation. A stopped sender's queued messages remain
readable in its epoch. A receiver cold restart clears its inbox.

Cold-restarting the manager invalidates its grants. A native shutdown rejects
queued guest creates/starts too. The hypervisor continues to own CPU, memory,
interrupts and final retirement. See [the scalar ABI and permissions](vm-config.md).
