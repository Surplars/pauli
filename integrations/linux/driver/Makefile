# SPDX-License-Identifier: GPL-2.0-only
# External Kbuild uses this repository's UAPI. In-tree, install the header
# into the kernel's include/uapi/linux/ and select CONFIG_PAULI_HV.
ifneq ($(KBUILD_EXTMOD),)
obj-m += pauli.o
ccflags-y += -I$(src)/../include
else
obj-$(CONFIG_PAULI_HV) += pauli.o
endif
