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

#define PAULI_LOG_CHUNK	256
#define PAULI_LOG_OVERRUN	(1U << 0)

struct pauli_log {
	__u32 id;
	__u32 epoch;	/* Input: exact target epoch, including after stop. */
	__u64 cursor;	/* In: next byte to read. Out: next byte or overrun resume. */
	__u32 len;	/* Out: valid bytes in data. */
	__u32 flags;	/* In: zero. Out: zero or PAULI_LOG_OVERRUN. */
	__u8 data[PAULI_LOG_CHUNK];
};

#define PAULI_LOG_READ	_IOWR('P', 0x77, struct pauli_log)

#define PAULI_MAIL_MAX		6
#define PAULI_MAIL_PRESENT	(1U << 0)
#define PAULI_MAIL_MORE		(1U << 1)

struct pauli_mail {
	__u32 id;	/* Peer VM ID. */
	__u32 epoch;	/* Zero selects the peer epoch bound to the MAIL grant. */
	__u8 tag;
	__u8 len;
	__u16 flags;	/* In: zero. Out: PAULI_MAIL_PRESENT and PAULI_MAIL_MORE. */
	__u8 data[8];	/* Six payload bytes at most. Unused bytes are zero. */
};

#define PAULI_MAIL_SEND	_IOW('P', 0x78, struct pauli_mail)
#define PAULI_MAIL_RECV	_IOWR('P', 0x79, struct pauli_mail)

#define PAULI_STATS_TRAP_REDACTED	(1U << 0)

/* Live samples. Each 64-bit counter is read independently. */
struct pauli_stats_header {
	__u32 version;
	__u32 host_cpus;
	__u64 sstc_owners;
	__u64 exclusive_owners;
	__u32 flags;	/* PAULI_STATS_TRAP_REDACTED. */
	__u32 reserved;
};

struct pauli_stats_owner {
	__u32 owner;	/* In: zero-based owner index below host_cpus. */
	__u32 registered;
	__u32 flags;	/* All fields except owner are output only. */
	__u32 pad;
	__u64 hart_id;	/* All ones for an unregistered owner. */
	__u64 vm_exits, wfi_exits, sbi_timer_exits, sstc_timer_exits;
	__u64 mmio_exits, other_exits, vtimer_irqs, ext_irqs;
	__u64 last_scause, last_sepc, last_stval;	/* Always zero. */
	__u64 idle_wfi, mail_sends, mail_recvs, reserved;
};

#define PAULI_STATS_HEADER	_IOR('P', 0x7a, struct pauli_stats_header)
#define PAULI_STATS_OWNER	_IOWR('P', 0x7b, struct pauli_stats_owner)

#endif /* _UAPI_LINUX_PAULI_H */
