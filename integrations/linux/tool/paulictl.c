// SPDX-License-Identifier: GPL-2.0-only
// Userspace never issues SBI ecalls: the guest kernel owns the VS/HS boundary.
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include <linux/pauli.h>

static void usage(FILE *out)
{
	fputs("Usage: paulictl info | {status|start|stop} {VM_ID|service}\n"
	      "  service is public VM ID 2 in the Wave8/Wave9 manager profile.\n", out);
}

static uint32_t target_id(const char *s)
{
	if (!strcmp(s, "service"))
		return 2;
	if (!*s || strspn(s, "0123456789") != strlen(s))
		return 0;
	errno = 0;
	unsigned long long n = strtoull(s, NULL, 10);
	return !errno && n <= UINT32_MAX ? (uint32_t)n : 0;
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

static int request(int fd, unsigned long op, uint32_t id)
{
	if (op == PAULI_INFO) {
		struct pauli_info info = {0};
		if (ioctl(fd, op, &info) < 0)
			return -1;
		printf("abi=%" PRIu32 " max_id=%" PRIu32 "\n", info.version, info.max_id);
		return 0;
	}

	struct pauli_vm vm = {.id = id};
	if (ioctl(fd, PAULI_INSPECT, &vm) < 0)
		return -1;
	if (op != PAULI_INSPECT) {
		// Bind this action to the instance we just saw. Never retry against
		// a replacement VM, even when the public ID stays the same.
		vm = (struct pauli_vm){.id = id, .epoch = vm.epoch};
		if (ioctl(fd, op, &vm) < 0)
			return -1;
	}
	printf("vm=%" PRIu32 " epoch=%" PRIu32 " state=%s vcpus=%u active=%u",
	       vm.id, vm.epoch, state_name(vm.state), vm.vcpus, vm.active);
	if (op != PAULI_INSPECT)
		printf(" changed=%u", vm.changed);
	putchar('\n');
	return 0;
}

int main(int argc, char **argv)
{
	unsigned long op;
	uint32_t id = 0;

	if (argc == 2 && !strcmp(argv[1], "--help")) {
		usage(stdout);
		return 0;
	}
	if (argc == 2 && !strcmp(argv[1], "info"))
		op = PAULI_INFO;
	else if (argc == 3 && (id = target_id(argv[2]))) {
		if (!strcmp(argv[1], "status"))
			op = PAULI_INSPECT;
		else if (!strcmp(argv[1], "start"))
			op = PAULI_START;
		else if (!strcmp(argv[1], "stop"))
			op = PAULI_STOP;
		else
			goto invalid;
	} else
		goto invalid;

	int fd = open("/dev/pauli", O_RDWR | O_CLOEXEC);
	if (fd < 0) {
		perror("paulictl: /dev/pauli");
		return 1;
	}
	int rc = request(fd, op, id);
	if (rc < 0)
		perror("paulictl: request");
	close(fd);
	return rc < 0 ? 1 : 0;
invalid:
	usage(stderr);
	return 2;
}
