# VM configuration and commands

Run from the independent workspace. A profile is TOML with one to four
`[[vm]]` entries. `cargo xtask run` boots the HS monitor and exposes a
resident terminal. It does not download or install a distribution and
does not accept ISO/UEFI images. Pass a raw RV64 Linux `Image`, an initramfs
when required, and an explicit raw disk if the Linux profile asks for one.
For static bare/RTOS ELFs, see [the diskless firmware ABI](firmware-guests.md).

```sh
cargo xtask run --vm configs/vm/dual.toml --host-cpus 2 --kernel /path/to/Image --initrd /path/to/initramfs.cpio
```

Use `--dry-run` to compile, validate and stage the launch without starting
QEMU. The printed paths for staged files are temporary; the launcher
removes them on return. `PAULI_QEMU` (or `OPENION_QEMU`) can select a QEMU executable.

## Profile fields

| Key | Meaning |
| --- | --- |
| `name` | Required unique ASCII identifier, starts with a letter, up to 24 characters |
| `memory_mib`, `guest_base` | Linux: private RAM, 64-1024 MiB in multiples of 2 MiB; aligned GPA base |
| `firmware`, `firmware_sha256`, `memory_kib` | Static RV64 VS ELF and checked digest; page-aligned private firmware RAM. Alternative to Linux `kernel` and `initramfs` |
| `vcpus`, `quantum_us` | 1-16 vCPUs, 100-10000 us slice; default 1 and 2000 |
| `cpu_affinity` | `automatic` (default), `workers`, or one physical owner index per vCPU |
| `autostart`, `reset` | Default manual start; `reload` (default) cold-restarts on guest reset, `stop` returns to monitor |
| `interrupts` | `plic` by default, or `aia`; the latter requires `--aia-host` in QEMU |
| `bootargs` | Linux command line; console default is `ttyS0,115200` |
| `kernel`, `kernel_sha256` | Override the shared `--kernel` source with a checked path/hash |
| `initramfs`, `initramfs_sha256` | Override `--initrd` with a checked path/hash |
| `disk`, `disk_image`, `disk_sha256` | `none` (default), `ro`, or `rw`; use a checked per-VM raw file or `--disk` |
| `net_peer`, `net_uplink` | Reciprocal, isolated pair by names, or one exclusive native NIC, never both |

Relative image paths in a profile resolve relative to that TOML file.
Each file specified in TOML requires a SHA-256; CLI paths are explicit
local inputs. A disk hash describes the input, not future writable state.
The launcher checks file identity; writable aliases are rejected even if
paths differ. Guest `rw` writes go to a disposable QEMU overlay unless
you explicitly select `--persistent`. Use a dedicated raw disk for a
persistent installation and shut down normally. There is no native VFS
or image-format autodetection in the hypervisor.

The boot manifest currently admits up to four VMs, each with its own 1-16
vCPUs, on 1-64 physical harts on QEMU virt. For example, a 1-vCPU VM and a 3-vCPU VM
can share one hart: all four vCPUs can make progress but only one can run
at a time. The profile also supplies Linux's CPU topology. Starting or
stopping an existing guest CPU does not add or remove a CPU from that
topology. There is no live vCPU hotplug. The four-record boot manifest does
not limit runtime VM IDs. The board computes an ID ceiling at boot by fitting
the smallest cloneable guest (RAM, private tables, VM state and alignment) into free
physical extents. The 48 boot reservation records are for firmware, boot guests
and DMA only; they do not bound runtime VM count. Host-only ID pages grow on
first use and are reused. Larger guests, fragmentation or index metadata can
exhaust RAM below the advertised ceiling. `vm capacity` is not a free-slot count.

Repeated CPU owner indices intentionally overcommit a physical CPU. The
default `--host-cpus` is 1; use 2 or more for a `workers` profile. `--aia-host`
enables AIA host support; `--features aia,aia-guest-file` additionally
requests the hardware guest-file path. The optional uplink opens QEMU user
networking only when the profile explicitly requests `net_uplink = true`.
Paired networking does not create a TAP, bridge, gateway or port forward.

## Host monitor

```text
help
vm list
vm capacity
vm disks
vm quota <manager> [<max-children> <total-vcpus> <total-MiB>|clear]
vm status [name]
vm create <boot-template> <child-name> [--vcpus N] [--memory-mib N] [--disk N]
vm destroy <child-name>
vm start <name> [--attach]
vm attach <name>
vm detach
vm stop <name>
vm grant <source> <target> <inspect,start,stop,console,create,shape,diskN,destroy|all>
vm revoke <source> <target>
shutdown
```

Use the VM name or one-based slot ID. When attached, press **Ctrl+]** to
return to `hv>` without stopping the VM. `vm stop` is asynchronous: wait
for `Stopped` or `Faulted`, with no I/O-join error, before cold start.
`shutdown` closes admission and waits for CPU/TLB and I/O joins before
native poweroff. Guest control calls require the `guest-control` build
feature and permission bound to the calling VM's current epoch.

## Create a VM after boot

With `configs/vm/linux.toml`, supply a raw Linux Image and a bootable initramfs.
Add `--host-memory-mib 512` to the run command to leave unassigned board RAM;
the launcher rejects a size smaller than the staged boot layout.

```text
hv> vm create linux work --vcpus 2 --memory-mib 128
hv> vm start work --attach
     (Ctrl+] to detach)
hv> vm stop work
hv> vm status work
hv> vm destroy work
```

Wait for `Stopped` or `Faulted` with no join error before destroy. `Busy` makes
no changes: retry after owners and transient readers have left. Omit either
option to inherit that field; both options are create-time, not live hotplug.
Choose 1-16 vCPUs and 64-1024 MiB in 2-MiB steps, large enough for the loaded
kernel and initrd. An explicit physical affinity list can shrink but cannot grow
into unspecified pins; automatic/worker placement supports growth and overcommit.
Both the boot path and runtime provisioning reserve private context and mailbox
storage for the requested count, not sixteen slots for every VM. In manifest v15,
legacy pin lists retain their byte encoding; long lists using owners 4-63 use
a distinct tag and six bits per owner. Old decoders reject that encoding.
A child uses immutable Image/initrd, never its parent's live memory. A changed
shape gets its own DTB with the new memory, CPU/IRQ topology and initrd addresses;
cold boot restores that same tree. Custom DTBs can be cloned unchanged but are
not silently rewritten. PLIC/AIA choice and boot arguments still come from the template.
Only diskless, network-free boot templates are accepted. A Linux child may
receive one explicit volume. A [firmware child](firmware-guests.md#clone-a-running-service)
retains its template's single vCPU, RAM size and immutable DTB; it cannot
attach disks. Neither kind inherits live guest bytes, and boot VMs cannot be destroyed.
Names are 1-16 ASCII letters, digits, `-` or `_` and
start with a letter. Slot or physical RAM exhaustion rejects the request.

To offer a volume, add this top-level entry to the TOML profile:

```toml
[[managed_disk]]
image = "work.raw"
sha256 = "<64 hex digits of the input file>"
mode = "rw" # or "ro"; a guest cannot upgrade it
```

`vm disks` lists zero-based slots (boot disks first, then declared volumes; four
total). For disk0, `vm grant manager linux create,disk0` allows that management
epoch to clone template `linux` with disk0, not disk1. Host equivalent:
`vm create linux work --disk 0`. Neither a boot disk nor a host path is assignable.
Stop, fault and cold restart keep the lease. Destroy releases it only after CPU
retirement, all writes and the final native flush. An unconfirmed mutation or
failed final flush quarantines that volume. Reassignment preserves its data;
provide a clean volume when changing tenants. Writable files use disposable
overlays unless you request `--persistent`; update the hash after persistent writes.

For a management guest, build with `--features guest-control` and explicitly
grant e.g. `vm grant manager linux inspect,create,shape`. Plain `create` only
permits creation without overrides; even an explicit equal value requires
`create,shape`. Legacy `all` grants neither right. Only give
`shape` to a trusted provisioner. For example, `vm quota manager 3 6 384`
caps its direct children at three allocated VMs, six vCPUs total and 384 MiB
total. Only the native monitor sets/clears limits; no explicit quota means
board capacity only. RAM charges sum the children's actual bytes, so two
512 KiB guests use exactly 1 MiB. Stopped children still count. A cold restart revokes
old credentials, not resource charges. A sponsor can be destroyed only after
all its children have actually been destroyed. The creator receives inspect/start/stop/console and
destroy rights on its new child, not create rights on other templates.
The scalar SBI management ABI (EID `0x08004f49`, FID 1) keeps version 1:
CREATE takes `[1,3,template_id,template_epoch,name_lo,name_hi]`; the name
is little-endian ASCII with zero padding and success returns `id<<32 | epoch`.
DESTROY takes `[1,4,id,epoch,0,0]`. Refresh the epoch after a cold restart.
Shaped CREATE is `[1,5,template_id,packed,name_lo,name_hi]`, with
`packed = epoch | (vcpus << 32) | (memory_mib << 40)`. The top byte is
reserved zero. Zero inherits a field; at least one override is required.
INSPECT reports the vCPU count; `[1,6,id,epoch,0,0]` returns exact RAM bytes
with the same inspect right. Disk CREATE uses opcode 7 and the same packed word
with `((slot+1) << 56)`; CPU/RAM overrides remain optional and require `shape`.
All CREATE variants return the same ID/epoch pair. Old opcodes reject the new bits.
The host monitor and guest transport use the same authorization/retirement path.
FID 0 with six zero arguments returns `version<<32 | highest_addressable_id`.
It neither enumerates peers nor promises free capacity. Management accepts
16 simultaneous scalar calls across all vCPUs. Its first 64 grants are inline;
on boards with a domain pool, further grant pages grow from host RAM and revoked
entries are reused. A failed grant allocation never creates an ownerless child.

## Local smoke gate

The `hv` suite exercises native preemption and a G-stage permission fault
in a bare test guest. Linux suites build a small PID1 and archive locally;
no userspace SDK, Debian disk or download is used.

| Suite | Required Linux capabilities / evidence |
| --- | --- |
| `hv` | No Image; entry/resume, native timer preemption and RX isolation |
| `linux` | RV64GC, SBI, initrd, serial console; VU PID1, TIME and F/D across sleep, local shutdown |
| `smp` | Also SMP and SBI HSM; run on both vCPUs. Optional `--host-cpus N` pins them to 0 and N-1 and verifies both owners; `provision` also accepts this option for automatic placement. |
| `smp-irq` | Two guest vCPUs on two physical owners; built-in Goldfish RTC, RTC character device, procfs, devtmpfs, writable IRQ affinity and effective affinity reporting. No disk or network. |
| `provision` | SMP (Linux must support at least 8 CPUs), sysfs and raw `/sys/firmware/fdt`: 1-CPU/96-MiB parent, three stopped reservations, 8-CPU/128-MiB child at ID 5 sharing two physical harts; verify affinity and getcpu on every vCPU, clean shutdown, cold restart, slot reuse and parent continuity |
| `instances` | Also devtmpfs and working `/dev/mem` mmap of the deliberately unmapped test GPA; peer survives fault, stop and cold start |
| `net` | Built-in VirtIO MMIO/net and IPv4; two guests exchange peer-checked datagrams over an isolated link |
| `storage` | Built-in VirtIO MMIO/block and the instances prerequisites; two new small raw disks, direct read/write, flush, cold restart and backing-file evidence |
| `volumes` | Same Linux capabilities as storage; dynamically give two children different temporary disks, check isolation, fault, restart, refused concurrent ownership, durable handoff and host readback |

For a larger provisioning gate, add `--domains N` (5 by default; the **test**
accepts 5..128). Only the parent and highest-ID child boot. The intervening
domains hold private RAM while stopped; the gate checks all IDs, the allocated
count, child restart/reuse and return to one live allocation. For example,
`--domains 70 --timeout-secs 180` needs about 5 GiB of QEMU RAM. Use
`--dry-run` to see the exact RAM requirement without launching QEMU.

An instance-fault test kernel must allow the test `/dev/mem` mapping
(`CONFIG_DEVMEM` and a compatible strict-devmem policy). It does not map
or probe a host physical address. A smoke timeout or missing evidence is
a failure, not a boot success. The default QEMU wall budget is 40 seconds
(`--timeout-secs` changes it); serial capture is capped at 1 MiB. An
independent watchdog holds this smoke's child handle and kills and reaps
it at the deadline even if the serial controller is blocked. Resident
`run` sessions have no watchdog.

### Linux SMP IRQ routing gate

From the standalone workspace, use a local raw Linux Image. These commands
use one guest, two harts, no disk and no network. Each 60-second limit covers
both the initial boot and one cold restart in a single QEMU process.

```powershell
$image = '../rtos/target/openion-linux/Image-6.11.9'
cargo xtask smoke --suite smp-irq --kernel $image --timeout-secs 60
cargo xtask smoke --suite smp-irq --kernel $image --aia-host --features aia-guest-file --timeout-secs 60
```

PID1 finds the virtual Goldfish RTC's Linux IRQ number. It selects CPU1,
checks effective affinity, sets a one-shot alarm and requires a real RTC
alarm event and exactly one additional RTC interrupt on CPU1, none on CPU0.
It then repeats on CPU0, with no extra RTC interrupt between the two alarms.
A UART message or a rising timer/IPI count cannot satisfy the test. This
establishes which CPU handled the device IRQ, not what woke a native hart
from WFI. Each cold boot supplies a fresh IRQ history and VM epoch.

Linux changes affinity here **before** arming each alarm. It does not move
an in-flight interrupt. Separate offline tests change the route while the
RTC level is high: a latched, unclaimed PLIC request can change its eligible
context; an APLIC MSI already forwarded to an IMSIC stays in that file.
Changing its target alone must not issue a second MSI.

For native IMSIC, the joined host status must show that both vCPUs actually
used a native guest file during that epoch, with all file loads matched by
puts and none still bound. The counter is per vCPU/epoch, not per RTC MSI:
it rules out a wholly software fallback, not other software involvement in
delivery. Omit `--features aia-guest-file` while retaining `--aia-host`
to check the software AIA configuration instead. A dry run validates neither
IRQ delivery nor native guest file use.

#### Measured on QEMU virt, Linux 6.11.9

Two diskless, network-disabled runs used the same Image (SHA-256
`174f8bb87f08961e54fa3fcd954a8e31f4645f6d6af4dd43983d5e9841490fb0`).
Each had two harts, 288 MiB and one 60 s process deadline for both boots.
Both reported `smoke smp-irq: PASS` and exited normally without a retry.

| Controller | VM epoch | Linux RTC IRQ | Counts [CPU0, CPU1]: before / after CPU1 alarm / after CPU0 alarm | IMSIC loads / puts |
| --- | --- | --- | --- | --- |
| PLIC | 1 and 6 | 13 | [0,0] / [0,1] / [1,1] on each boot | n/a |
| Native IMSIC | 1 | 12 | [0,0] / [0,1] / [1,1] | 3223 / 3223 |
| Native IMSIC | 6 | 12 | [0,0] / [0,1] / [1,1] | 3398 / 3398 |

Both vCPUs retired on distinct owners (0 and 1). On both IMSIC boots,
`loaded_vcpus=0x3` and `bound=0x0`: each vCPU used a guest file and neither
kept it after shutdown. The two epochs recorded 3575 and 3625 pending-file
merge attempts, but only 25 and 20 pending ID bits respectively. These counts
include empty attempts and cannot attribute individual bits to the RTC or
measure their cost. This is QEMU routing evidence, not a hardware latency test.

The resident native path now opens its CSR window only if the captured inbox
has bits to inject. Previously even an empty batch selected the guest file
and restored the original CSRs: five CSR instructions with no EIP write.
Of the 7,200 merge attempts above, at least 7,155 were empty (there were only
45 injected bits), so the new
path would avoid at least 35,775 CSR instructions on that trace. This is an
instruction count, not a latency or throughput measurement. Load, final
save/clear and late-injection handling are unchanged.

One native IMSIC rerun with the optimization passed the same two-boot RTC gate:
epoch 1 reported 3,595 loads/puts, 3,930 merges and 25 injected bits; epoch 6
reported 992 loads/puts, 1,293 merges and 19 bits. Both had `loaded_vcpus=0x3`
and `bound=0x0` at join. The number of attempts varies between boots; it is
not a speed comparison.

To reproduce the host-side operation counts without QEMU:

```powershell
cargo test --offline --locked --release -p pauli-arch-riscv --features selftest,aia-guest-file --lib sample_imsic_cost -- --ignored --nocapture --test-threads=1
```

For 262,144 merges the window now opens 0 times for an empty inbox, 2,048
times for one injection every 128 merges, and 262,144 times for continuous
injection. Two bits in different EIP words still require two sets but share
one window. Host timing includes the bitmap check, not the physical CSRs;
it is not a guest interrupt-latency benchmark.
