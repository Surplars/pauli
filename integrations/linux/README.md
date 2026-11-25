# Pauli Linux control kit (RV64)

The product console is for Linux text and keyboard input. Management uses an
independent path: `paulictl` -> ioctl on `/dev/pauli` -> guest kernel
SBI ecall (vendor EID `0x08004f49`) -> Pauli HS. There is no control
protocol on the serial port and no dependency on KVM, a network or a daemon.

## Install with the OS image

1. **Firmware/DT (not rootfs):** use the new HS payload and the native
   **manager** DT/profile. Rebuild the boot package and its checksums; use
   the matching OoO bitstream with `senvcfg`. The Wave9 distribution's
   250000-baud / Orbital A2 guest-layout overlay belongs on top of the
   updated HV sources when rebuilding its distribution-specific payload.
2. **Kernel:** for a new kernel build use the [driver](driver/) and
   [UAPI header](include/uapi/linux/pauli.h). For built-in integration
   copy the driver directory to `drivers/misc/pauli/`, install the
   header at `include/uapi/linux/pauli.h`, source its Kconfig from
   `drivers/misc/Kconfig` and add
   `obj-$(CONFIG_PAULI_HV) += pauli/` to `drivers/misc/Makefile`.
   Select `CONFIG_PAULI_HV=y`. With devtmpfs mounted, probing ABI v1
   registers the root-only device node. No module autoload is needed.
   For a separately built `.ko`, see the driver notes; it must match the
   **exact guest kernel build**, not the WSL host. Load it before using
   the tool. The userspace executable alone cannot issue SBI calls.
3. **Rootfs:** install the RV64 `paulictl` as `/usr/sbin/paulictl`, mode
   0755. To reproduce the standalone binary from this directory:

   ```sh
   make -C tool CROSS_COMPILE=riscv64-linux-gnu- LDFLAGS='-static -s'
   install -Dm755 ../../target/paulictl/paulictl "${ROOTFS:?set ROOTFS to the staging directory}/usr/sbin/paulictl"
   ```

## Acceptance on a management boot

Run as root inside Debian. There is no `hv>` login or serial escape to grant
rights. The native manager profile grants VM 1 inspect, start, stop, log and mail
access to its declared service VM (ID 2). Ordinary Linux has no cross-VM rights.

```sh
paulictl info
paulictl status service
paulictl start service
paulictl status service
paulictl stop service --wait
paulictl logs service > service-console.bin
```

`info` reports the ABI and the largest addressable ID, **not** a list of
accessible VMs. `service` is shorthand for public ID 2 on this board;
numeric IDs also work. Stop is asynchronous: if status is `stopping`, query
again until `stopped`. The service runs on its own virtual console; its
bytes remain available through `logs` after it stops. Redirect stdout to
preserve arbitrary binary output.

## Commands

```text
paulictl info
paulictl list
paulictl status ID
paulictl start ID [--epoch E]
paulictl stop ID [--epoch E] [--wait [--timeout-ms MS]]
paulictl memory ID [--epoch E]
paulictl logs ID [--epoch E] [--cursor N] [--follow [--timeout-ms MS]]
paulictl send ID [--epoch E] [--tag N] [--hex HEX | TEXT]
paulictl recv ID [--epoch E] [--wait [--timeout-ms MS]] [--raw]
paulictl create TEMPLATE NAME [--epoch E] [--vcpus N] [--memory-mib M] [--disk SLOT]
paulictl destroy ID [--epoch E]
```

Use a positive decimal public ID or `service` (2) for ID and TEMPLATE.
`list` prints the same rows as `status`, in ascending ID order. It skips IDs
for which INSPECT returns access denied or invalid target and reports other
errors. The rows are successive observations and may change during a scan.
`memory` uses the inspect grant and reports the exact byte count.

`logs` reads the most recent 4096 guest UART TX bytes of the selected epoch.
Reading starts at byte offset zero by default. Multiple readers can use
independent cursors. When the producer has overwritten the requested byte,
the tool reports the new starting offset on stderr and resumes there. Stdout
contains exactly the retained bytes. At the current end, a normal read exits
and `--follow` waits for more bytes. `--timeout-ms` bounds the entire follow
session. An epoch change ends the read.

Reading requires the separate `log` grant. A reader with only `log` must
supply `--epoch` because automatic epoch discovery uses `inspect`. A stopped
or faulted target keeps its log. Cold restart clears it and expires access
to that target's log. First activation uses the epoch reported at creation.

`send` queues one tagged message on a ready peer. The tag defaults to zero
and ranges from 0 to 255. Supply up to six text bytes, or use `--hex` with
up to twelve hex digits in pairs. An omitted payload sends a zero-byte
message. Success is silent. For example, `paulictl send service --tag 7 --hex 00ff`
queues two binary bytes. A full inbox or sender quota returns `EAGAIN`.

`recv` removes the oldest message from the named peer. A result such as
`vm=2 epoch=1 tag=7 len=2 more=0 hex=00ff` reports the tag and whether
more messages from that peer are queued. An empty inbox prints `empty=1`.
A present zero-byte message prints `len=0` and an empty `hex=` field.
`--raw` writes exactly the payload, so both cases produce zero output bytes.
`--wait` polls only after a successful empty receive. The default limit is
30,000 ms. A timeout or ioctl error ends the command.

Mail requires a directional `mail` grant. To send or receive with only
that right, use `--epoch 0` to select the grant's bound peer epoch.
Structured output then reports `epoch=0`. An omitted epoch uses INSPECT
once. To pin a wait to one incarnation even if a host replaces the grant
between polls, use its nonzero epoch. The stock Valence service emits UART heartbeats. A mailbox exchange
requires peer firmware that calls the send and receive operations.

`create` requires a template grant and quota. The name is 1–16 ASCII bytes,
starting with a letter and continuing with letters, digits, underscores or
hyphens. The CLI accepts 1–255 vCPUs and 1–65,535 MiB. Omit either option
to inherit that template field. A shape override requires the shape grant. Children default to
diskless. `--disk 0` through `--disk 3` select a separately granted logical
volume. The create reply identifies the new child and its initial epoch.
`destroy` returns the child's last status on successful retirement.

Commands that take `--epoch` first inspect the target when the option is
omitted. With an explicit nonzero epoch they submit the request directly.
For create, supply the template epoch. For subsequent child operations, use
the latest child epoch. A stale epoch, busy result or copy error ends the
request. Inspect the target before deciding whether to issue another action.

`stop --wait` issues STOP once and polls INSPECT for the same epoch until
the state is `stopped` or `faulted`. The successful row shows the observed
state and the original STOP reply's `changed` bit. An epoch change or timeout
ends the wait with an error. The default timeout is 30,000 ms. A timeout
does not cancel the acknowledged stop.

Run the tool and driver tests on a 64-bit Linux host:

```sh
make -C tool test
make -C driver test
```

## Troubleshooting

An ordinary guest may discover the ABI, but cannot control a peer.
Missing device: check that the driver was built/loaded, devtmpfs is mounted,
and the **guest-control** firmware is actually running. Permission denied:
check guest credentials and that the **manager** DT, not just the same kernel
or rootfs, was selected. If Linux cannot boot, the external-Linux serial
profile has no hidden host shell; use a separate embedded debug boot.
