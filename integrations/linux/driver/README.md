# Pauli Linux management bridge

This RV64 guest driver is a client of Pauli, not Linux KVM or an SBI
implementation. When loaded, it probes the vendor EID `0x08004f49` and
requires ABI v1 (FID 0). If absent, it returns `ENODEV` and creates no
device node. The version and address ceiling are not proof of a grant.

The root-owned, mode 0600 device `/dev/pauli` checks `CAP_SYS_ADMIN` in
the initial user namespace for every ioctl, including use of a passed file
descriptor. Pauli independently checks the calling VM/vCPU's authority.
All ioctl payloads have the same binary layout for RV64 and compat tasks.
See the [UAPI header](../include/uapi/linux/pauli.h).

| ioctl | Number | Payload (bytes) | Result |
| --- | --- | --- | --- |
| PAULI_INFO | 0x70 | pauli_info (8) | ABI version and maximum public ID, cached at probe |
| PAULI_INSPECT | 0x71 | pauli_vm (12) | Current status and epoch (op 0) |
| PAULI_START / PAULI_STOP | 0x72 / 0x73 | pauli_vm (12) | Status and changed bit (ops 1 / 2) |
| PAULI_DESTROY | 0x74 | pauli_vm (12) | Last status of the retired child (op 4) |
| PAULI_CREATE | 0x75 | pauli_create (40) | New child ID and epoch (ops 3 / 5 / 7) |
| PAULI_MEMORY | 0x76 | pauli_memory (16) | Exact unsigned 64-bit memory size in bytes (op 6) |

Each FID 1 call carries six scalars `[1, op, id, metadata, lo, hi]`. INSPECT
uses zero metadata. START, STOP, DESTROY and MEMORY require the target's
nonzero epoch. All four use zero for the last two words. A create targets the
template. Metadata bits 0–31 carry its epoch, 32–39 the vCPU count, 40–55
the memory size in MiB and 56–63 the disk byte. A zero shape field inherits
that template field. A disk byte of 1–4 selects op 7 and denotes logical
slot 0–3. With a zero disk byte, any nonzero shape field selects op 5 and
the default shape selects op 3. Flags must be zero.

The two name words carry bytes 0–7 and 8–15 in little-endian order. The
first byte must be an ASCII letter. Remaining characters may also be digits,
hyphens or underscores. A shorter name requires zero padding through byte
15. A 16-character name fills both words. Only scalar values reach HS.

`EPERM` indicates a failed Linux capability check. `EACCES` indicates an
HS denial. `EINVAL` covers malformed inputs and invalid or missing targets.
`EAGAIN` covers stale epochs, inactive guests and busy resources.
`EOPNOTSUPP` means the operation is unsupported. ABI v1 reports capacity
exhaustion and backend failures with the same SBI error, mapped to `EIO`.
An `EFAULT` copying out a reply can occur after HS committed the operation.
The driver issues each mutation once. Reinspect before deciding how to recover.

Run `make -C integrations/linux/driver test` from the repository root on
a 64-bit Linux host to exercise the production ioctl path with a mock SBI.

## Build without altering the kernel

Use the **same configured kernel build** as the deployed Image. With a
prepared RV64 tree, matching Module.symvers, and a host cross compiler:

```sh
# From the integrations/linux directory. MO (separate module output) is
# supported by the Wave9 Linux build; choose a fresh absolute output path.
make -C "$LINUX_SRC" O="$LINUX_OBJ" ARCH=riscv CROSS_COMPILE=riscv64-linux-gnu- \
     M="$PWD/driver" MO="$MODULE_OUT" modules
```

For the Wave9 Image-only build, the kernel's own `vmlinux.symvers` exists
but `Module.symvers` and `scripts/module.lds` may not. Use the supplied
helper with a **new, disposable output path**. It prepares matching headers,
checks the generated configuration and release, and runs modpost against
the exact kernel's exports. It does not rebuild the Image, install
anything, or write to either input tree:

```sh
bash driver/build-image-module.sh "$LINUX_SRC" "$LINUX_OBJ" "$NEW_WORK_DIR"
modinfo "$NEW_WORK_DIR/module/pauli.ko"
```

Put `pauli.ko` under `/lib/modules/<kernel-release>/extra/` in the
**manager** rootfs. The init script can directly `insmod` that exact file
after devtmpfs is mounted. For `modprobe`, generate the module dependency
index in the staging root and install the module-load policy appropriate to
its init system. A standalone modules-load.d file does not load anything
in a BusyBox init image without such a service. If module signature
enforcement is enabled, have the OS builder sign with its trusted key.

## Build into Linux instead

Copy this directory's `pauli.c`, `Makefile` and `Kconfig` into
`drivers/misc/pauli/` of the target kernel. Copy the UAPI header into
`include/uapi/linux/pauli.h`. Add
`source "drivers/misc/pauli/Kconfig"` to `drivers/misc/Kconfig` and
`obj-$(CONFIG_PAULI_HV) += pauli/` to `drivers/misc/Makefile`.
Enable `CONFIG_PAULI_HV=y` and rebuild the Image. No .ko or insmod is
needed in that case. Do not ship a built-in and a second external copy.
