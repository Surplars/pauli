// SPDX-License-Identifier: GPL-2.0-only
/* Scalar Pauli SBI v1 management bridge; no guest pointers cross into HS. */
#include <linux/bitfield.h>
#include <linux/capability.h>
#include <linux/compat.h>
#include <linux/fs.h>
#include <linux/miscdevice.h>
#include <linux/module.h>
#include <linux/uaccess.h>
#include <asm/sbi.h>
#include <uapi/linux/pauli.h>

#define PAULI_SBI_EID	0x08004f49
#define PAULI_ABI	1

/* Status layout shared with pauli-control/src/abi.rs. */
#define STATUS_EPOCH	GENMASK_ULL(31, 0)
#define STATUS_STATE	GENMASK_ULL(39, 32)
#define STATUS_ACTIVE	BIT_ULL(40)
#define STATUS_VCPUS	GENMASK_ULL(48, 41)
#define STATUS_CHANGED	BIT_ULL(49)

static struct pauli_info hv_info;

static int hv_errno(long err)
{
	switch (err) {
	case SBI_SUCCESS:		return 0;
	case SBI_ERR_NOT_SUPPORTED:	return -EOPNOTSUPP;
	case SBI_ERR_INVALID_PARAM:	return -EINVAL;
	case SBI_ERR_DENIED:		return -EACCES;
	case SBI_ERR_INVALID_STATE:	return -EAGAIN; /* stale, inactive, or busy */
	default:			return -EIO;
	}
}

static long hv_ioctl(struct file *file, unsigned int cmd, unsigned long arg)
{
	void __user *out = (void __user *)arg;
	struct pauli_vm vm;
	struct sbiret ret;
	unsigned int op;
	int err;

	/* Check at use, not open: a privileged process can pass a descriptor.
	 * capable() checks the initial user namespace; guest root is still
	 * subject to the HS grant on the actual calling VM and target.
	 */
	if (!capable(CAP_SYS_ADMIN))
		return -EPERM;

	switch (cmd) {
	case PAULI_INFO:
		return copy_to_user(out, &hv_info, sizeof(hv_info)) ? -EFAULT : 0;
	case PAULI_INSPECT:
		op = 0;
		break;
	case PAULI_START:
		op = 1;
		break;
	case PAULI_STOP:
		op = 2;
		break;
	default:
		return -ENOTTY;
	}
	if (copy_from_user(&vm, out, sizeof(vm)))
		return -EFAULT;
	if (!vm.id || vm.id > hv_info.max_id || (op ? !vm.epoch : vm.epoch))
		return -EINVAL;

	ret = sbi_ecall(PAULI_SBI_EID, 1, PAULI_ABI, op, vm.id, vm.epoch, 0, 0);
	err = hv_errno(ret.error);
	if (err)
		return err;
	vm.epoch = FIELD_GET(STATUS_EPOCH, ret.value);
	vm.state = FIELD_GET(STATUS_STATE, ret.value);
	vm.active = FIELD_GET(STATUS_ACTIVE, ret.value);
	vm.vcpus = FIELD_GET(STATUS_VCPUS, ret.value);
	vm.changed = FIELD_GET(STATUS_CHANGED, ret.value);
	return copy_to_user(out, &vm, sizeof(vm)) ? -EFAULT : 0;
}

static const struct file_operations hv_fops = {
	.owner = THIS_MODULE,
	.unlocked_ioctl = hv_ioctl,
	.compat_ioctl = compat_ptr_ioctl,
};

static struct miscdevice hv_device = {
	.minor = MISC_DYNAMIC_MINOR,
	.name = "pauli",
	.fops = &hv_fops,
	.mode = 0600,
};

static int __init hv_init(void)
{
	struct sbiret ret;

	static_assert(BITS_PER_LONG == 64);
	static_assert(sizeof(struct pauli_info) == 8);
	static_assert(sizeof(struct pauli_vm) == 12);
	if (sbi_probe_extension(PAULI_SBI_EID) <= 0)
		return -ENODEV;
	ret = sbi_ecall(PAULI_SBI_EID, 0, 0, 0, 0, 0, 0, 0);
	if (ret.error)
		return hv_errno(ret.error);
	if ((ret.value >> 32) != PAULI_ABI || !(u32)ret.value)
		return -EPROTONOSUPPORT;
	hv_info.version = PAULI_ABI;
	hv_info.max_id = (u32)ret.value;
	return misc_register(&hv_device);
}

static void __exit hv_exit(void)
{
	misc_deregister(&hv_device);
}

module_init(hv_init);
module_exit(hv_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Pauli SBI guest management bridge");
