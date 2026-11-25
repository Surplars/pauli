// SPDX-License-Identifier: GPL-2.0-only
// Exercise the real parser and request path with a scripted device and monotonic clock.
#define _GNU_SOURCE
#include <assert.h>
#include <poll.h>
#include <signal.h>
#include <sys/ioctl.h>
#include <time.h>

static int fixture_ioctl(int fd, unsigned long op, void *arg);
static int fixture_now(clockid_t clock, struct timespec *now);
static int fixture_sleep(clockid_t clock, int flags, const struct timespec *at, struct timespec *left);
static int fixture_ppoll(struct pollfd *fds, nfds_t count, const struct timespec *timeout,
			 const sigset_t *sigmask);
#define ioctl fixture_ioctl
#define clock_gettime fixture_now
#define clock_nanosleep fixture_sleep
#define ppoll fixture_ppoll
#define main paulictl_main
#include "paulictl.c"
#undef main
#undef ioctl
#undef clock_gettime
#undef clock_nanosleep
#undef ppoll

_Static_assert(sizeof(struct pauli_info) == 8, "pauli_info ABI");
_Static_assert(sizeof(struct pauli_vm) == 12, "pauli_vm ABI");
_Static_assert(sizeof(struct pauli_memory) == 16, "pauli_memory ABI");
_Static_assert(sizeof(struct pauli_create) == 40, "pauli_create ABI");
_Static_assert(sizeof(struct pauli_log) == 280, "pauli_log ABI");
_Static_assert(sizeof(struct pauli_mail) == 20, "pauli_mail ABI");
_Static_assert(sizeof(struct pauli_stats_header) == 32, "pauli_stats_header ABI");
_Static_assert(sizeof(struct pauli_stats_owner) == 144, "pauli_stats_owner ABI");
_Static_assert(sizeof(struct pauli_event_info) == 16, "event info ABI");
_Static_assert(sizeof(struct pauli_event_arm) == 8, "event arm ABI");
_Static_assert(sizeof(struct pauli_events) == 8, "event read ABI");

struct exchange {
	unsigned long op;
	int error;
	int64_t timeout_ms;
	uint32_t elapsed_ms;
	short revents;
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

/* ppoll occupies a step in the same script as ioctl, checking their ordering. */
static void poll_step(int64_t timeout_ms, uint32_t elapsed_ms, short revents, int error)
{
	expect(0, NULL, NULL, error);
	struct exchange *e = &script[length - 1];
	e->timeout_ms = timeout_ms;
	e->elapsed_ms = elapsed_ms;
	e->revents = revents;
}

static int fixture_ppoll(struct pollfd *fds, nfds_t count, const struct timespec *timeout,
			 const sigset_t *sigmask)
{
	assert(count == 1 && fds->fd == 5 && fds->events == POLLIN && !sigmask);
	assert(position < length && !script[position].op);
	const struct exchange *e = &script[position++];
	if (e->timeout_ms == -1)
		assert(!timeout);
	else
		assert(timeout && timeout->tv_sec == e->timeout_ms / 1000 &&
		       timeout->tv_nsec == e->timeout_ms % 1000 * 1000000);
	clock_now = plus_ms(clock_now, e->elapsed_ms);
	if (e->error) {
		errno = e->error;
		return -1;
	}
	fds->revents = e->revents;
	return !!e->revents;
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
	unsigned char actual[4096];
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
	/* Existing wait transcripts exercise old modules. Every wait must negotiate
	 * before its first data or management ioctl, including on these modules. */
	if ((cmd.wait || cmd.follow) && (!length || script[0].op != PAULI_EVENT_INFO)) {
		assert(length < sizeof(script) / sizeof(script[0]));
		memmove(script + 1, script, length * sizeof(script[0]));
		length++;
		script[0] = (struct exchange){.op = PAULI_EVENT_INFO, .error = ENOTTY};
	}
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
		.memory_mib = 65535, .disk = 4};
	memcpy(create.name, "Ab_cd-1234567890", sizeof(create.name));
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

static void stats_tests(void)
{
	const uint32_t flag = PAULI_STATS_TRAP_REDACTED;
	struct pauli_stats_header hdr = {.version = 2, .host_cpus = 3, .sstc_owners = 5,
		.exclusive_owners = 4, .flags = flag};
	struct pauli_stats_owner first = {.owner = 0, .registered = 1, .flags = flag, .hart_id = 0,
		.vm_exits = UINT64_MAX};
	struct pauli_stats_owner hole = {.owner = 1, .flags = flag, .hart_id = UINT64_MAX};
	struct pauli_stats_owner last = {.owner = 2, .registered = 1, .flags = flag,
		.hart_id = UINT64_C(9007199254740993), .vm_exits = UINT64_MAX};
	const struct pauli_stats_owner query[3] = {{.owner = 0}, {.owner = 1}, {.owner = 2}};

#define ZERO_COUNTS " wfi_exits=0 sbi_timer_exits=0 sstc_timer_exits=0 mmio_exits=0" \
	" other_exits=0 vtimer_irqs=0 ext_irqs=0 idle_wfi=0 mail_sends=0 mail_recvs=0\n"
	expect(PAULI_STATS_HEADER, NULL, &hdr, 0);
	expect(PAULI_STATS_OWNER, &query[0], &first, 0);
	expect(PAULI_STATS_OWNER, &query[1], &hole, 0);
	expect(PAULI_STATS_OWNER, &query[2], &last, 0);
	RUN(0, "version=2 sample=live trap_redacted=1 host_cpus=3 sstc_owners=0x5 exclusive_owners=0x4\n"
	    "owner=0 registered=1 hart_id=0 vm_exits=18446744073709551615" ZERO_COUNTS
	    "owner=1 registered=0 hart_id=unregistered\n"
	    "owner=2 registered=1 hart_id=9007199254740993 vm_exits=18446744073709551615" ZERO_COUNTS
	    "totals vm_exits=36893488147419103230" ZERO_COUNTS, "stats");
#undef ZERO_COUNTS

#define ZERO_JSON ",\"wfi_exits\":\"0\",\"sbi_timer_exits\":\"0\",\"sstc_timer_exits\":\"0\"," \
	"\"mmio_exits\":\"0\",\"other_exits\":\"0\",\"vtimer_irqs\":\"0\",\"ext_irqs\":\"0\"," \
	"\"idle_wfi\":\"0\",\"mail_sends\":\"0\",\"mail_recvs\":\"0\""
#define JSON_HEADER "{\"version\":2,\"sample\":\"live\",\"trap_redacted\":true," \
	"\"host_cpus\":3,\"sstc_owners\":\"0x5\",\"exclusive_owners\":\"0x4\",\"owners\":["
	expect(PAULI_STATS_HEADER, NULL, &hdr, 0);
	expect(PAULI_STATS_OWNER, &query[1], &hole, 0);
	RUN(0, JSON_HEADER "{\"owner\":1,\"registered\":false,\"hart_id\":null}],"
	    "\"totals\":{\"vm_exits\":\"0\"" ZERO_JSON "}}\n", "stats", "--owner", "1", "--json");
	expect(PAULI_STATS_HEADER, NULL, &hdr, 0);
	expect(PAULI_STATS_OWNER, &query[2], &last, 0);
	RUN(0, JSON_HEADER "{\"owner\":2,\"registered\":true,\"hart_id\":\"9007199254740993\","
	    "\"vm_exits\":\"18446744073709551615\"" ZERO_JSON "}],"
	    "\"totals\":{\"vm_exits\":\"18446744073709551615\"" ZERO_JSON "}}\n",
	    "stats", "--json", "--owner", "2");
#undef JSON_HEADER
#undef ZERO_JSON

	expect(PAULI_STATS_HEADER, NULL, &hdr, 0);
	RUN(EINVAL, "", "stats", "--owner", "63");
	for (size_t i = 0; i < 3; i++) {
		const int errors[] = {ENOTTY, EACCES, EOPNOTSUPP};
		expect(PAULI_STATS_HEADER, NULL, NULL, errors[i]);
		RUN(errors[i], "", "stats");
	}
	expect(PAULI_STATS_HEADER, NULL, &hdr, 0);
	expect(PAULI_STATS_OWNER, &query[0], &first, 0);
	expect(PAULI_STATS_OWNER, &query[1], NULL, EACCES);
	RUN(EACCES, "", "stats", "--json");
	first.last_sepc = 1;
	expect(PAULI_STATS_HEADER, NULL, &hdr, 0);
	expect(PAULI_STATS_OWNER, &query[0], &first, 0);
	RUN(EPROTO, "", "stats", "--owner", "0");
	hdr.host_cpus = 65;
	expect(PAULI_STATS_HEADER, NULL, &hdr, 0);
	RUN(EPROTO, "", "stats");
	INVALID("stats", "service");
	INVALID("stats", "--epoch", "1");
	INVALID("stats", "--owner");
	INVALID("stats", "--owner", "64");
	INVALID("stats", "--owner", "-1");
	INVALID("stats", "--owner", "0", "--owner", "0");
	INVALID("stats", "--json", "--json");
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

static void send_mail_tests(void)
{
	struct pauli_vm peer = observed(2, 9, PAULI_READY);
	struct pauli_mail msg = {.id = 2, .epoch = 9, .tag = 42, .len = 6, .data = "ABCdef"};
	inspection(2, &peer, 0);
	expect(PAULI_MAIL_SEND, &msg, NULL, 0);
	RUN(0, "", "send", "service", "--tag", "42", "ABCdef");

	msg = (struct pauli_mail){.id = 2, .tag = 255, .len = 6, .data = {0, 255, 0x0a, 0xab, 0xcd, 0x80}};
	expect(PAULI_MAIL_SEND, &msg, NULL, 0);
	RUN(0, "", "send", "2", "--epoch", "0", "--tag", "255", "--hex", "00Ff0aaBCd80");
	msg = (struct pauli_mail){.id = 2, .epoch = UINT32_MAX};
	expect(PAULI_MAIL_SEND, &msg, NULL, 0);
	RUN(0, "", "send", "2", "--epoch", "4294967295");
	expect(PAULI_MAIL_SEND, &msg, NULL, 0);
	RUN(0, "", "send", "2", "--hex", "", "--epoch", "4294967295");
	expect(PAULI_MAIL_SEND, &msg, NULL, EAGAIN);
	RUN(EAGAIN, "", "send", "2", "", "--epoch", "4294967295");

	msg = (struct pauli_mail){.id = 2, .len = 5, .data = "--tag"};
	expect(PAULI_MAIL_SEND, &msg, NULL, 0);
	RUN(0, "", "send", "2", "--epoch", "0", "--", "--tag");
	msg = (struct pauli_mail){.id = 2};
	for (size_t i = 0; i < 3; i++) {
		const int errors[] = {EFAULT, ESTALE, EACCES};
		expect(PAULI_MAIL_SEND, &msg, NULL, errors[i]);
		RUN(errors[i], "", "send", "2", "--epoch", "0");
	}
	inspection(2, NULL, EACCES);
	RUN(EACCES, "", "send", "service", "x");
}

static void recv_mail_tests(void)
{
	struct pauli_vm peer = observed(2, 9, PAULI_STOPPED);
	struct pauli_mail in = {.id = 2, .epoch = 9};
	inspection(2, &peer, 0);
	expect(PAULI_MAIL_RECV, &in, &in, 0);
	RUN(0, "empty=1\n", "recv", "service");
	expect(PAULI_MAIL_RECV, &in, &in, 0);
	RUN(0, "", "recv", "service", "--epoch", "9", "--raw");

	struct pauli_mail out = {.id = 2, .epoch = 9, .tag = 255, .len = 6,
		.flags = PAULI_MAIL_PRESENT | PAULI_MAIL_MORE, .data = {0, 255, 0x0a, 0x7e, 0x80, 0xab}};
	expect(PAULI_MAIL_RECV, &in, &out, 0);
	RUN(0, "vm=2 epoch=9 tag=255 len=6 more=1 hex=00ff0a7e80ab\n", "recv", "2", "--epoch", "9");
	expect(PAULI_MAIL_RECV, &in, &out, 0);
	RUN_BYTES(0, out.data, out.len, "", "recv", "2", "--raw", "--epoch", "9");
	in.epoch = out.epoch = 0;
	out.flags = PAULI_MAIL_PRESENT;
	sleeps = 0;
	expect(PAULI_MAIL_RECV, &in, &in, 0);
	expect(PAULI_MAIL_RECV, &in, &out, 0);
	RUN(0, "vm=2 epoch=0 tag=255 len=6 more=0 hex=00ff0a7e80ab\n",
	    "recv", "service", "--wait", "--epoch", "0");
	assert(sleeps == 1);
	out = (struct pauli_mail){.id = 2, .tag = 7, .flags = PAULI_MAIL_PRESENT};
	expect(PAULI_MAIL_RECV, &in, &out, 0);
	RUN(0, "vm=2 epoch=0 tag=7 len=0 more=0 hex=\n",
	    "recv", "2", "--epoch", "0", "--wait", "--timeout-ms", "0");
	expect(PAULI_MAIL_RECV, &in, &out, 0);
	RUN(0, "", "recv", "2", "--epoch", "0", "--raw", "--wait");
	assert(sleeps == 1);

	clock_now = (struct timespec){.tv_sec = 5, .tv_nsec = 990000000L};
	sleeps = 0;
	for (int i = 0; i < 2; i++)
		expect(PAULI_MAIL_RECV, &in, &in, 0);
	RUN(ETIMEDOUT, "", "recv", "2", "--epoch", "0", "--wait", "--timeout-ms", "51");
	assert(sleeps == 2 && clock_now.tv_sec == 6 && clock_now.tv_nsec == 41000000L);
	expect(PAULI_MAIL_RECV, &in, &in, 0);
	RUN(ETIMEDOUT, "", "recv", "2", "--epoch", "0", "--wait", "--timeout-ms", "0");
	assert(sleeps == 2);

	in.epoch = 9;
	inspection(2, &peer, 0);
	expect(PAULI_MAIL_RECV, &in, &in, 0);
	expect(PAULI_MAIL_RECV, &in, NULL, EAGAIN);
	RUN(EAGAIN, "", "recv", "2", "--wait");
	const int errors[] = {ESTALE, EACCES, EFAULT, EINTR};
	for (size_t i = 0; i < sizeof(errors) / sizeof(errors[0]); i++) {
		expect(PAULI_MAIL_RECV, &in, NULL, errors[i]);
		RUN(errors[i], "", "recv", "2", "--epoch", "9", "--wait");
	}
	// Malformed replies cannot cause a duplicate dequeue or emit a partial record.
	expect(PAULI_MAIL_RECV, &in, &out, 0); // Wrong epoch.
	RUN(ESTALE, "", "recv", "2", "--epoch", "9", "--wait");
	out.epoch = 9;
	out.len = PAULI_MAIL_MAX + 1;
	expect(PAULI_MAIL_RECV, &in, &out, 0);
	RUN(EPROTO, "", "recv", "2", "--epoch", "9", "--raw");
	out = in;
	out.flags = PAULI_MAIL_MORE;
	expect(PAULI_MAIL_RECV, &in, &out, 0);
	RUN(EPROTO, "", "recv", "2", "--epoch", "9", "--wait");
}

static void event_info(uint32_t features, uint32_t flags, uint32_t supported)
{
	const struct pauli_event_info info = {.features = features, .flags = flags, .supported = supported};
	expect(PAULI_EVENT_INFO, NULL, &info, 0);
}

static void subscribe(uint32_t mask, int error)
{
	event_info(PAULI_EVENT_FEAT, PAULI_EVENT_DT | PAULI_EVENT_READY,
		   PAULI_EVENT_MAIL | PAULI_EVENT_LOG | PAULI_EVENT_STATE);
	expect(PAULI_EVENT_ARM, &(struct pauli_event_arm){.mask = mask}, NULL, error);
}

static void consume(uint32_t reasons, int error)
{
	expect(PAULI_EVENT_READ, NULL, &(struct pauli_events){.reasons = reasons}, error);
}

static void active_receive(void)
{
	const struct pauli_mail in = {.id = 2};
	const struct pauli_mail message = {.id = 2, .tag = 7, .flags = PAULI_MAIL_PRESENT};
	clock_now = (struct timespec){.tv_nsec = 980000000L};
	sleeps = 0;
	/* EINTR and a wake with no generation leave the data ioctl untouched.
	 * RESCAN is enough to recheck, even if this particular peer is still empty. */
	subscribe(PAULI_EVENT_MAIL, 0);
	expect(PAULI_MAIL_RECV, &in, &in, 0);
	poll_step(1000, 41, 0, EINTR);
	poll_step(959, 50, POLLIN, 0);
	consume(0, 0);
	poll_step(909, 100, POLLIN, 0);
	consume(PAULI_EVENT_RESCAN, 0);
	expect(PAULI_MAIL_RECV, &in, &in, 0);
	poll_step(809, 700, POLLIN, 0);
	consume(PAULI_EVENT_MAIL | PAULI_EVENT_RESCAN, 0);
	expect(PAULI_MAIL_RECV, &in, &message, 0);
	RUN(0, "vm=2 epoch=0 tag=7 len=0 more=0 hex=\n",
	    "recv", "2", "--epoch", "0", "--wait", "--timeout-ms", "1000");

	subscribe(PAULI_EVENT_MAIL, 0);
	expect(PAULI_MAIL_RECV, &in, &message, 0);
	RUN(0, "", "recv", "2", "--epoch", "0", "--wait", "--timeout-ms", "0", "--raw");
	/* A quiet fd blocks for the entire timeout, including values above INT_MAX. */
	subscribe(PAULI_EVENT_MAIL, 0);
	expect(PAULI_MAIL_RECV, &in, &in, 0);
	poll_step(UINT32_MAX, UINT32_MAX, 0, 0);
	RUN(ETIMEDOUT, "", "recv", "2", "--epoch", "0", "--wait", "--timeout-ms", "4294967295");
	/* A failed dequeue may already have consumed a message. */
	const int errors[] = {EACCES, EFAULT, EINTR};
	for (size_t i = 0; i < sizeof(errors) / sizeof(errors[0]); i++) {
		subscribe(PAULI_EVENT_MAIL, 0);
		expect(PAULI_MAIL_RECV, &in, &in, 0);
		poll_step(DEFAULT_TIMEOUT_MS, 0, POLLIN, 0);
		consume(PAULI_EVENT_MAIL, 0);
		expect(PAULI_MAIL_RECV, &in, NULL, errors[i]);
		RUN(errors[i], "", "recv", "2", "--epoch", "0", "--wait");
	}
	assert(!sleeps);
}

static void active_logs(void)
{
	const struct pauli_vm peer = observed(2, 9, PAULI_RUNNING);
	const struct pauli_log lost = {.id = 2, .epoch = 9, .cursor = 6, .flags = PAULI_LOG_OVERRUN};
	const unsigned char bytes[] = {0, 255, 'a'};
	sleeps = 0;
	subscribe(PAULI_EVENT_LOG, 0);
	inspection(2, &peer, 0);
	log_chunk(0, NULL, 0);
	poll_step(-1, 5000, POLLIN, 0);
	consume(PAULI_EVENT_LOG | PAULI_EVENT_RESCAN, 0);
	log_read(0, &lost, 0);
	log_chunk(6, bytes, sizeof(bytes));
	log_chunk(9, NULL, 0);
	poll_step(-1, 10000, POLLIN, 0);
	consume(PAULI_EVENT_RESCAN, 0);
	log_read(9, NULL, EACCES);
	RUN_BYTES(EACCES, bytes, sizeof(bytes), "paulictl: log overrun at cursor 0, resuming at 6\n",
		  "logs", "service", "--follow");
	subscribe(PAULI_EVENT_LOG, 0);
	log_chunk(0, NULL, 0);
	poll_step(55, 55, 0, EINTR);
	RUN(ETIMEDOUT, "", "logs", "2", "--follow", "--epoch", "9", "--timeout-ms", "55");
	assert(!sleeps);
}

static void active_stop(void)
{
	const struct pauli_vm bound = {.id = 2, .epoch = 9};
	struct pauli_vm stopping = observed(2, 9, PAULI_STOPPING);
	struct pauli_vm terminal = observed(2, 9, PAULI_STOPPED);
	stopping.changed = 1;
	sleeps = 0;
	/* Arm before the initial epoch inspection and the single STOP. */
	subscribe(PAULI_EVENT_STATE, 0);
	inspection(2, &stopping, 0);
	expect(PAULI_STOP, &bound, &stopping, 0);
	inspection(2, &stopping, 0);
	poll_step(DEFAULT_TIMEOUT_MS, 5000, POLLIN, 0);
	consume(PAULI_EVENT_STATE, 0);
	inspection(2, &terminal, 0);
	RUN(0, "vm=2 epoch=9 state=stopped vcpus=1 active=0 changed=1\n", "stop", "service", "--wait");
	terminal.epoch++;
	subscribe(PAULI_EVENT_STATE, 0);
	expect(PAULI_STOP, &bound, &stopping, 0);
	inspection(2, &stopping, 0);
	poll_step(DEFAULT_TIMEOUT_MS, 0, POLLIN, 0);
	consume(PAULI_EVENT_RESCAN, 0);
	inspection(2, &terminal, 0);
	RUN(ESTALE, "", "stop", "2", "--epoch", "9", "--wait");
	subscribe(PAULI_EVENT_STATE, 0);
	expect(PAULI_STOP, &bound, NULL, EINTR);
	RUN(EINTR, "", "stop", "2", "--epoch", "9", "--wait");
	assert(!sleeps);
}

static void negotiation_and_removal(void)
{
	const struct pauli_mail in = {.id = 2};
	/* An old module, feature-less host, missing IRQ, or unsupported event
	 * class retains the legacy timed check. */
	for (int i = 0; i < 5; i++) {
		switch (i) {
		case 0: expect(PAULI_EVENT_INFO, NULL, NULL, EOPNOTSUPP); break;
		case 1: event_info(0, 0, 0); break;
		case 2: event_info(PAULI_EVENT_FEAT, PAULI_EVENT_DT, 0); break;
		case 3: event_info(PAULI_EVENT_FEAT, PAULI_EVENT_DT | PAULI_EVENT_READY,
				   PAULI_EVENT_MAIL); break;
		case 4: subscribe(PAULI_EVENT_LOG, EOPNOTSUPP); break;
		}
		sleeps = 0;
		log_chunk(0, NULL, 0);
		RUN(ETIMEDOUT, "", "logs", "2", "--epoch", "9", "--follow", "--timeout-ms", "10");
		assert(sleeps == 1);
	}
	sleeps = 0;
	expect(PAULI_EVENT_INFO, NULL, NULL, EACCES);
	RUN(EACCES, "", "recv", "2", "--wait");
	subscribe(PAULI_EVENT_STATE, EPERM);
	RUN(EPERM, "", "stop", "2", "--wait");
	subscribe(PAULI_EVENT_MAIL, 0);
	expect(PAULI_MAIL_RECV, &in, &in, 0);
	poll_step(DEFAULT_TIMEOUT_MS, 1, POLLERR, 0);
	consume(0, EACCES);
	RUN(EACCES, "", "recv", "2", "--wait", "--epoch", "0");
	subscribe(PAULI_EVENT_LOG, 0);
	log_chunk(0, NULL, 0);
	poll_step(-1, 1, POLLHUP | POLLIN, 0);
	consume(0, ENODEV);
	RUN(ENODEV, "", "logs", "2", "--follow", "--epoch", "9");
	subscribe(PAULI_EVENT_LOG, 0);
	log_chunk(0, NULL, 0);
	poll_step(-1, 0, POLLIN, 0);
	consume(0, EOPNOTSUPP);
	RUN(EOPNOTSUPP, "", "logs", "2", "--follow", "--epoch", "9");
	assert(!sleeps);
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
	INVALID("send", "service", "--tag", "-1");
	INVALID("send", "service", "--tag", "256");
	INVALID("send", "service", "--tag", "0", "--tag", "0");
	INVALID("send", "2", "--hex");
	INVALID("send", "2", "--hex", "0");
	INVALID("send", "2", "--hex", "0g");
	INVALID("send", "2", "--hex", "00000000000000");
	INVALID("send", "2", "--hex", "", "--hex", "");
	INVALID("send", "2", "--hex", "", "text");
	INVALID("send", "2", "", "--hex", "01");
	INVALID("send", "2", "", "extra");
	INVALID("send", "2", "toolong");
	INVALID("send", "2", "--epoch", "0", "--epoch", "1");
	INVALID("send", "2", "--unknown");
	INVALID("send", "2", "--wait");
	INVALID("recv", "2", "--timeout-ms", "4");
	INVALID("recv", "2", "--tag", "4");
	INVALID("recv", "2", "--raw", "--raw");
	INVALID("recv", "2", "abc");
	INVALID("logs", "2", "--epoch", "0");
	legacy_and_list();
	stats_tests();
	provision();
	stop_wait();
	logs();
	send_mail_tests();
	recv_mail_tests();
	active_receive();
	active_logs();
	active_stop();
	negotiation_and_removal();
	puts("PASS: legacy output, visible list, epoch binding, create/disk shapes, exact memory, stop waits and no mutation retry");
	puts("PASS: binary logs, cursor recovery, epoch changes and bounded follow");
	puts("PASS: live stats, redaction, sparse owners, lossless JSON, wide totals and compatibility errors");
	puts("PASS: tagged text/hex mail, bound and inspected epochs, raw/structured receive, bounded wait and no duplicate dequeue");
	puts("PASS: class-specific fd waits, immutable deadlines, quiet and coalesced hints, legacy fallback and terminal errors");
	return 0;
}
