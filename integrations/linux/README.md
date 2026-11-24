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
rights. The native manager profile gives VM 1 only inspect/start/stop on its
declared service VM (ID 2); ordinary Linux has no cross-VM rights.

```sh
paulictl info
paulictl status service
paulictl start service
paulictl status service
paulictl stop service
paulictl status service
```

`info` reports the ABI and the largest addressable ID, **not** a list of
accessible VMs. `service` is shorthand for public ID 2 on this board;
numeric IDs also work. Stop is asynchronous: if status is `stopping`, query
again until `stopped`. The service runs on its own virtual console; its
output is not mixed into the Linux shell. Confirm status, not a UART banner.
Each mutation uses the epoch obtained by a preceding inspect. A stale or
busy error is returned without retrying against an unknown incarnation.

An ordinary guest may discover the ABI, but cannot control a peer.
Missing device: check that the driver was built/loaded, devtmpfs is mounted,
and the **guest-control** firmware is actually running. Permission denied:
check guest credentials and that the **manager** DT, not just the same kernel
or rootfs, was selected. If Linux cannot boot, the external-Linux serial
profile has no hidden host shell; use a separate embedded debug boot.
