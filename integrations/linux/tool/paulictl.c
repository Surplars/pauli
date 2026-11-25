// SPDX-License-Identifier: GPL-2.0-only
// Userspace never issues SBI ecalls: the guest kernel owns the VS/HS boundary.
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdbool.h>
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

struct command {
	unsigned long op;
	uint32_t id, epoch, timeout_ms;
	bool wait;
	struct pauli_create create;
};

static void usage(FILE *out)
{
	fputs("Usage: paulictl info | list | status TARGET\n"
	      "       paulictl {start|destroy|memory} TARGET [--epoch E]\n"
	      "       paulictl stop TARGET [--epoch E] [--wait [--timeout-ms MS]]\n"
	      "       paulictl create TEMPLATE NAME [--epoch E] [--vcpus N] [--memory-mib M] [--disk 0..3]\n"
	      "  TARGET and TEMPLATE accept a public VM ID or service (ID 2).\n"
	      "  Stop waits default to 30000 ms and poll at most every 50 ms.\n", out);
}

static bool number(const char *text, uint32_t min, uint32_t max, uint32_t *out)
{
	if (!*text || text[strspn(text, "0123456789")] != '\0')
		return false;
	errno = 0;
	uintmax_t n = strtoumax(text, NULL, 10);
	if (errno || n < min || n > max)
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

static bool parse_command(int argc, char **argv, struct command *cmd)
{
	static const struct {
		const char *name;
		unsigned long op;
	} commands[] = {
		{"info", PAULI_INFO}, {"list", LIST_COMMAND}, {"status", PAULI_INSPECT},
		{"start", PAULI_START}, {"stop", PAULI_STOP}, {"destroy", PAULI_DESTROY},
		{"create", PAULI_CREATE}, {"memory", PAULI_MEMORY},
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
	if (argc < 3 || !(cmd->id = target_id(argv[2])))
		return false;
	if (cmd->op == PAULI_INSPECT)
		return argc == 3;

	int i = 3;
	bool has_timeout = false;
	if (cmd->op == PAULI_CREATE) {
		if (i == argc || !valid_name(argv[i]))
			return false;
		memcpy(cmd->create.name, argv[i], strlen(argv[i]));
		i++;
	}
	while (i < argc) {
		const char *option = argv[i++];
		if (!strcmp(option, "--wait") && cmd->op == PAULI_STOP && !cmd->wait) {
			cmd->wait = true;
			continue;
		}
		if (i == argc)
			return false;
		const char *value = argv[i++];
		uint32_t n;
		if (!strcmp(option, "--epoch") && !cmd->epoch) {
			if (!number(value, 1, UINT32_MAX, &cmd->epoch))
				return false;
		} else if (!strcmp(option, "--timeout-ms") && cmd->op == PAULI_STOP && !has_timeout) {
			if (!number(value, 0, UINT32_MAX, &cmd->timeout_ms))
				return false;
			has_timeout = true;
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
	return !has_timeout || cmd->wait;
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
		if (clock_gettime(CLOCK_MONOTONIC, &now) < 0)
			return -1;
		if (!before(now, limit)) {
			errno = ETIMEDOUT;
			return -1;
		}
		struct timespec next = plus_ms(now, POLL_MS);
		if (before(limit, next))
			next = limit;
		int error;
		do {
			error = clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &next, NULL);
		} while (error == EINTR);
		if (error) {
			errno = error;
			return -1;
		}
	}
}

static int request(int fd, const struct command *cmd)
{
	struct pauli_vm vm;
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
	if (!epoch) {
		if (inspect(fd, cmd->id, &vm) < 0)
			return -1;
		if (cmd->op == PAULI_INSPECT) {
			print_vm(&vm, false);
			return 0;
		}
		epoch = vm.epoch;
	}
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
