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

#define PAULI_NAME_MAX	16
#define PAULI_DISK_NONE	0

struct pauli_memory {
	__u32 id;	/* In: target public VM ID and its observed epoch. */
	__u32 epoch;
	__u64 bytes;	/* Out: guest memory size in bytes. */
};

struct pauli_create {
	__u32 template_id;	/* In: template public VM ID and its observed epoch. */
	__u32 epoch;
	__u32 id;		/* Out: new child's public VM ID and epoch. */
	__u32 child_epoch;
	__u16 memory_mib;	/* Zero inherits this field from the template. */
	__u8 vcpus;		/* Zero inherits this field from the template. */
	__u8 disk;		/* PAULI_DISK_NONE or one-based logical volume 1..4. */
	__u32 flags;		/* Reserved; must be zero. */
	char name[PAULI_NAME_MAX]; /* ASCII child name, zero-padded if shorter. */
};

#define PAULI_DESTROY	_IOWR('P', 0x74, struct pauli_vm)
#define PAULI_CREATE	_IOWR('P', 0x75, struct pauli_create)
#define PAULI_MEMORY	_IOWR('P', 0x76, struct pauli_memory)

#endif /* _UAPI_LINUX_PAULI_H */
