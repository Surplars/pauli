// SPDX-License-Identifier: GPL-2.0-only
// Exercise the real parser and request path with a scripted device and monotonic clock.
#include <assert.h>
#include <sys/ioctl.h>
#include <time.h>

static int fixture_ioctl(int fd, unsigned long op, void *arg);
static int fixture_now(clockid_t clock, struct timespec *now);
static int fixture_sleep(clockid_t clock, int flags, const struct timespec *at, struct timespec *left);
#define ioctl fixture_ioctl
#define clock_gettime fixture_now
#define clock_nanosleep fixture_sleep
#define main paulictl_main
#include "paulictl.c"
#undef main
#undef ioctl
#undef clock_gettime
#undef clock_nanosleep

_Static_assert(sizeof(struct pauli_info) == 8, "pauli_info ABI");
_Static_assert(sizeof(struct pauli_vm) == 12, "pauli_vm ABI");
_Static_assert(sizeof(struct pauli_memory) == 16, "pauli_memory ABI");
_Static_assert(sizeof(struct pauli_create) == 40, "pauli_create ABI");
_Static_assert(sizeof(struct pauli_log) == 280, "pauli_log ABI");

struct exchange {
	unsigned long op;
	int error;
	unsigned char in[sizeof(struct pauli_log)], out[sizeof(struct pauli_log)];
};
static struct exchange script[16];
static size_t length, position;
static struct timespec clock_now;
static unsigned int sleeps;

static void expect(unsigned long op, const void *in, const void *out, int error)
{
	size_t size = _IOC_SIZE(op);
	assert(size <= sizeof(script[0].in) && length < sizeof(script) / sizeof(script[0]));
	struct exchange *e = &script[length++];
	*e = (struct exchange){.op = op, .error = error};
	if (in)
		memcpy(e->in, in, size);
	if (out)
		memcpy(e->out, out, size);
}

static int fixture_ioctl(int fd, unsigned long op, void *arg)
{
	assert(fd == 5 && position < length);
	const struct exchange *e = &script[position++];
	assert(e->op == op && !memcmp(e->in, arg, _IOC_SIZE(op)));
	if (e->error) {
		errno = e->error;
		return -1;
	}
	memcpy(arg, e->out, _IOC_SIZE(op));
	return 0;
}

static int fixture_now(clockid_t clock, struct timespec *now)
{
	assert(clock == CLOCK_MONOTONIC);
	*now = clock_now;
	return 0;
}

static int fixture_sleep(clockid_t clock, int flags, const struct timespec *at, struct timespec *left)
{
	assert(clock == CLOCK_MONOTONIC && flags == TIMER_ABSTIME && !left);
	assert(before(clock_now, *at));
	clock_now = *at;
	sleeps++;
	return 0;
}

static void check_stream(FILE *stream, const void *expected, size_t len)
{
	unsigned char actual[512];
	rewind(stream);
	assert(len < sizeof(actual));
	assert(fread(actual, 1, sizeof(actual), stream) == len && feof(stream));
	assert(!memcmp(actual, expected, len));
}

static void exercise(int argc, char **argv, int error, const void *output, size_t len,
		     const char *diagnostic)
{
	struct command cmd;
	assert(parse_command(argc, argv, &cmd));
	FILE *capture = tmpfile(), *notices = tmpfile();
	assert(capture && notices);
	fflush(stdout);
	fflush(stderr);
	int saved = dup(STDOUT_FILENO);
	int saved_err = dup(STDERR_FILENO);
	assert(saved >= 0 && dup2(fileno(capture), STDOUT_FILENO) >= 0);
	assert(saved_err >= 0 && dup2(fileno(notices), STDERR_FILENO) >= 0);
	int rc = request(5, &cmd);
	int last_errno = errno;
	fflush(stdout);
	fflush(stderr);
	assert(dup2(saved, STDOUT_FILENO) >= 0);
	assert(dup2(saved_err, STDERR_FILENO) >= 0);
	close(saved);
	close(saved_err);
	assert(error ? rc == -1 && last_errno == error : rc == 0);
	assert(position == length);
	check_stream(capture, output, len);
	check_stream(notices, diagnostic, strlen(diagnostic));
	fclose(capture);
	fclose(notices);
	position = length = 0;
}

#define RUN_BYTES(error, bytes, size, diagnostic, ...) do { \
	char *args[] = {"paulictl", __VA_ARGS__}; \
	exercise(sizeof(args)/sizeof(args[0]), args, error, bytes, size, diagnostic); \
} while (0)
#define RUN(error, output, ...) RUN_BYTES(error, output, strlen(output), "", __VA_ARGS__)
#define INVALID(...) do { \
	char *args[] = {"paulictl", __VA_ARGS__}; struct command cmd; \
	assert(!parse_command(sizeof(args)/sizeof(args[0]), args, &cmd)); \
} while (0)

static struct pauli_vm observed(uint32_t id, uint32_t epoch, unsigned int state)
{
	return (struct pauli_vm){.id = id, .epoch = epoch, .state = state, .vcpus = 1};
}

static void inspection(uint32_t id, const struct pauli_vm *out, int error)
{
	expect(PAULI_INSPECT, &(struct pauli_vm){.id = id}, out, error);
}

static void legacy_and_list(void)
{
	struct pauli_info info = {.version = 1, .max_id = 4};
	struct pauli_vm old = observed(2, 42, PAULI_STOPPED), started = observed(2, 43, PAULI_READY);
	struct pauli_vm bound = {.id = 2, .epoch = 42};
	started.changed = 1;
	expect(PAULI_INFO, NULL, &info, 0);
	RUN(0, "abi=1 max_id=4\n", "info");
	inspection(2, &old, 0);
	RUN(0, "vm=2 epoch=42 state=stopped vcpus=1 active=0\n", "status", "service");
	inspection(2, &old, 0);
	expect(PAULI_START, &bound, &started, 0);
	RUN(0, "vm=2 epoch=43 state=ready vcpus=1 active=0 changed=1\n", "start", "service");
	expect(PAULI_START, &bound, &started, 0);
	RUN(0, "vm=2 epoch=43 state=ready vcpus=1 active=0 changed=1\n",
	    "start", "service", "--epoch", "42");
	inspection(2, &old, 0);
	expect(PAULI_STOP, &bound, &old, 0);
	RUN(0, "vm=2 epoch=42 state=stopped vcpus=1 active=0 changed=0\n", "stop", "2");
	expect(PAULI_DESTROY, &bound, &old, 0);
	RUN(0, "vm=2 epoch=42 state=stopped vcpus=1 active=0 changed=0\n", "destroy", "service", "--epoch", "42");
	inspection(2, &old, 0);
	expect(PAULI_DESTROY, &bound, &old, 0);
	RUN(0, "vm=2 epoch=42 state=stopped vcpus=1 active=0 changed=0\n", "destroy", "service");
	struct pauli_vm last = observed(4, 77, PAULI_RUNNING);
	last.active = 1;
	expect(PAULI_INFO, NULL, &info, 0);
	inspection(1, NULL, EACCES);
	inspection(2, &old, 0);
	inspection(3, NULL, EINVAL);
	inspection(4, &last, 0);
	RUN(0, "vm=2 epoch=42 state=stopped vcpus=1 active=0\n"
	       "vm=4 epoch=77 state=running vcpus=1 active=1\n", "list");
	info.max_id = UINT32_MAX;
	expect(PAULI_INFO, NULL, &info, 0);
	inspection(1, NULL, ENOENT);
	RUN(ENOENT, "", "list");
}

static void provision(void)
{
	struct pauli_vm old = observed(2, 9, PAULI_STOPPED);
	struct pauli_create create = {.template_id = 2, .epoch = 9, .name = "child"};
	struct pauli_create child = create;
	child.id = 3;
	child.child_epoch = 21;
	inspection(2, &old, 0);
	expect(PAULI_CREATE, &create, &child, 0);
	RUN(0, "vm=3 epoch=21 template=2 name=child\n", "create", "service", "child");

	create = (struct pauli_create){.template_id = 2, .epoch = 9, .vcpus = 255,
		.memory_mib = 65535, .disk = 4, .name = "Ab_cd-1234567890"};
	expect(PAULI_CREATE, &create, &child, 0);
	RUN(0, "vm=3 epoch=21 template=2 name=Ab_cd-1234567890\n", "create", "2", "Ab_cd-1234567890",
	    "--disk", "3", "--epoch", "9", "--memory-mib", "65535", "--vcpus", "255");
	create = (struct pauli_create){.template_id = 2, .epoch = 9, .disk = 1, .name = "D"};
	expect(PAULI_CREATE, &create, NULL, EFAULT);
	RUN(EFAULT, "", "create", "2", "D", "--epoch", "9", "--disk", "0");

	struct pauli_memory memory = {.id = 2, .epoch = 9}, reply = memory;
	reply.bytes = UINT64_MAX;
	inspection(2, &old, 0);
	expect(PAULI_MEMORY, &memory, &reply, 0);
	RUN(0, "vm=2 epoch=9 memory_bytes=18446744073709551615\n", "memory", "service");
	expect(PAULI_MEMORY, &memory, NULL, EAGAIN);
	RUN(EAGAIN, "", "memory", "service", "--epoch", "9");
}

static void stop_wait(void)
{
	struct pauli_vm bound = {.id = 2, .epoch = 9};
	struct pauli_vm stopping = observed(2, 9, PAULI_STOPPING), terminal = observed(2, 9, PAULI_FAULTED);
	stopping.changed = 1;
	clock_now = (struct timespec){.tv_sec = 5, .tv_nsec = 990000000L};
	sleeps = 0;
	expect(PAULI_STOP, &bound, &stopping, 0);
	inspection(2, &stopping, 0);
	inspection(2, &terminal, 0);
	RUN(0, "vm=2 epoch=9 state=faulted vcpus=1 active=0 changed=1\n",
	    "stop", "2", "--epoch", "9", "--wait");
	assert(sleeps == 1 && clock_now.tv_sec == 6 && clock_now.tv_nsec == 40000000L);
	terminal.state = PAULI_STOPPED;
	inspection(2, &stopping, 0);
	expect(PAULI_STOP, &bound, &stopping, 0);
	inspection(2, &terminal, 0);
	RUN(0, "vm=2 epoch=9 state=stopped vcpus=1 active=0 changed=1\n",
	    "stop", "service", "--wait", "--timeout-ms", "0");
	sleeps = 0;
	expect(PAULI_STOP, &bound, &stopping, 0);
	inspection(2, &stopping, 0);
	inspection(2, &stopping, 0);
	inspection(2, &stopping, 0);
	RUN(ETIMEDOUT, "", "stop", "2", "--timeout-ms", "51", "--wait", "--epoch", "9");
	assert(sleeps == 2 && clock_now.tv_sec == 6 && clock_now.tv_nsec == 91000000L);
	terminal.epoch++;
	expect(PAULI_STOP, &bound, &stopping, 0);
	inspection(2, &terminal, 0);
	RUN(ESTALE, "", "stop", "2", "--epoch", "9", "--wait");
	expect(PAULI_STOP, &bound, NULL, EFAULT);
	RUN(EFAULT, "", "stop", "2", "--epoch", "9", "--wait");
	inspection(2, &stopping, 0);
	expect(PAULI_STOP, &bound, NULL, EAGAIN);
	RUN(EAGAIN, "", "stop", "2");
	inspection(2, NULL, EACCES);
	RUN(EACCES, "", "destroy", "2");
}

static void log_read(uint64_t cursor, const struct pauli_log *reply, int error)
{
	struct pauli_log in = {.id = 2, .epoch = 9, .cursor = cursor};
	expect(PAULI_LOG_READ, &in, reply, error);
}

static void log_chunk(uint64_t cursor, const void *data, size_t len)
{
	struct pauli_log out = {.id = 2, .epoch = 9, .cursor = cursor + len, .len = len};
	assert(len <= sizeof(out.data));
	if (len)
		memcpy(out.data, data, len);
	log_read(cursor, &out, 0);
}

static void logs(void)
{
	struct pauli_vm vm = observed(2, 9, PAULI_FAULTED);
	uint8_t bytes[PAULI_LOG_CHUNK + 4];
	for (size_t i = 0; i < sizeof(bytes); i++)
		bytes[i] = (uint8_t)i;
	sleeps = 0;
	inspection(2, &vm, 0);
	log_chunk(0, bytes, PAULI_LOG_CHUNK);
	log_chunk(PAULI_LOG_CHUNK, bytes + PAULI_LOG_CHUNK, 4);
	log_chunk(sizeof(bytes), NULL, 0);
	RUN_BYTES(0, bytes, sizeof(bytes), "", "logs", "service");
	assert(!sleeps);

	uint64_t resume = UINT64_C(4294967309);
	struct pauli_log lost = {.id = 2, .epoch = 9, .cursor = resume, .flags = PAULI_LOG_OVERRUN};
	const unsigned char recovered[] = {'X', 0, 255, '\n'};
	log_chunk(4, recovered, 1);
	log_read(5, &lost, 0);
	log_chunk(resume, recovered + 1, 3);
	log_chunk(resume + 3, NULL, 0);
	RUN_BYTES(0, recovered, sizeof(recovered),
		  "paulictl: log overrun at cursor 5, resuming at 4294967309\n",
		  "logs", "2", "--cursor", "4", "--epoch", "9");
	assert(!sleeps);

	// Poll at EOF, stream new bytes, and stop on reincarnation without inspecting again.
	log_chunk(0, NULL, 0);
	log_chunk(0, bytes, 3);
	log_read(3, NULL, EAGAIN);
	RUN_BYTES(EAGAIN, bytes, 3, "", "logs", "service", "--follow", "--epoch", "9");
	assert(sleeps == 1);
	log_read(UINT64_MAX, NULL, ESTALE);
	RUN(ESTALE, "", "logs", "2", "--epoch", "9", "--cursor", "18446744073709551615");
	lost.epoch++;
	log_read(0, &lost, 0);
	RUN(ESTALE, "", "logs", "2", "--epoch", "9");
	inspection(2, NULL, EACCES);
	RUN(EACCES, "", "logs", "service");
	log_read(0, NULL, EACCES);
	RUN(EACCES, "", "logs", "2", "--epoch", "9");

	sleeps = 0;
	clock_now = (struct timespec){.tv_sec = 6, .tv_nsec = 990000000L};
	for (int i = 0; i < 2; i++) /* No additional ioctl at the 51 ms deadline. */
		log_chunk(0, NULL, 0);
	RUN(ETIMEDOUT, "", "logs", "service", "--timeout-ms", "51", "--epoch", "9", "--follow");
	assert(sleeps == 2 && clock_now.tv_sec == 7 && clock_now.tv_nsec == 41000000L);
	log_chunk(0, bytes, 3);
	RUN_BYTES(ETIMEDOUT, bytes, 3, "", "logs", "2", "--follow", "--epoch", "9", "--timeout-ms", "0");
	assert(sleeps == 2);

	lost.epoch = 9;
	log_read(resume, &lost, 0); // an overrun must advance the cursor
	RUN(EPROTO, "", "logs", "2", "--epoch", "9", "--cursor", "4294967309");
	struct pauli_log oversized = {.id = 2, .epoch = 9, .len = 257, .cursor = 257};
	log_read(0, &oversized, 0);
	RUN(EPROTO, "", "logs", "2", "--epoch", "9");
}

int main(void)
{
	assert(target_id("service") == 2 && target_id("4294967295") == UINT32_MAX);
	assert(!target_id("") && !target_id("4294967296") && !target_id("-1"));
	assert(!target_id("+2") && !target_id(" 2") && !target_id("0"));
	INVALID("list", "3");
	INVALID("status", "2", "--epoch", "3");
	INVALID("start", "2", "--wait");
	INVALID("stop", "2", "--timeout-ms", "300");
	INVALID("stop", "2", "--wait", "--timeout-ms", "4294967296");
	INVALID("stop", "2", "--wait", "--wait");
	INVALID("start", "2", "--epoch", "0");
	INVALID("start", "2", "--epoch", "9", "--epoch", "9");
	INVALID("create", "2", "");
	INVALID("create", "2", "0123");
	INVALID("create", "2", "a/b");
	INVALID("create", "2", "abcdefghijklmnopq");
	INVALID("create", "2", "\xc3\xa9");
	INVALID("create", "2", "child", "--vcpus", "256");
	INVALID("create", "2", "child", "--memory-mib", "65536");
	INVALID("create", "2", "child", "--disk", "4");
	INVALID("create", "2", "child", "--flags", "1");
	INVALID("create", "2", "child", "--vcpus", "0");
	INVALID("create", "2", "child", "--disk", "0", "--disk", "1");
	INVALID("memory", "service", "--epoch");
	INVALID("logs", "service", "--cursor", "18446744073709551616");
	INVALID("logs", "service", "--cursor", "-1");
	INVALID("logs", "service", "--cursor", "0", "--cursor", "0");
	INVALID("logs", "service", "--follow", "--follow");
	INVALID("logs", "service", "--timeout-ms", "1");
	INVALID("logs", "service", "--follow", "--timeout-ms", "1", "--timeout-ms", "2");
	INVALID("logs", "service", "--wait");
	INVALID("memory", "service", "--follow");
	INVALID("stop", "service", "--cursor", "0");
	legacy_and_list();
	provision();
	stop_wait();
	logs();
	puts("PASS: legacy output, visible list, epoch binding, create/disk shapes, exact memory, stop waits and no mutation retry");
	puts("PASS: binary logs, cursor recovery, epoch changes and bounded follow");
	return 0;
}
