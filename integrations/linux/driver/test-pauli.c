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
	struct sbiret features;
	struct sbiret (*log_reply)(unsigned long cursor);
	struct sbiret (*stats_reply)(unsigned long selector);
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
	if (fid == 0 || fid == 2) {
		assert(eid == 0x08004f49 && !(a0 | a1 | a2 | a3 | a4 | a5));
		return fid == 0 ? mock.reply : mock.features;
	}
	if (mock.stats_reply) {
		assert(eid == 0x08004f49 && fid == 1 && a0 == 1 && a1 == 11);
		assert(!(a2 | a4 | a5));
		return mock.stats_reply(a3);
	}
	if (mock.log_reply) {
		assert(eid == 0x08004f49 && fid == 1 && a0 == 1 && a1 == 8);
		assert(a2 == 2 && a3 == 7 && a5 == 0);
		return mock.log_reply(a4);
	}
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
static_assert(PAULI_LOG_READ == 0xc1185077U && sizeof(struct pauli_log) == 280);
static_assert(PAULI_MAIL_SEND == 0x40145078U && PAULI_MAIL_RECV == 0xc0145079U);
static_assert(PAULI_STATS_HEADER == 0x8020507aU && sizeof(struct pauli_stats_header) == 32);
static_assert(PAULI_STATS_OWNER == 0xc090507bU && sizeof(struct pauli_stats_owner) == 144);
static_assert(offsetof(struct pauli_stats_owner, last_scause) == 88);
static_assert(offsetof(struct pauli_stats_owner, idle_wfi) == 112);
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
	const unsigned long expected[] = {0x08004f49, 2, 0, 0, 0, 0, 0, 0};
	sbi_present = 0;
	assert(hv_init() == -ENODEV && !registered && !mock.ecalls);
	sbi_present = 1;
	mock.reply.value = 2ULL << 32 | 8;
	assert(hv_init() == -EPROTONOSUPPORT && !registered);
	mock.reply.value = 1ULL << 32;
	assert(hv_init() == -EPROTONOSUPPORT && !registered);
	mock.reply.value |= 8;
	mock.features.value = 1;
	assert(hv_init() == 0 && registered && registered->mode == 0600);
	assert(!strcmp(registered->name, "pauli") && registered->fops->compat_ioctl == compat_ptr_ioctl);
	assert(!memcmp(mock.words, expected, sizeof(expected)));
}
static void permissions_and_info(void)
{
	const unsigned int cmds[] = {PAULI_INFO, PAULI_INSPECT, PAULI_START, PAULI_STOP,
				     PAULI_DESTROY, PAULI_CREATE, PAULI_MEMORY, PAULI_LOG_READ,
				     PAULI_MAIL_SEND, PAULI_MAIL_RECV, PAULI_STATS_HEADER, PAULI_STATS_OWNER};
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

static const u64 log_start = 0x100000001ULL;
static u64 log_end, oldest;
static unsigned int overrun_call, stale_call, scalar_size;

static unsigned char stream_byte(u64 cursor)
{
	return (unsigned char)((cursor - log_start) * 127);
}

static struct sbiret log_stream(unsigned long cursor)
{
	u64 value = 0;
	unsigned int count = 0;

	if (mock.ecalls == stale_call)
		return (struct sbiret){SBI_ERR_INVALID_STATE, 0};
	if (mock.ecalls == overrun_call)
		oldest = cursor + 256;
	if (cursor < oldest)
		return (struct sbiret){0, 1ULL << 51 | oldest};
	if (cursor > log_end)
		return (struct sbiret){SBI_ERR_INVALID_PARAM, 0};
	while (count < scalar_size && cursor + count < log_end) {
		value |= (u64)stream_byte(cursor + count) << (8 * count);
		count++;
	}
	return (struct sbiret){0, (u64)count << 48 | value};
}

static void logs(void)
{
	struct pauli_log log = {.id = 2, .epoch = 7, .cursor = log_start};
	mock.log_reply = log_stream;
	scalar_size = 6;
	oldest = log_start;
	log_end = log_start + 259;
	log.len = ~0U; /* output-only */
	memset(log.data, 0xa5, sizeof(log.data));
	assert(call(PAULI_LOG_READ, &log) == 0 && mock.ecalls == 43 && mock.writes == 1);
	assert(log.id == 2 && log.epoch == 7 && !log.flags && log.len == 256);
	assert(log.cursor == log_start + 256);
	for (unsigned int i = 0; i < log.len; i++)
		assert(log.data[i] == stream_byte(log_start + i));
	assert(call(PAULI_LOG_READ, &log) == 0 && mock.ecalls == 2);
	assert(!log.flags && log.len == 3 && log.cursor == log_end);
	for (unsigned int i = 0; i < sizeof(log.data); i++)
		assert(log.data[i] == (i < 3 ? stream_byte(log_start + 256 + i) : 0));
	assert(call(PAULI_LOG_READ, &log) == 0 && mock.ecalls == 1);
	assert(!log.flags && !log.len && log.cursor == log_end);
	log.cursor++;
	assert(call(PAULI_LOG_READ, &log) == -EINVAL && !mock.writes);

	log.cursor = log_start;
	scalar_size = 1; /* Short data replies also obey the 43-call budget. */
	assert(call(PAULI_LOG_READ, &log) == 0 && mock.ecalls == 43);
	assert(log.len == 43 && log.cursor == log_start + 43);
	log.cursor = log_start;
	scalar_size = 6;
	stale_call = 2;
	assert(call(PAULI_LOG_READ, &log) == -EAGAIN && mock.ecalls == 2 && !mock.writes);
	assert(log.cursor == log_start);
	stale_call = 0;
	overrun_call = 2;
	assert(call(PAULI_LOG_READ, &log) == 0 && mock.ecalls == 2);
	assert(log.len == 6 && log.cursor == log_start + 6 && !log.flags);
	for (unsigned int i = 0; i < log.len; i++)
		assert(log.data[i] == stream_byte(log_start + i));
	overrun_call = 0;
	log_end = oldest + 1;
	assert(call(PAULI_LOG_READ, &log) == 0 && mock.ecalls == 1);
	assert(log.len == 0 && log.cursor == oldest && log.flags == PAULI_LOG_OVERRUN);
	rejected(PAULI_LOG_READ, &log); /* Clear output flags before resubmitting. */
	log.flags = 0;
	assert(call(PAULI_LOG_READ, &log) == 0 && log.len == 1 && log.cursor == log_end);
	assert(log.data[0] == stream_byte(oldest));
	mock.log_reply = NULL;

	log.epoch = 0;
	rejected(PAULI_LOG_READ, &log);
	log.epoch = 7; log.id = 0;
	rejected(PAULI_LOG_READ, &log);
	log.id = 9;
	rejected(PAULI_LOG_READ, &log);
	log.id = 2;
	const u64 malformed[] = {1ULL << 52, 7ULL << 48, 9ULL << 48, 1ULL << 51 | log.cursor};
	for (size_t i = 0; i < sizeof(malformed)/sizeof(malformed[0]); i++) {
		mock.reply.value = malformed[i];
		assert(call(PAULI_LOG_READ, &log) == -EIO && mock.ecalls == 1 && !mock.writes);
	}
	log.cursor = ~0ULL;
	mock.reply.value = 1ULL << 48;
	assert(call(PAULI_LOG_READ, &log) == -EIO && !mock.writes);
	mock.reply.value = 0;
}
static void mail(void)
{
	const u64 payload = 0xfe00dcba9800ULL;
	const unsigned char bytes[] = {0x00, 0x98, 0xba, 0xdc, 0x00, 0xfe};
	const u64 malformed[] = {1, 1ULL << 52, 1ULL << 53, 1ULL << 61,
		1ULL << 63 | 1ULL << 51, 7ULL << 48 | 1ULL << 51,
		1ULL << 51 | 1, 1ULL << 48 | 1ULL << 51 | 0x100};
	const unsigned int cmds[] = {PAULI_MAIL_SEND, PAULI_MAIL_RECV};
	struct pauli_mail m;

	for (unsigned int len = 0; len <= PAULI_MAIL_MAX; len++) {
		u32 epoch = len & 1 ? 0 : 0xfedcba98U;
		unsigned int tag = len ? 255 : 0;
		u64 lo = payload & ((1ULL << (8 * len)) - 1);
		m = (struct pauli_mail){.id = 8, .epoch = epoch, .len = len, .tag = tag};
		memcpy(m.data, bytes, len);
		mock.reply.value = 0;
		assert(call(PAULI_MAIL_SEND, &m) == 0 && !mock.writes);
		check_wire(9, 8, epoch | (u64)len << 32 | (u64)tag << 40, lo, 0);
		for (unsigned int more = 0; more <= 1; more++) {
			memset(m.data, 0xcc, sizeof(m.data));
			m.flags = 0;
			m.len = m.tag = 0xee; /* Output fields of RECV. */
			mock.reply.value = lo | (u64)len << 48 | 1ULL << 51 |
					   (u64)more << 52 | (u64)tag << 53;
			assert(call(PAULI_MAIL_RECV, &m) == 0 && mock.writes == 1);
			check_wire(10, 8, epoch, 0, 0);
			assert(m.id == 8 && m.epoch == epoch && m.tag == tag && m.len == len);
			assert(m.flags == (PAULI_MAIL_PRESENT | (more ? PAULI_MAIL_MORE : 0)));
			for (unsigned int i = 0; i < sizeof(m.data); i++)
				assert(m.data[i] == (i < len ? bytes[i] : 0));
		}
	}
	m.flags = 0;
	mock.reply.value = 0;
	assert(call(PAULI_MAIL_RECV, &m) == 0 && !m.len && !m.flags && !m.tag);
	for (unsigned int i = 0; i < sizeof(m.data); i++)
		assert(!m.data[i]);
	for (size_t i = 0; i < sizeof(malformed)/sizeof(malformed[0]); i++) {
		mock.reply.value = malformed[i];
		assert(call(PAULI_MAIL_RECV, &m) == -EIO && mock.ecalls == 1 && !mock.writes);
	}
	mock.reply.value = 1;
	assert(call(PAULI_MAIL_SEND, &m) == -EIO && mock.ecalls == 1 && !mock.writes);
	for (size_t i = 0; i < sizeof(cmds)/sizeof(cmds[0]); i++) {
		m.id = 0;
		rejected(cmds[i], &m);
		m.id = 9;
		rejected(cmds[i], &m);
		m.id = 1; m.flags = PAULI_MAIL_PRESENT;
		rejected(cmds[i], &m);
		m.flags = 0;
	}
	for (unsigned int len = 0; len <= PAULI_MAIL_MAX; len++) {
		m.len = len;
		for (unsigned int i = len; i < sizeof(m.data); i++) {
			m.data[i] = 0xa5;
			rejected(PAULI_MAIL_SEND, &m);
			m.data[i] = 0;
		}
	}
	m.len = 7;
	rejected(PAULI_MAIL_SEND, &m);
	m.len = 255;
	rejected(PAULI_MAIL_SEND, &m);
	mock.reply.value = 0;
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
	struct pauli_log log = {.id = 2, .epoch = 7};
	struct pauli_mail mail = {.id = 2};
	struct pauli_info info;
	const struct { unsigned int cmd; void *arg; } requests[] = {
		{PAULI_START, &vm}, {PAULI_DESTROY, &vm}, {PAULI_CREATE, &create}, {PAULI_MEMORY, &mem},
		{PAULI_LOG_READ, &log}, {PAULI_MAIL_RECV, &mail}, {PAULI_MAIL_SEND, &mail},
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
		if (requests[i].cmd == PAULI_MAIL_SEND) {
			assert(call(PAULI_MAIL_SEND, &mail) == 0 && mock.ecalls == 1 && !mock.writes);
		} else {
			assert(call(requests[i].cmd, requests[i].arg) == -EFAULT);
			assert(mock.ecalls == 1 && mock.writes == 1); // Mutation may already have completed.
		}
		mock.write_fault = false;
	}
	mock.write_fault = true;
	assert(call(PAULI_INFO, &info) == -EFAULT && !mock.ecalls && mock.writes == 1);
	mock.write_fault = false;
}

static u64 stats_header_words[] = {0x5041554c49535431ULL, 2, 64, 128, 64,
				   1ULL << 63 | 1, 1ULL << 63, 0};
static unsigned int stats_owner, stats_section, stats_fail_call;
static long stats_error;
static u64 stats_hart = 0xf000000000000201ULL;

static struct sbiret stats_word(unsigned long selector)
{
	unsigned int word = mock.ecalls - 1;
	u64 prefix = 0x8001000200030000ULL;

	assert(selector == (stats_section | (u64)stats_owner << 8 | (u64)word << 16));
	assert(word < (stats_section ? 16U : 8U));
	if (mock.ecalls == stats_fail_call)
		return (struct sbiret){stats_error, 0};
	if (!stats_section)
		return (struct sbiret){0, stats_header_words[word]};
	/* Poison the redacted and reserved words too, to check driver copyout. */
	return (struct sbiret){0, word ? prefix + word : stats_hart};
}

static void stats(void)
{
	struct pauli_stats_header header;
	struct pauli_stats_owner owner;
	const u64 base = 0x8001000200030000ULL;
	const struct { unsigned int word; u64 value; } invalid[] = {
		{0, 0}, {1, 1}, {2, 128}, {3, 64}, {4, 0}, {4, 65}, {4, 63}, {7, 1}
	};

	mock.stats_reply = stats_word;
	memset(&header, 0xaa, sizeof(header));
	assert(call(PAULI_STATS_HEADER, &header) == 0 && mock.ecalls == 8 && !mock.reads);
	assert(header.version == 2 && header.host_cpus == 64);
	assert(header.sstc_owners == (1ULL << 63 | 1) && header.exclusive_owners == 1ULL << 63);
	assert(header.flags == PAULI_STATS_TRAP_REDACTED && !header.reserved);
	for (size_t i = 0; i < sizeof(invalid)/sizeof(invalid[0]); i++) {
		unsigned int word = invalid[i].word;
		u64 saved = stats_header_words[word];
		stats_header_words[word] = invalid[i].value;
		assert(call(PAULI_STATS_HEADER, &header) == -EIO && !mock.writes);
		stats_header_words[word] = saved;
	}
	/* An authorized caller can find collection disabled, despite FID 2 support. */
	stats_fail_call = 1;
	stats_error = SBI_ERR_NOT_SUPPORTED;
	assert(call(PAULI_STATS_HEADER, &header) == -EOPNOTSUPP && mock.ecalls == 1 && !mock.writes);
	stats_fail_call = 8;
	stats_error = SBI_ERR_DENIED;
	assert(call(PAULI_STATS_HEADER, &header) == -EACCES && mock.ecalls == 8 && !mock.writes);
	stats_fail_call = 0;
	mock.write_fault = true;
	assert(call(PAULI_STATS_HEADER, &header) == -EFAULT && mock.ecalls == 8 && mock.writes == 1);
	mock.write_fault = false;

	stats_section = 1;
	stats_owner = 63;
	memset(&owner, 0xcc, sizeof(owner));
	owner.owner = stats_owner;
	assert(call(PAULI_STATS_OWNER, &owner) == 0 && mock.ecalls == 16 && mock.reads == 1);
	assert(owner.owner == 63 && owner.registered == 1 && owner.hart_id == stats_hart);
	assert(owner.flags == PAULI_STATS_TRAP_REDACTED && !owner.pad && !owner.reserved);
	assert(owner.vm_exits == base + 1 && owner.wfi_exits == base + 2);
	assert(owner.sbi_timer_exits == base + 3 && owner.sstc_timer_exits == base + 4);
	assert(owner.mmio_exits == base + 5 && owner.other_exits == base + 6);
	assert(owner.vtimer_irqs == base + 7 && owner.ext_irqs == base + 8);
	assert(!owner.last_scause && !owner.last_sepc && !owner.last_stval);
	assert(owner.idle_wfi == base + 12 && owner.mail_sends == base + 13 && owner.mail_recvs == base + 14);
	stats_hart = ~0ULL;
	assert(call(PAULI_STATS_OWNER, &owner) == 0 && !owner.registered && owner.hart_id == ~0ULL);
	owner.owner = 64;
	rejected(PAULI_STATS_OWNER, &owner);
	owner.owner = ~0U;
	rejected(PAULI_STATS_OWNER, &owner);
	owner.owner = stats_owner;
	mock.read_fault = true;
	assert(call(PAULI_STATS_OWNER, &owner) == -EFAULT && !mock.ecalls && !mock.writes);
	mock.read_fault = false;
	stats_error = SBI_ERR_INVALID_PARAM; /* Below 64, beyond this host's topology. */
	stats_fail_call = 1;
	assert(call(PAULI_STATS_OWNER, &owner) == -EINVAL && mock.ecalls == 1 && !mock.writes);
	stats_error = SBI_ERR_INVALID_STATE;
	stats_fail_call = 16;
	assert(call(PAULI_STATS_OWNER, &owner) == -EAGAIN && mock.ecalls == 16 && !mock.writes);
	stats_fail_call = 0;
	mock.write_fault = true;
	assert(call(PAULI_STATS_OWNER, &owner) == -EFAULT && mock.ecalls == 16 && mock.writes == 1);
	mock.write_fault = false;
	mock.stats_reply = NULL;
}

static void optional_features(void)
{
	const struct { struct sbiret fid2; int error; } cases[] = {
		{{SBI_ERR_NOT_SUPPORTED, 0}, EOPNOTSUPP}, /* Old hypervisor. */
		{{0, 0}, EOPNOTSUPP}, {{0, 2}, EOPNOTSUPP}, {{SBI_ERR_FAILED, 0}, EIO}
	};
	struct pauli_info info;
	for (size_t i = 0; i < sizeof(cases)/sizeof(cases[0]); i++) {
		struct pauli_vm vm = {.id = 1};
		hv_exit();
		mock.reply = (struct sbiret){0, 1ULL << 32 | 8};
		mock.features = cases[i].fid2;
		assert(!hv_init() && registered);
		assert(call(PAULI_STATS_HEADER, NULL) == -cases[i].error && !mock.ecalls && !mock.writes);
		assert(call(PAULI_STATS_OWNER, NULL) == -cases[i].error && !mock.ecalls && !mock.reads);
		assert(!call(PAULI_INFO, &info) && info.version == 1 && info.max_id == 8);
		mock.reply.value = 7 | (u64)PAULI_READY << 32;
		assert(!call(PAULI_INSPECT, &vm) && vm.epoch == 7 && vm.state == PAULI_READY);
		check_wire(OP_INSPECT, 1, 0, 0, 0);
	}
}

int main(void)
{
	probe();
	permissions_and_info();
	legacy_and_destroy();
	creates();
	rejected_creates();
	memory_bytes();
	logs();
	mail();
	errors_and_copy_faults();
	stats();
	optional_features();
	hv_exit();
	assert(!registered);
	puts("PASS: Pauli driver probe, ioctl ABI, SBI words and replies, log chunks, mailbox records, stats and redaction, validation, credentials and copy faults");
	return 0;
}
