// SPDX-License-Identifier: GPL-2.0-only
/* Scalar Pauli SBI v1 management bridge; no guest pointers cross into HS. */
#ifndef PAULI_DRIVER_HOST_TEST
#include <linux/bitfield.h>
#include <linux/capability.h>
#include <linux/compat.h>
#include <linux/fs.h>
#include <linux/interrupt.h>
#include <linux/jiffies.h>
#include <linux/miscdevice.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/of.h>
#include <linux/platform_device.h>
#include <linux/poll.h>
#include <linux/slab.h>
#include <linux/stddef.h>
#include <linux/uaccess.h>
#include <asm/sbi.h>
#endif
#include <uapi/linux/pauli.h>

#define PAULI_SBI_EID	0x08004f49
#define PAULI_ABI	1
#define PAULI_DISK_SLOTS	4	/* pauli_control::DiskSlot::COUNT */
#define PAULI_FEATURE_STATS	BIT_ULL(0)
#define STATS_MAGIC		0x5041554c49535431ULL
#define STATS_MAX_OWNERS	64
#define STATS_HEADER_WORDS	8
#define STATS_OWNER_WORDS	16

enum pauli_sbi_op {
	OP_INSPECT = 0, OP_START = 1, OP_STOP = 2, OP_CREATE = 3,
	OP_DESTROY = 4, OP_CREATE_SHAPED = 5, OP_MEMORY_BYTES = 6,
	OP_CREATE_DISK = 7, OP_CONSOLE_READ = 8, OP_MAIL_SEND = 9, OP_MAIL_RECV = 10,
	OP_STATS_WORD = 11,
};

/* Status layout shared with pauli-control/src/abi.rs. */
#define STATUS_EPOCH	GENMASK_ULL(31, 0)
#define STATUS_STATE	GENMASK_ULL(39, 32)
#define STATUS_ACTIVE	BIT_ULL(40)
#define STATUS_VCPUS	GENMASK_ULL(48, 41)
#define STATUS_CHANGED	BIT_ULL(49)

#define LOG_PAYLOAD	GENMASK_ULL(47, 0)
#define LOG_COUNT	GENMASK_ULL(50, 48)
#define LOG_OVERRUN	BIT_ULL(51)
#define LOG_SCALAR_BYTES	6
#define LOG_MAX_CALLS	((PAULI_LOG_CHUNK + LOG_SCALAR_BYTES - 1) / LOG_SCALAR_BYTES)

#define MAIL_PAYLOAD	GENMASK_ULL(47, 0)
#define MAIL_LENGTH	GENMASK_ULL(50, 48)
#define MAIL_PRESENT	BIT_ULL(51)
#define MAIL_MORE	BIT_ULL(52)
#define MAIL_TAG	GENMASK_ULL(60, 53)

static struct pauli_info hv_info;
static int stats_probe_error;

static int hv_errno(long err)
{
	switch (err) {
	case SBI_SUCCESS:		return 0;
	case SBI_ERR_NOT_SUPPORTED:	return -EOPNOTSUPP;
	case SBI_ERR_INVALID_PARAM:	return -EINVAL;
	case SBI_ERR_DENIED:		return -EACCES;
	case SBI_ERR_INVALID_STATE:	return -EAGAIN; /* stale, inactive, or busy */
	/* ABI v1 reports both exhausted capacity and backend faults as FAILED. */
	default:			return -EIO;
	}
}

static bool valid_id(u32 id)
{
	return id && id <= hv_info.max_id;
}

#include "pauli-events.h"

static struct sbiret hv_call(unsigned int op, u32 id, u64 metadata, u64 lo, u64 hi)
{
	return sbi_ecall(PAULI_SBI_EID, 1, PAULI_ABI, op, id, metadata, lo, hi);
}

static bool ascii_letter(unsigned char c)
{
	return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z');
}

static bool pack_name(const char name[PAULI_NAME_MAX], u64 words[2])
{
	unsigned int i;

	if (!ascii_letter(name[0]))
		return false;
	for (i = 0; i < PAULI_NAME_MAX && name[i]; i++) {
		unsigned char c = name[i];

		if (!ascii_letter(c) && !(c >= '0' && c <= '9') && c != '-' && c != '_')
			return false;
		/* ChildName::from_words interprets both scalars as little endian. */
		words[i / 8] |= (u64)c << (8 * (i % 8));
	}
	for (; i < PAULI_NAME_MAX; i++)
		if (name[i])
			return false;
	return true;
}

static long hv_create(void __user *arg)
{
	struct pauli_create req;
	struct sbiret ret;
	u64 name[2] = {}, metadata;
	unsigned int op;
	int err;

	if (copy_from_user(&req, arg, sizeof(req)))
		return -EFAULT;
	if (!valid_id(req.template_id) || !req.epoch || req.flags ||
	    req.disk > PAULI_DISK_SLOTS || !pack_name(req.name, name))
		return -EINVAL;

	metadata = req.epoch | ((u64)req.vcpus << 32) |
		   ((u64)req.memory_mib << 40) | ((u64)req.disk << 56);
	if (req.disk != PAULI_DISK_NONE)
		op = OP_CREATE_DISK;
	else if (req.memory_mib || req.vcpus)
		op = OP_CREATE_SHAPED;
	else
		op = OP_CREATE;
	ret = hv_call(op, req.template_id, metadata, name[0], name[1]);
	err = hv_errno(ret.error);
	if (err)
		return err;
	req.id = (u64)ret.value >> 32;
	req.child_epoch = (u32)ret.value;
	return copy_to_user(arg, &req, sizeof(req)) ? -EFAULT : 0;
}

static long hv_memory(void __user *arg)
{
	struct pauli_memory req;
	struct sbiret ret;
	int err;

	if (copy_from_user(&req, arg, sizeof(req)))
		return -EFAULT;
	if (!valid_id(req.id) || !req.epoch)
		return -EINVAL;
	ret = hv_call(OP_MEMORY_BYTES, req.id, req.epoch, 0, 0);
	err = hv_errno(ret.error);
	if (err)
		return err;
	req.bytes = (u64)ret.value;
	return copy_to_user(arg, &req, sizeof(req)) ? -EFAULT : 0;
}

static long hv_log_read(void __user *arg)
{
	struct pauli_log req = {};
	unsigned int calls;

	if (copy_from_user(&req, arg, offsetof(struct pauli_log, data)))
		return -EFAULT;
	if (!valid_id(req.id) || !req.epoch || req.flags)
		return -EINVAL;
	req.len = 0;
	for (calls = 0; calls < LOG_MAX_CALLS; calls++) {
		struct sbiret ret = hv_call(OP_CONSOLE_READ, req.id, req.epoch, req.cursor, 0);
		u64 word = (u64)ret.value;
		unsigned int i, count = FIELD_GET(LOG_COUNT, word);
		int err = hv_errno(ret.error);

		if (err)
			return err;
		if ((word >> 52) || count > LOG_SCALAR_BYTES)
			return -EIO;
		if (word & LOG_OVERRUN) {
			u64 resume = word & LOG_PAYLOAD;

			if (count || resume <= req.cursor)
				return -EIO;
			if (!req.len) {
				req.cursor = resume;
				req.flags = PAULI_LOG_OVERRUN;
			}
			break;
		}
		if (!count)
			break;
		/* The last scalar can straddle the ioctl buffer boundary. */
		if (count > PAULI_LOG_CHUNK - req.len)
			count = PAULI_LOG_CHUNK - req.len;
		if (req.cursor + count < req.cursor)
			return -EIO;
		for (i = 0; i < count; i++)
			req.data[req.len++] = word >> (8 * i);
		req.cursor += count;
	}
	return copy_to_user(arg, &req, sizeof(req)) ? -EFAULT : 0;
}

static long hv_mail(void __user *arg, unsigned int op)
{
	struct pauli_mail req;
	struct sbiret ret;
	u64 payload = 0, metadata, word;
	unsigned int i;
	int err;

	if (copy_from_user(&req, arg, sizeof(req)))
		return -EFAULT;
	if (!valid_id(req.id) || req.flags)
		return -EINVAL;

	if (op == OP_MAIL_SEND) {
		if (req.len > PAULI_MAIL_MAX)
			return -EINVAL;
		for (i = 0; i < req.len; i++)
			payload |= (u64)req.data[i] << (8 * i);
		for (; i < sizeof(req.data); i++)
			if (req.data[i])
				return -EINVAL;
		metadata = req.epoch | ((u64)req.len << 32) | ((u64)req.tag << 40);
		ret = hv_call(op, req.id, metadata, payload, 0);
		return ret.error ? hv_errno(ret.error) : (ret.value ? -EIO : 0);
	}

	ret = hv_call(op, req.id, req.epoch, 0, 0);
	err = hv_errno(ret.error);
	if (err)
		return err;
	word = (u64)ret.value;
	req.len = FIELD_GET(MAIL_LENGTH, word);
	if ((word >> 61) || (!(word & MAIL_PRESENT) && word) ||
	    req.len > PAULI_MAIL_MAX || ((word & MAIL_PAYLOAD) >> (8 * req.len)))
		return -EIO;
	req.tag = FIELD_GET(MAIL_TAG, word);
	if (word & MAIL_PRESENT)
		req.flags = PAULI_MAIL_PRESENT | ((word & MAIL_MORE) ? PAULI_MAIL_MORE : 0);
	for (i = 0; i < sizeof(req.data); i++)
		req.data[i] = i < req.len ? word >> (8 * i) : 0;
	/* A successful receive has already removed the message, including on EFAULT. */
	return copy_to_user(arg, &req, sizeof(req)) ? -EFAULT : 0;
}

/* FID 1 op 11 authorizes each read against the currently calling VM. */
static int hv_stats_words(unsigned int section, u32 owner, u64 *words, unsigned int count)
{
	unsigned int i;

	for (i = 0; i < count; i++) {
		u64 selector = section | ((u64)owner << 8) | ((u64)i << 16);
		struct sbiret ret = hv_call(OP_STATS_WORD, 0, selector, 0, 0);

		if (ret.error)
			return hv_errno(ret.error);
		words[i] = (u64)ret.value;
	}
	return 0;
}

static long hv_stats_header(void __user *arg)
{
	struct pauli_stats_header header = { .flags = PAULI_STATS_TRAP_REDACTED };
	u64 words[STATS_HEADER_WORDS];
	int err;

	if (stats_probe_error)
		return stats_probe_error;
	err = hv_stats_words(0, 0, words, STATS_HEADER_WORDS);
	if (err)
		return err;
	if (words[0] != STATS_MAGIC || words[1] != 2 ||
	    words[2] != sizeof(words) || words[3] != STATS_OWNER_WORDS * sizeof(u64) ||
	    !words[4] || words[4] > STATS_MAX_OWNERS || words[7])
		return -EIO;
	if (words[4] < STATS_MAX_OWNERS && ((words[5] | words[6]) >> words[4]))
		return -EIO;
	header.version = words[1];
	header.host_cpus = words[4];
	header.sstc_owners = words[5];
	header.exclusive_owners = words[6];
	return copy_to_user(arg, &header, sizeof(header)) ? -EFAULT : 0;
}

static long hv_stats_owner(void __user *arg)
{
	struct pauli_stats_owner out = { .flags = PAULI_STATS_TRAP_REDACTED };
	u64 words[STATS_OWNER_WORDS];
	int err;

	if (stats_probe_error)
		return stats_probe_error;
	if (copy_from_user(&out.owner, arg, sizeof(out.owner)))
		return -EFAULT;
	if (out.owner >= STATS_MAX_OWNERS)
		return -EINVAL;
	err = hv_stats_words(1, out.owner, words, STATS_OWNER_WORDS);
	if (err)
		return err;
	out.hart_id = words[0];
	out.registered = out.hart_id != ~0ULL;
	out.vm_exits = words[1];
	out.wfi_exits = words[2];
	out.sbi_timer_exits = words[3];
	out.sstc_timer_exits = words[4];
	out.mmio_exits = words[5];
	out.other_exits = words[6];
	out.vtimer_irqs = words[7];
	out.ext_irqs = words[8];
	/* Trap context and reserved words stay zero even on a malformed reply. */
	out.idle_wfi = words[12];
	out.mail_sends = words[13];
	out.mail_recvs = words[14];
	return copy_to_user(arg, &out, sizeof(out)) ? -EFAULT : 0;
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
	case PAULI_EVENT_INFO:
		return hv_event_info(out);
	case PAULI_EVENT_ARM:
		return hv_event_arm(file, out);
	case PAULI_EVENT_READ:
		return hv_event_read(file, out);
	case PAULI_INFO:
		return copy_to_user(out, &hv_info, sizeof(hv_info)) ? -EFAULT : 0;
	case PAULI_INSPECT:
		op = OP_INSPECT;
		break;
	case PAULI_START:
		op = OP_START;
		break;
	case PAULI_STOP:
		op = OP_STOP;
		break;
	case PAULI_DESTROY:
		op = OP_DESTROY;
		break;
	case PAULI_CREATE:
		return hv_create(out);
	case PAULI_MEMORY:
		return hv_memory(out);
	case PAULI_LOG_READ:
		return hv_log_read(out);
	case PAULI_MAIL_SEND:
		return hv_mail(out, OP_MAIL_SEND);
	case PAULI_MAIL_RECV:
		return hv_mail(out, OP_MAIL_RECV);
	case PAULI_STATS_HEADER:
		return hv_stats_header(out);
	case PAULI_STATS_OWNER:
		return hv_stats_owner(out);
	default:
		return -ENOTTY;
	}
	if (copy_from_user(&vm, out, sizeof(vm)))
		return -EFAULT;
	if (!valid_id(vm.id) || (op == OP_INSPECT ? vm.epoch : !vm.epoch))
		return -EINVAL;

	ret = hv_call(op, vm.id, vm.epoch, 0, 0);
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
	.open = hv_open,
	.release = hv_release,
	.poll = hv_poll,
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
	int err;

	static_assert(BITS_PER_LONG == 64);
	static_assert(sizeof(struct pauli_event_info) == 16);
	static_assert(sizeof(struct pauli_event_arm) == 8);
	static_assert(sizeof(struct pauli_events) == 8);
	static_assert(sizeof(struct pauli_info) == 8);
	static_assert(sizeof(struct pauli_vm) == 12);
	static_assert(sizeof(struct pauli_memory) == 16);
	static_assert(offsetof(struct pauli_memory, bytes) == 8);
	static_assert(sizeof(struct pauli_create) == 40);
	static_assert(offsetof(struct pauli_create, id) == 8);
	static_assert(offsetof(struct pauli_create, child_epoch) == 12);
	static_assert(offsetof(struct pauli_create, memory_mib) == 16);
	static_assert(offsetof(struct pauli_create, vcpus) == 18);
	static_assert(offsetof(struct pauli_create, disk) == 19);
	static_assert(offsetof(struct pauli_create, flags) == 20);
	static_assert(offsetof(struct pauli_create, name) == 24);
	static_assert(sizeof(struct pauli_log) == 280);
	static_assert(offsetof(struct pauli_log, id) == 0);
	static_assert(offsetof(struct pauli_log, epoch) == 4);
	static_assert(offsetof(struct pauli_log, cursor) == 8);
	static_assert(offsetof(struct pauli_log, len) == 16);
	static_assert(offsetof(struct pauli_log, flags) == 20);
	static_assert(offsetof(struct pauli_log, data) == 24);
	static_assert(sizeof(struct pauli_mail) == 20);
	static_assert(offsetof(struct pauli_mail, id) == 0);
	static_assert(offsetof(struct pauli_mail, epoch) == 4);
	static_assert(offsetof(struct pauli_mail, tag) == 8);
	static_assert(offsetof(struct pauli_mail, len) == 9);
	static_assert(offsetof(struct pauli_mail, flags) == 10);
	static_assert(offsetof(struct pauli_mail, data) == 12);
	static_assert(sizeof(struct pauli_stats_header) == 32);
	static_assert(offsetof(struct pauli_stats_header, sstc_owners) == 8);
	static_assert(offsetof(struct pauli_stats_header, flags) == 24);
	static_assert(sizeof(struct pauli_stats_owner) == 144);
	static_assert(offsetof(struct pauli_stats_owner, hart_id) == 16);
	static_assert(offsetof(struct pauli_stats_owner, reserved) == 136);
	if (sbi_probe_extension(PAULI_SBI_EID) <= 0)
		return -ENODEV;
	ret = sbi_ecall(PAULI_SBI_EID, 0, 0, 0, 0, 0, 0, 0);
	if (ret.error)
		return hv_errno(ret.error);
	if ((ret.value >> 32) != PAULI_ABI || !(u32)ret.value)
		return -EPROTONOSUPPORT;
	hv_info.version = PAULI_ABI;
	hv_info.max_id = (u32)ret.value;
	/* Optional feature discovery cannot prevent registration on an older HS. */
	ret = sbi_ecall(PAULI_SBI_EID, 2, 0, 0, 0, 0, 0, 0);
	stats_probe_error = hv_errno(ret.error);
	if (!stats_probe_error && !(ret.value & PAULI_FEATURE_STATS))
		stats_probe_error = -EOPNOTSUPP;
	hv_events_register(ret);
	err = misc_register(&hv_device);
	if (err)
		hv_events_unregister();
	return err;
}

static void __exit hv_exit(void)
{
	misc_deregister(&hv_device);
	hv_events_unregister();
}

module_init(hv_init);
module_exit(hv_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Pauli SBI guest management bridge");
