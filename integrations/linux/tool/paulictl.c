// SPDX-License-Identifier: GPL-2.0-only
// Userspace never issues SBI ecalls: the guest kernel owns the VS/HS boundary.
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

#include <linux/pauli.h>

#define DEFAULT_TIMEOUT_MS 30000
#define POLL_MS 50
#define LIST_COMMAND 1UL /* local command, never submitted as an ioctl */
#define STATS_MAX_OWNERS 64

struct command {
	unsigned long op;
	uint32_t id, epoch, timeout_ms;
	uint64_t cursor;
	bool wait, follow, raw, has_epoch, has_timeout;
	bool json, has_owner;
	uint32_t owner;
	struct pauli_create create;
	struct pauli_mail mail;
};

static void usage(FILE *out)
{
	fputs("Usage: paulictl info | list | status TARGET\n"
	      "       paulictl stats [--owner N] [--json]\n"
	      "       paulictl {start|destroy|memory} TARGET [--epoch E]\n"
	      "       paulictl stop TARGET [--epoch E] [--wait [--timeout-ms MS]]\n"
	      "       paulictl logs TARGET [--epoch E] [--cursor N] [--follow [--timeout-ms MS]]\n"
	      "       paulictl send TARGET [--epoch E] [--tag N] [--hex HEX | TEXT]\n"
	      "       paulictl recv TARGET [--epoch E] [--wait [--timeout-ms MS]] [--raw]\n"
	      "       paulictl create TEMPLATE NAME [--epoch E] [--vcpus N] [--memory-mib M] [--disk 0..3]\n"
	      "  TARGET and TEMPLATE accept a public VM ID or service (ID 2).\n"
	      "  Stop and receive waits default to 30000 ms. Log following has no default timeout.\n"
	      "  Mail uses the inspected peer epoch by default. --epoch 0 uses the granted epoch\n"
	      "  without inspecting and shows epoch=0 in the reply. Tags range from 0 to 255.\n"
	      "  Send copies up to 6 bytes (default empty) and succeeds silently.\n"
	      "  Use -- TEXT for literal text beginning with --.\n"
	      "  Receive prints vm, epoch, tag, len, more and hex for one message, or empty=1.\n"
	      "  --raw writes just the payload. A zero-byte message also completes --wait.\n", out);
}

static bool number64(const char *text, uint64_t min, uint64_t max, uint64_t *out)
{
	if (!*text || text[strspn(text, "0123456789")] != '\0')
		return false;
	errno = 0;
	uintmax_t n = strtoumax(text, NULL, 10);
	if (errno || n < min || n > max)
		return false;
	*out = (uint64_t)n;
	return true;
}

static bool number(const char *text, uint32_t min, uint32_t max, uint32_t *out)
{
	uint64_t n;
	if (!number64(text, min, max, &n))
		return false;
	*out = (uint32_t)n;
	return true;
}

static uint32_t target_id(const char *text)
{
	uint32_t id;
	if (!strcmp(text, "service"))
		return 2;
	return number(text, 1, UINT32_MAX, &id) ? id : 0;
}

static bool valid_name(const char *name)
{
	const char *letters = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
	size_t len = strlen(name);
	return len && len <= PAULI_NAME_MAX && strchr(letters, name[0]) &&
		strspn(name, "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-") == len;
}

static int hex_digit(char c)
{
	if (c >= '0' && c <= '9')
		return c - '0';
	if (c >= 'a' && c <= 'f')
		return c - 'a' + 10;
	if (c >= 'A' && c <= 'F')
		return c - 'A' + 10;
	return -1;
}

static bool hex_payload(struct pauli_mail *mail, const char *text)
{
	size_t n = strlen(text);
	if (n > 2 * PAULI_MAIL_MAX || n % 2)
		return false;
	mail->len = n / 2;
	for (size_t i = 0; i < mail->len; i++) {
		int hi = hex_digit(text[2 * i]), lo = hex_digit(text[2 * i + 1]);
		if (hi < 0 || lo < 0)
			return false;
		mail->data[i] = (hi << 4) | lo;
	}
	return true;
}

static bool parse_command(int argc, char **argv, struct command *cmd)
{
	static const struct {
		const char *name;
		unsigned long op;
	} commands[] = {
		{"info", PAULI_INFO}, {"list", LIST_COMMAND}, {"status", PAULI_INSPECT},
		{"start", PAULI_START}, {"stop", PAULI_STOP}, {"destroy", PAULI_DESTROY},
		{"create", PAULI_CREATE}, {"memory", PAULI_MEMORY},
		{"logs", PAULI_LOG_READ}, {"send", PAULI_MAIL_SEND}, {"recv", PAULI_MAIL_RECV},
		{"stats", PAULI_STATS_HEADER},
	};
	*cmd = (struct command){.timeout_ms = DEFAULT_TIMEOUT_MS};
	if (argc < 2)
		return false;
	for (size_t i = 0; i < sizeof(commands) / sizeof(commands[0]); i++)
		if (!strcmp(argv[1], commands[i].name))
			cmd->op = commands[i].op;
	if (!cmd->op)
		return false;
	if (cmd->op == PAULI_INFO || cmd->op == LIST_COMMAND)
		return argc == 2;
	if (cmd->op == PAULI_STATS_HEADER) {
		for (int i = 2; i < argc; i++) {
			if (!strcmp(argv[i], "--json") && !cmd->json) {
				cmd->json = true;
			} else if (!strcmp(argv[i], "--owner") && !cmd->has_owner && i + 1 < argc) {
				if (!number(argv[++i], 0, STATS_MAX_OWNERS - 1, &cmd->owner))
					return false;
				cmd->has_owner = true;
			} else {
				return false;
			}
		}
		return true;
	}
	if (argc < 3 || !(cmd->id = target_id(argv[2])))
		return false;
	if (cmd->op == PAULI_INSPECT)
		return argc == 3;

	int i = 3;
	bool has_cursor = false, has_tag = false, has_payload = false;
	bool mail = cmd->op == PAULI_MAIL_SEND || cmd->op == PAULI_MAIL_RECV;
	if (cmd->op == PAULI_CREATE) {
		if (i == argc || !valid_name(argv[i]))
			return false;
		memcpy(cmd->create.name, argv[i], strlen(argv[i]));
		i++;
	}
	while (i < argc) {
		const char *option = argv[i++];
		if (cmd->op == PAULI_MAIL_SEND && (!strcmp(option, "--") || strncmp(option, "--", 2))) {
			if (!strcmp(option, "--")) {
				if (i != argc - 1)
					return false;
				option = argv[i++];
			}
			size_t n = strlen(option);
			if (has_payload || n > PAULI_MAIL_MAX)
				return false;
			memcpy(cmd->mail.data, option, n);
			cmd->mail.len = n;
			has_payload = true;
			continue;
		}
		if (!strcmp(option, "--raw") && cmd->op == PAULI_MAIL_RECV && !cmd->raw) {
			cmd->raw = true;
			continue;
		}
		if (!strcmp(option, "--wait") &&
		    (cmd->op == PAULI_STOP || cmd->op == PAULI_MAIL_RECV) && !cmd->wait) {
			cmd->wait = true;
			continue;
		}
		if (!strcmp(option, "--follow") && cmd->op == PAULI_LOG_READ && !cmd->follow) {
			cmd->follow = true;
			continue;
		}
		if (i == argc)
			return false;
		const char *value = argv[i++];
		uint32_t n;
		if (!strcmp(option, "--epoch") && !cmd->has_epoch) {
			if (!number(value, mail ? 0 : 1, UINT32_MAX, &cmd->epoch))
				return false;
			cmd->has_epoch = true;
		} else if (cmd->op == PAULI_MAIL_SEND && !strcmp(option, "--tag") && !has_tag) {
			if (!number(value, 0, UINT8_MAX, &n))
				return false;
			cmd->mail.tag = n;
			has_tag = true;
		} else if (cmd->op == PAULI_MAIL_SEND && !strcmp(option, "--hex") && !has_payload) {
			if (!hex_payload(&cmd->mail, value))
				return false;
			has_payload = true;
		} else if (cmd->op == PAULI_LOG_READ && !strcmp(option, "--cursor") && !has_cursor) {
			if (!number64(value, 0, UINT64_MAX, &cmd->cursor))
				return false;
			has_cursor = true;
		} else if (!strcmp(option, "--timeout-ms") && !cmd->has_timeout &&
			   (cmd->op == PAULI_STOP || cmd->op == PAULI_LOG_READ || cmd->op == PAULI_MAIL_RECV)) {
			if (!number(value, 0, UINT32_MAX, &cmd->timeout_ms))
				return false;
			cmd->has_timeout = true;
		} else if (cmd->op == PAULI_CREATE && !strcmp(option, "--vcpus") && !cmd->create.vcpus) {
			if (!number(value, 1, UINT8_MAX, &n))
				return false;
			cmd->create.vcpus = n;
		} else if (cmd->op == PAULI_CREATE && !strcmp(option, "--memory-mib") && !cmd->create.memory_mib) {
			if (!number(value, 1, UINT16_MAX, &n))
				return false;
			cmd->create.memory_mib = n;
		} else if (cmd->op == PAULI_CREATE && !strcmp(option, "--disk") && !cmd->create.disk) {
			if (!number(value, 0, 3, &n))
				return false;
			cmd->create.disk = n + 1;
		} else {
			return false;
		}
	}
	return !cmd->has_timeout || cmd->wait || cmd->follow;
}

static const char *state_name(unsigned int state)
{
	static const char *const names[] = {
		[PAULI_CREATED] = "created", [PAULI_READY] = "ready",
		[PAULI_RUNNING] = "running", [PAULI_WAITING] = "waiting",
		[PAULI_STOPPING] = "stopping", [PAULI_STOPPED] = "stopped",
		[PAULI_FAULTED] = "faulted",
	};
	return state < sizeof(names) / sizeof(names[0]) && names[state] ?
		names[state] : "unknown";
}

static void print_vm(const struct pauli_vm *vm, bool mutation)
{
	printf("vm=%" PRIu32 " epoch=%" PRIu32 " state=%s vcpus=%u active=%u",
	       vm->id, vm->epoch, state_name(vm->state), vm->vcpus, vm->active);
	if (mutation)
		printf(" changed=%u", vm->changed);
	putchar('\n');
}

static int inspect(int fd, uint32_t id, struct pauli_vm *vm)
{
	*vm = (struct pauli_vm){.id = id};
	return ioctl(fd, PAULI_INSPECT, vm);
}

static struct timespec plus_ms(struct timespec time, uint32_t ms)
{
	time.tv_sec += ms / 1000;
	time.tv_nsec += (long)(ms % 1000) * 1000000L;
	if (time.tv_nsec >= 1000000000L) {
		time.tv_sec++;
		time.tv_nsec -= 1000000000L;
	}
	return time;
}

static bool before(struct timespec a, struct timespec b)
{
	return a.tv_sec < b.tv_sec || (a.tv_sec == b.tv_sec && a.tv_nsec < b.tv_nsec);
}

/* Check even a busy stream's deadline; only sleep when polling for new data. */
static int next_poll(const struct timespec *limit, bool idle)
{
	struct timespec now;
	if (!limit && !idle)
		return 0;
	if (clock_gettime(CLOCK_MONOTONIC, &now) < 0)
		return -1;
	if (limit && !before(now, *limit)) {
		errno = ETIMEDOUT;
		return -1;
	}
	if (!idle)
		return 0;
	struct timespec next = plus_ms(now, POLL_MS);
	if (limit && before(*limit, next))
		next = *limit;
	int error;
	do {
		error = clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &next, NULL);
	} while (error == EINTR);
	if (error) {
		errno = error;
		return -1;
	}
	return 0;
}

static int wait_stop(int fd, struct pauli_vm *vm, uint32_t timeout_ms)
{
	const uint32_t id = vm->id, epoch = vm->epoch;
	const unsigned int changed = vm->changed;
	struct timespec now, limit;

	if (clock_gettime(CLOCK_MONOTONIC, &now) < 0)
		return -1;
	limit = plus_ms(now, timeout_ms);
	for (;;) {
		if (inspect(fd, id, vm) < 0)
			return -1;
		if (vm->epoch != epoch) {
			errno = ESTALE;
			return -1;
		}
		if (vm->state == PAULI_STOPPED || vm->state == PAULI_FAULTED) {
			vm->changed = changed;
			return 0;
		}
		if (next_poll(&limit, true) < 0)
			return -1;
	}
}

static int read_logs(int fd, const struct command *cmd, uint32_t epoch)
{
	uint64_t cursor = cmd->cursor;
	struct timespec limit;
	const struct timespec *deadline = cmd->has_timeout ? &limit : NULL;
	if (deadline) {
		if (clock_gettime(CLOCK_MONOTONIC, &limit) < 0)
			return -1;
		limit = plus_ms(limit, cmd->timeout_ms);
	}
	for (;;) {
		struct pauli_log log = {.id = cmd->id, .epoch = epoch, .cursor = cursor};
		if (ioctl(fd, PAULI_LOG_READ, &log) < 0)
			return -1;
		if (log.epoch != epoch) {
			errno = ESTALE;
			return -1;
		}
		if (log.flags == PAULI_LOG_OVERRUN && !log.len && log.cursor > cursor) {
			fprintf(stderr, "paulictl: log overrun at cursor %" PRIu64
				", resuming at %" PRIu64 "\n", cursor, (uint64_t)log.cursor);
		} else if (log.flags || log.len > sizeof(log.data) || log.cursor < cursor ||
			   log.cursor - cursor != log.len) {
			errno = EPROTO;
			return -1;
		} else if (log.len) {
			if (fwrite(log.data, 1, log.len, stdout) != log.len || fflush(stdout) == EOF)
				return -1;
		} else if (!cmd->follow) {
			return 0;
		}
		cursor = log.cursor;
		if (!log.len && !log.flags && next_poll(deadline, true) < 0)
			return -1;
		/* A sleep can end at or after the deadline. Check before the next ioctl. */
		if (deadline && next_poll(deadline, false) < 0)
			return -1;
	}
}

static int print_mail(const struct pauli_mail *mail, bool raw)
{
	if (raw) {
		if (fwrite(mail->data, 1, mail->len, stdout) != mail->len)
			return -1;
	} else {
		static const char digits[] = "0123456789abcdef";
		char hex[2 * PAULI_MAIL_MAX + 1];
		for (size_t i = 0; i < mail->len; i++) {
			hex[2 * i] = digits[mail->data[i] >> 4];
			hex[2 * i + 1] = digits[mail->data[i] & 15];
		}
		hex[2 * mail->len] = '\0';
		if (printf("vm=%" PRIu32 " epoch=%" PRIu32 " tag=%u len=%u more=%u hex=%s\n",
			   mail->id, mail->epoch, mail->tag, mail->len,
			   !!(mail->flags & PAULI_MAIL_MORE), hex) < 0)
			return -1;
	}
	return fflush(stdout);
}

static int recv_mail(int fd, const struct command *cmd, uint32_t epoch)
{
	struct timespec limit;
	if (cmd->wait) {
		if (clock_gettime(CLOCK_MONOTONIC, &limit) < 0)
			return -1;
		limit = plus_ms(limit, cmd->timeout_ms);
	}
	for (;;) {
		struct pauli_mail mail = {.id = cmd->id, .epoch = epoch};
		if (ioctl(fd, PAULI_MAIL_RECV, &mail) < 0)
			return -1;
		if (mail.epoch != epoch) {
			errno = ESTALE;
			return -1;
		}
		if (mail.id != cmd->id || mail.len > PAULI_MAIL_MAX ||
		    mail.flags & ~(PAULI_MAIL_PRESENT | PAULI_MAIL_MORE) ||
		    (!(mail.flags & PAULI_MAIL_PRESENT) && (mail.len || mail.tag || mail.flags))) {
			errno = EPROTO;
			return -1;
		}
		if (mail.flags & PAULI_MAIL_PRESENT)
			return print_mail(&mail, cmd->raw);
		if (!cmd->wait) {
			if (!cmd->raw && puts("empty=1") == EOF)
				return -1;
			return fflush(stdout);
		}
		/* A successful empty dequeue can be polled. A failed dequeue may have consumed mail. */
		if (next_poll(&limit, true) < 0 || next_poll(&limit, false) < 0)
			return -1;
	}
}

/* These fields alone have additive semantics across physical owners. */
static const struct {
	const char *name;
	size_t offset;
} stats_counters[] = {
#define COUNTER(field) {#field, offsetof(struct pauli_stats_owner, field)}
	COUNTER(vm_exits), COUNTER(wfi_exits), COUNTER(sbi_timer_exits),
	COUNTER(sstc_timer_exits), COUNTER(mmio_exits), COUNTER(other_exits),
	COUNTER(vtimer_irqs), COUNTER(ext_irqs), COUNTER(idle_wfi),
	COUNTER(mail_sends), COUNTER(mail_recvs),
#undef COUNTER
};

/* A sum spans at most 64 u64 values. Base 10^18 also works on compat hosts. */
#define DECIMAL_BASE UINT64_C(1000000000000000000)
struct counter_sum {
	uint64_t high, low;
};

static void add_counter(struct counter_sum *sum, uint64_t value)
{
	sum->high += value / DECIMAL_BASE;
	sum->low += value % DECIMAL_BASE;
	if (sum->low >= DECIMAL_BASE) {
		sum->low -= DECIMAL_BASE;
		sum->high++;
	}
}

static void print_stats(const struct pauli_stats_header *header,
			const struct pauli_stats_owner *rows, size_t count, bool json)
{
	enum { COUNTERS = sizeof(stats_counters) / sizeof(stats_counters[0]) };
	struct counter_sum totals[COUNTERS] = {};
	if (json)
		printf("{\"version\":%u,\"sample\":\"live\",\"trap_redacted\":true,"
		       "\"host_cpus\":%u,\"sstc_owners\":\"0x%" PRIx64 "\","
		       "\"exclusive_owners\":\"0x%" PRIx64 "\",\"owners\":[",
		       header->version, header->host_cpus, (uint64_t)header->sstc_owners,
		       (uint64_t)header->exclusive_owners);
	else
		printf("version=%u sample=live trap_redacted=1 host_cpus=%u "
		       "sstc_owners=0x%" PRIx64 " exclusive_owners=0x%" PRIx64 "\n",
		       header->version, header->host_cpus, (uint64_t)header->sstc_owners,
		       (uint64_t)header->exclusive_owners);
	for (size_t i = 0; i < count; i++) {
		const struct pauli_stats_owner *row = &rows[i];
		if (json)
			printf("%s{\"owner\":%u,\"registered\":%s,\"hart_id\":", i ? "," : "",
			       row->owner, row->registered ? "true" : "false");
		else
			printf("owner=%u registered=%u hart_id=", row->owner, row->registered);
		if (!row->registered) {
			fputs(json ? "null}" : "unregistered\n", stdout);
			continue;
		}
		printf(json ? "\"%" PRIu64 "\"" : "%" PRIu64, (uint64_t)row->hart_id);
		for (size_t k = 0; k < COUNTERS; k++) {
			uint64_t value;
			memcpy(&value, (const char *)row + stats_counters[k].offset, sizeof(value));
			add_counter(&totals[k], value);
			printf(json ? ",\"%s\":\"%" PRIu64 "\"" : " %s=%" PRIu64,
			       stats_counters[k].name, value);
		}
		putchar(json ? '}' : '\n');
	}
	fputs(json ? "],\"totals\":{" : "totals", stdout);
	for (size_t k = 0; k < COUNTERS; k++) {
		if (json)
			printf("%s\"%s\":\"", k ? "," : "", stats_counters[k].name);
		else
			printf(" %s=", stats_counters[k].name);
		if (totals[k].high)
			printf("%" PRIu64 "%018" PRIu64, totals[k].high, totals[k].low);
		else
			printf("%" PRIu64, totals[k].low);
		if (json)
			putchar('"');
	}
	fputs(json ? "}}\n" : "\n", stdout);
}

static int read_stats(int fd, const struct command *cmd)
{
	struct pauli_stats_header hdr = {};
	struct pauli_stats_owner rows[STATS_MAX_OWNERS] = {};
	if (ioctl(fd, PAULI_STATS_HEADER, &hdr) < 0)
		return -1;
	if (hdr.version != 2 || !hdr.host_cpus || hdr.host_cpus > STATS_MAX_OWNERS ||
	    !(hdr.flags & PAULI_STATS_TRAP_REDACTED)) {
		errno = EPROTO;
		return -1;
	}
	if (cmd->has_owner && cmd->owner >= hdr.host_cpus) {
		errno = EINVAL;
		return -1;
	}
	size_t count = cmd->has_owner ? 1 : hdr.host_cpus;
	for (size_t i = 0; i < count; i++) {
		struct pauli_stats_owner *row = &rows[i];
		uint32_t owner = cmd->has_owner ? cmd->owner : (uint32_t)i;
		row->owner = owner;
		if (ioctl(fd, PAULI_STATS_OWNER, row) < 0)
			return -1;
		if (row->owner != owner || row->registered != (row->hart_id != UINT64_MAX) ||
		    !(row->flags & PAULI_STATS_TRAP_REDACTED) ||
		    row->last_scause || row->last_sepc || row->last_stval) {
			errno = EPROTO;
			return -1;
		}
	}
	/* Complete every ioctl before emitting any part of a machine-readable reply. */
	print_stats(&hdr, rows, count, cmd->json);
	return ferror(stdout) ? -1 : fflush(stdout);
}

static int request(int fd, const struct command *cmd)
{
	struct pauli_vm vm;
	if (cmd->op == PAULI_STATS_HEADER)
		return read_stats(fd, cmd);
	if (cmd->op == PAULI_INFO || cmd->op == LIST_COMMAND) {
		struct pauli_info info = {0};
		if (ioctl(fd, PAULI_INFO, &info) < 0)
			return -1;
		if (cmd->op == PAULI_INFO) {
			printf("abi=%" PRIu32 " max_id=%" PRIu32 "\n", info.version, info.max_id);
			return 0;
		}
		// max_id is an inclusive address ceiling; UINT32_MAX cannot wrap this counter.
		for (uint64_t id = 1; id <= info.max_id; id++) {
			if (inspect(fd, (uint32_t)id, &vm) == 0)
				print_vm(&vm, false);
			else if (errno != EACCES && errno != EINVAL)
				return -1;
		}
		return 0;
	}

	uint32_t epoch = cmd->epoch;
	if (!cmd->has_epoch) {
		if (inspect(fd, cmd->id, &vm) < 0)
			return -1;
		if (cmd->op == PAULI_INSPECT) {
			print_vm(&vm, false);
			return 0;
		}
		epoch = vm.epoch;
	}
	if (cmd->op == PAULI_LOG_READ)
		return read_logs(fd, cmd, epoch);
	if (cmd->op == PAULI_MAIL_SEND) {
		struct pauli_mail mail = cmd->mail;
		mail.id = cmd->id;
		mail.epoch = epoch;
		return ioctl(fd, PAULI_MAIL_SEND, &mail);
	}
	if (cmd->op == PAULI_MAIL_RECV)
		return recv_mail(fd, cmd, epoch);
	if (cmd->op == PAULI_CREATE) {
		struct pauli_create child = cmd->create;
		child.template_id = cmd->id;
		child.epoch = epoch;
		if (ioctl(fd, PAULI_CREATE, &child) < 0)
			return -1;
		printf("vm=%" PRIu32 " epoch=%" PRIu32 " template=%" PRIu32 " name=%.*s\n",
		       child.id, child.child_epoch, cmd->id, PAULI_NAME_MAX, cmd->create.name);
		return 0;
	}
	if (cmd->op == PAULI_MEMORY) {
		struct pauli_memory memory = {.id = cmd->id, .epoch = epoch};
		if (ioctl(fd, PAULI_MEMORY, &memory) < 0)
			return -1;
		printf("vm=%" PRIu32 " epoch=%" PRIu32 " memory_bytes=%" PRIu64 "\n",
		       memory.id, memory.epoch, (uint64_t)memory.bytes);
		return 0;
	}

	// A failed mutation may already have taken effect. Never retry it.
	vm = (struct pauli_vm){.id = cmd->id, .epoch = epoch};
	if (ioctl(fd, cmd->op, &vm) < 0)
		return -1;
	if (cmd->wait) {
		if (vm.epoch != epoch) {
			errno = ESTALE;
			return -1;
		}
		if (wait_stop(fd, &vm, cmd->timeout_ms) < 0)
			return -1;
	}
	print_vm(&vm, true);
	return 0;
}

int main(int argc, char **argv)
{
	struct command cmd;
	if (argc == 2 && !strcmp(argv[1], "--help")) {
		usage(stdout);
		return 0;
	}
	if (!parse_command(argc, argv, &cmd)) {
		usage(stderr);
		return 2;
	}
	int fd = open("/dev/pauli", O_RDWR | O_CLOEXEC);
	if (fd < 0) {
		perror("paulictl: /dev/pauli");
		return 1;
	}
	int rc = request(fd, &cmd);
	if (rc < 0)
		perror("paulictl: request");
	close(fd);
	return rc < 0 ? 1 : 0;
}
