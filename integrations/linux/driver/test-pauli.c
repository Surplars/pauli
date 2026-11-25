// SPDX-License-Identifier: GPL-2.0-only
// Host-side adapter tests. Only the Linux service calls and SBI endpoint are mocked.
#include <assert.h>
#include <errno.h>
#include <limits.h>
#include <linux/capability.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

typedef uint32_t u32;
typedef uint64_t u64;
#define BITS_PER_LONG (sizeof(long) * CHAR_BIT)
#define BIT_ULL(bit) (1ULL << (bit))
#define GENMASK_ULL(hi, lo) ((~0ULL >> (63 - (hi))) & (~0ULL << (lo)))
#define FIELD_GET(mask, word) (((u64)(word) & (mask)) >> __builtin_ctzll(mask))
#define __user
#define __init
#define __exit
#define THIS_MODULE NULL
#define MISC_DYNAMIC_MINOR 255
#define module_init(fn)
#define module_exit(fn)
#define MODULE_LICENSE(value)
#define MODULE_DESCRIPTION(value)

struct file;
struct file_operations {
	void *owner;
	long (*unlocked_ioctl)(struct file *, unsigned int, unsigned long);
	long (*compat_ioctl)(struct file *, unsigned int, unsigned long);
};
struct miscdevice {
	int minor;
	const char *name;
	const struct file_operations *fops;
	unsigned int mode;
};
static long compat_ptr_ioctl(struct file *, unsigned int, unsigned long);

enum {
	SBI_SUCCESS = 0, SBI_ERR_FAILED = -1, SBI_ERR_NOT_SUPPORTED = -2,
	SBI_ERR_INVALID_PARAM = -3, SBI_ERR_DENIED = -4, SBI_ERR_INVALID_STATE = -10,
};
struct sbiret { long error, value; };
static struct {
	bool privileged, read_fault, write_fault;
	unsigned int ecalls, reads, writes;
	unsigned long words[8];
	struct sbiret reply;
} mock;
static struct miscdevice *registered;
static int sbi_present = 1;

static bool capable(int capability)
{
	assert(capability == CAP_SYS_ADMIN);
	return mock.privileged;
}
static size_t copy_from_user(void *to, const void *from, size_t size)
{
	++mock.reads;
	if (mock.read_fault)
		return size;
	memcpy(to, from, size);
	return 0;
}
static size_t copy_to_user(void *to, const void *from, size_t size)
{
	++mock.writes;
	if (mock.write_fault)
		return size;
	memcpy(to, from, size);
	return 0;
}
static long sbi_probe_extension(unsigned long eid)
{
	assert(eid == 0x08004f49);
	return sbi_present;
}
static struct sbiret sbi_ecall(unsigned long eid, unsigned long fid, unsigned long a0,
			      unsigned long a1, unsigned long a2, unsigned long a3,
			      unsigned long a4, unsigned long a5)
{
	unsigned long words[] = {eid, fid, a0, a1, a2, a3, a4, a5};
	++mock.ecalls;
	memcpy(mock.words, words, sizeof(words));
	return mock.reply;
}
static int misc_register(struct miscdevice *device)
{
	assert(!registered);
	registered = device;
	return 0;
}
static void misc_deregister(struct miscdevice *device)
{
	assert(registered == device);
	registered = NULL;
}

#define PAULI_DRIVER_HOST_TEST
#include "pauli.c"

static long compat_ptr_ioctl(struct file *file, unsigned int cmd, unsigned long arg)
{
	return registered->fops->unlocked_ioctl(file, cmd, arg);
}

// Existing Linux ioctl numbers are part of the deployed RV64 and compat ABI.
static_assert(PAULI_INFO == 0x80085070U && PAULI_INSPECT == 0xc00c5071U);
static_assert(PAULI_START == 0xc00c5072U && PAULI_STOP == 0xc00c5073U);
static_assert(PAULI_DESTROY == 0xc00c5074U && PAULI_CREATE == 0xc0285075U);
static_assert(PAULI_MEMORY == 0xc0105076U);
static_assert(sizeof(struct pauli_info) == 8 && sizeof(struct pauli_vm) == 12);
static_assert(offsetof(struct pauli_vm, changed) == 11);
static_assert(sizeof(struct pauli_create) == 40 && offsetof(struct pauli_create, epoch) == 4);
static_assert(offsetof(struct pauli_create, id) == 8 && offsetof(struct pauli_create, child_epoch) == 12);
static_assert(offsetof(struct pauli_create, memory_mib) == 16 && offsetof(struct pauli_create, vcpus) == 18);
static_assert(offsetof(struct pauli_create, disk) == 19 && offsetof(struct pauli_create, flags) == 20);
static_assert(offsetof(struct pauli_create, name) == 24);
static_assert(sizeof(struct pauli_memory) == 16 && offsetof(struct pauli_memory, bytes) == 8);

static long call(unsigned int cmd, void *input)
{
	mock.ecalls = mock.reads = mock.writes = 0;
	return registered->fops->unlocked_ioctl(NULL, cmd, (unsigned long)input);
}
static void check_wire(unsigned int op, u32 id, u64 meta, u64 lo, u64 hi)
{
	const unsigned long expected[] = {0x08004f49, 1, 1, op, id, meta, lo, hi};
	assert(mock.ecalls == 1 && mock.reads == 1);
	assert(!memcmp(mock.words, expected, sizeof(expected)));
}
static void rejected(unsigned int cmd, void *input)
{
	assert(call(cmd, input) == -EINVAL);
	assert(mock.ecalls == 0 && mock.reads == 1 && mock.writes == 0);
}
static void probe(void)
{
	const unsigned long expected[] = {0x08004f49, 0, 0, 0, 0, 0, 0, 0};
	sbi_present = 0;
	assert(hv_init() == -ENODEV && !registered && !mock.ecalls);
	sbi_present = 1;
	mock.reply.value = 2ULL << 32 | 8;
	assert(hv_init() == -EPROTONOSUPPORT && !registered);
	mock.reply.value = 1ULL << 32;
	assert(hv_init() == -EPROTONOSUPPORT && !registered);
	mock.reply.value |= 8;
	assert(hv_init() == 0 && registered && registered->mode == 0600);
	assert(!strcmp(registered->name, "pauli") && registered->fops->compat_ioctl == compat_ptr_ioctl);
	assert(!memcmp(mock.words, expected, sizeof(expected)));
}
static void permissions_and_info(void)
{
	const unsigned int cmds[] = {PAULI_INFO, PAULI_INSPECT, PAULI_START, PAULI_STOP,
				     PAULI_DESTROY, PAULI_CREATE, PAULI_MEMORY};
	struct pauli_info info;
	for (size_t i = 0; i < sizeof(cmds)/sizeof(cmds[0]); ++i) {
		assert(call(cmds[i], NULL) == -EPERM);
		assert(!mock.reads && !mock.writes && !mock.ecalls);
	}
	mock.privileged = true;
	assert(call(PAULI_INFO, &info) == 0 && info.version == 1 && info.max_id == 8);
	assert(!mock.reads && mock.writes == 1 && !mock.ecalls);
	assert(call(0, NULL) == -ENOTTY && !mock.reads && !mock.writes && !mock.ecalls);
}
static void legacy_and_destroy(void)
{
	const unsigned int cmds[] = {PAULI_INSPECT, PAULI_START, PAULI_STOP, PAULI_DESTROY};
	const unsigned int ops[] = {0, 1, 2, 4};
	for (size_t i = 0; i < sizeof(ops)/sizeof(ops[0]); ++i) {
		u32 epoch = i ? 0x87654321U : 0;
		struct pauli_vm vm = {.id = 8, .epoch = epoch};
		mock.reply.value = 0x80000001ULL | 7ULL << 32 | 1ULL << 40 | 129ULL << 41 | 1ULL << 49;
		assert(call(cmds[i], &vm) == 0 && mock.writes == 1);
		check_wire(ops[i], 8, epoch, 0, 0);
		assert(vm.id == 8 && vm.epoch == 0x80000001U && vm.state == PAULI_FAULTED);
		assert(vm.active == 1 && vm.vcpus == 129 && vm.changed == 1);
		vm.epoch = i ? 0 : 1;
		rejected(cmds[i], &vm);
		vm.epoch = epoch;
		vm.id = 0;
		rejected(cmds[i], &vm);
		vm.id = 9;
		rejected(cmds[i], &vm);
	}
}

static struct pauli_create template(void)
{
	struct pauli_create create = {.template_id = 2, .epoch = 0xdeadbeefU,
				      .id = 77, .child_epoch = 123};
	memcpy(create.name, "alpha-0123456789", sizeof(create.name));
	return create;
}
static void creates(void)
{
	// Production Rust abi.rs: op 3 inherits, op 5 carries shape, op 7 carries disk 1..4.
	const struct { unsigned int op, vcpus, mib, disk; u64 meta; } cases[] = {
		{3, 0, 0, 0, 0x00000000deadbeefULL},
		{5, 2, 0, 0, 0x00000002deadbeefULL},
		{5, 0, 0xabcd, 0, 0x00abcd00deadbeefULL},
		{7, 0, 0, 1, 0x01000000deadbeefULL},
		{7, 255, 65535, 4, 0x04ffffffdeadbeefULL},
	};
	for (size_t i = 0; i < sizeof(cases)/sizeof(cases[0]); ++i) {
		struct pauli_create create = template();
		create.vcpus = cases[i].vcpus;
		create.memory_mib = cases[i].mib;
		create.disk = cases[i].disk;
		mock.reply.value = (6ULL << 32) | 0xfedcba98U;
		assert(call(PAULI_CREATE, &create) == 0 && mock.writes == 1);
		check_wire(cases[i].op, 2, cases[i].meta, 0x31302d6168706c61ULL, 0x3938373635343332ULL);
		assert(create.id == 6 && create.child_epoch == 0xfedcba98U);
		assert(create.template_id == 2 && create.epoch == 0xdeadbeefU);
	}
	struct pauli_create create = template();
	memset(create.name, 0, sizeof(create.name));
	memcpy(create.name, "Z_9", 3);
	assert(call(PAULI_CREATE, &create) == 0);
	check_wire(3, 2, 0xdeadbeefU, 0x395f5a, 0);
}
static void rejected_creates(void)
{
	const char invalid[][16] = {"", "1abc", "_abc", "a/b", "a b", "x\0y", "a\xff"};
	struct pauli_create create;
	for (size_t i = 0; i < sizeof(invalid)/sizeof(invalid[0]); ++i) {
		create = template();
		memcpy(create.name, invalid[i], sizeof(create.name));
		rejected(PAULI_CREATE, &create);
	}
	create = template(); create.template_id = 0;
	rejected(PAULI_CREATE, &create);
	create.template_id = 9;
	rejected(PAULI_CREATE, &create);
	create = template(); create.epoch = 0;
	rejected(PAULI_CREATE, &create);
	create = template(); create.flags = 0x80000000U;
	rejected(PAULI_CREATE, &create);
	create = template(); create.disk = 5;
	rejected(PAULI_CREATE, &create);
}
static void memory_bytes(void)
{
	struct pauli_memory mem = {.id = 2, .epoch = 0xabcdef01U};
	mock.reply.value = 0x123456789abcde01UL;
	assert(call(PAULI_MEMORY, &mem) == 0 && mock.writes == 1);
	check_wire(6, 2, 0xabcdef01U, 0, 0);
	assert(mem.bytes == 0x123456789abcde01ULL && mem.id == 2 && mem.epoch == 0xabcdef01U);
	mem.epoch = 0;
	rejected(PAULI_MEMORY, &mem);
	mem.epoch = 1; mem.id = 0;
	rejected(PAULI_MEMORY, &mem);
	mem.id = 9;
	rejected(PAULI_MEMORY, &mem);
}
static void errors_and_copy_faults(void)
{
	const struct { long sbi; int error; } errors[] = {
		{-1, EIO}, {-2, EOPNOTSUPP}, {-3, EINVAL}, {-4, EACCES},
		{-10, EAGAIN}, {-99, EIO},
	};
	struct pauli_create create = template();
	struct pauli_vm vm = {.id = 2, .epoch = 7};
	struct pauli_memory mem = {.id = 2, .epoch = 7};
	struct pauli_info info;
	const struct { unsigned int cmd; void *arg; } requests[] = {
		{PAULI_START, &vm}, {PAULI_DESTROY, &vm}, {PAULI_CREATE, &create}, {PAULI_MEMORY, &mem},
	};
	for (size_t i = 0; i < sizeof(requests)/sizeof(requests[0]); ++i) {
		mock.read_fault = true;
		assert(call(requests[i].cmd, requests[i].arg) == -EFAULT && !mock.ecalls && !mock.writes);
		mock.read_fault = false;
		for (size_t j = 0; j < sizeof(errors)/sizeof(errors[0]); ++j) {
			mock.reply.error = errors[j].sbi;
			assert(call(requests[i].cmd, requests[i].arg) == -errors[j].error);
			assert(mock.ecalls == 1 && mock.writes == 0);
		}
		mock.reply.error = 0;
		mock.write_fault = true;
		assert(call(requests[i].cmd, requests[i].arg) == -EFAULT);
		assert(mock.ecalls == 1 && mock.writes == 1); // Mutation may already have completed.
		mock.write_fault = false;
	}
	mock.write_fault = true;
	assert(call(PAULI_INFO, &info) == -EFAULT && !mock.ecalls && mock.writes == 1);
	mock.write_fault = false;
}
int main(void)
{
	probe();
	permissions_and_info();
	legacy_and_destroy();
	creates();
	rejected_creates();
	memory_bytes();
	errors_and_copy_faults();
	hv_exit();
	assert(!registered);
	puts("PASS: Pauli driver probe, ioctl ABI, SBI words and replies, validation, credentials and copy faults");
	return 0;
}
