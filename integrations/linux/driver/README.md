# Pauli Linux management bridge

This RV64 guest driver is a client of Pauli, not Linux KVM or an SBI
implementation. When loaded, it probes the vendor EID `0x08004f49` and
requires ABI v1 (FID 0). If absent, it returns `ENODEV` and creates no
device node. The version and address ceiling are not proof of a grant.

The root-owned, mode 0600 misc device `/dev/pauli` accepts only INFO,
INSPECT, START and STOP from a task with `CAP_SYS_ADMIN` in the initial user
namespace. It sends six scalar words using Linux `sbi_ecall` (FID 1):
`[1, op, public_vm_id, expected_epoch, 0, 0]`. No address, caller ID or
raw SBI selector is accepted from userspace. Pauli authenticates the
calling VM/vCPU and authorizes operations itself. INSPECT uses epoch zero;
mutations must carry the epoch returned by INSPECT. Reinspect after a stop
request or an error; never automatically replay a mutation, including
`EFAULT` on copying the reply out, since it may already have taken effect.

Linux errno `EPERM` is the local capability check, `EACCES` is denial by
the hypervisor, and `EAGAIN` is the v1 stale/inactive/busy result. For a
missing or invalid target the hypervisor may return `EINVAL` instead.
The UAPI header is [pauli.h](../include/uapi/linux/pauli.h).

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
