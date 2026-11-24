/* SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note */
#ifndef _UAPI_LINUX_PAULI_H
#define _UAPI_LINUX_PAULI_H

#include <linux/ioctl.h>
#include <linux/types.h>

/* /dev/pauli: fixed-width, pointer-free ioctls on RV64 and compat tasks. */
struct pauli_info {
	__u32 version;
	__u32 max_id;	/* Address ceiling, not the number of visible VMs. */
};

enum pauli_state {
	PAULI_CREATED = 1,
	PAULI_READY,
	PAULI_RUNNING,
	PAULI_WAITING,
	PAULI_STOPPING,
	PAULI_STOPPED,
	PAULI_FAULTED,
};

struct pauli_vm {
	__u32 id;	/* Input: one-based Pauli resource ID. */
	__u32 epoch;	/* In: zero (inspect) or observed epoch; out: current epoch. */
	__u8 state;	/* The four fields below are output only. */
	__u8 active;
	__u8 vcpus;
	__u8 changed;
};

#define PAULI_INFO	_IOR('P', 0x70, struct pauli_info)
#define PAULI_INSPECT	_IOWR('P', 0x71, struct pauli_vm)
#define PAULI_START	_IOWR('P', 0x72, struct pauli_vm)
#define PAULI_STOP	_IOWR('P', 0x73, struct pauli_vm)

#endif /* _UAPI_LINUX_PAULI_H */
