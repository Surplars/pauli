// SPDX-License-Identifier: GPL-2.0-only
// Exercise the actual CLI request sequence, including a cold restart race.
#include <assert.h>
#include <stdarg.h>
#include <sys/ioctl.h>

static int fixture_ioctl(int fd, unsigned long op, ...);
#define ioctl fixture_ioctl
#define main paulictl_main
#include "paulictl.c"
#undef main
#undef ioctl

static unsigned int calls;
static unsigned long action;
static int fail_inspect;

static int fixture_ioctl(int fd, unsigned long op, ...)
{
	assert(fd == 5);
	va_list args;
	va_start(args, op);
	++calls;
	if (op == PAULI_INFO) {
		struct pauli_info *info = va_arg(args, struct pauli_info *);
		assert(calls == 1);
		*info = (struct pauli_info){.version = 1, .max_id = 4};
	} else {
		struct pauli_vm *vm = va_arg(args, struct pauli_vm *);
		assert(vm->id == 2);
		if (calls == 1) {
			assert(op == PAULI_INSPECT && vm->epoch == 0);
			if (fail_inspect) {
				va_end(args);
				errno = EACCES;
				return -1;
			}
			vm->epoch = 42;
			vm->state = PAULI_STOPPED;
			vm->vcpus = 1;
		} else {
			assert(calls == 2 && op == action && vm->epoch == 42);
			assert(!vm->state && !vm->active && !vm->vcpus && !vm->changed);
			if (action == PAULI_STOP) {
				va_end(args);
				errno = EAGAIN;
				return -1;
			}
			vm->epoch = 43;
			vm->state = PAULI_READY;
			vm->vcpus = vm->changed = 1;
		}
	}
	va_end(args);
	return 0;
}

int main(void)
{
	assert(target_id("service") == 2 && target_id("4294967295") == UINT32_MAX);
	assert(!target_id("") && !target_id("4294967296") && !target_id("-1"));
	assert(!target_id("+2") && !target_id(" 2") && !target_id("0"));
	assert(request(5, PAULI_INFO, 0) == 0 && calls == 1);
	calls = 0;
	assert(request(5, PAULI_INSPECT, 2) == 0 && calls == 1);
	calls = 0;
	action = PAULI_START;
	assert(request(5, action, 2) == 0 && calls == 2);
	calls = 0;
	action = PAULI_STOP;
	assert(request(5, action, 2) < 0 && errno == EAGAIN && calls == 2);
	calls = 0;
	fail_inspect = 1;
	assert(request(5, action, 2) < 0 && errno == EACCES && calls == 1);
	puts("PASS: target IDs, info, inspect, epoch-bound start/stop, denied, no retry");
	return 0;
}
