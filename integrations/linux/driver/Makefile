# SPDX-License-Identifier: GPL-2.0-only
# External Kbuild uses this repository's UAPI. In-tree, install the header
# into the kernel's include/uapi/linux/ and select CONFIG_PAULI_HV.
ifneq ($(KBUILD_EXTMOD),)
obj-m += pauli.o
# Kbuild's standard include paths can contain an older Pauli UAPI. Load the
# packaged header first so its include guard also covers the kernel-tree copy.
ccflags-y += -include $(src)/../include/uapi/linux/pauli.h -I$(src)/../include
else
obj-$(CONFIG_PAULI_HV) += pauli.o
endif

# A standalone Linux host can exercise the ioctl adapter without a kernel build.
ifeq ($(KERNELRELEASE)$(KBUILD_EXTMOD),)
HOSTCC ?= cc
BUILD_DIR ?= ../../../target/pauli-driver
.PHONY: test
test: $(BUILD_DIR)/test-pauli
	"$<"

$(BUILD_DIR)/test-pauli: test-pauli.c test-events.h pauli.c pauli-events.h ../include/uapi/linux/pauli.h
	mkdir -p "$(@D)"
	$(HOSTCC) $(HOSTCFLAGS) -std=gnu11 -O2 -Wall -Wextra -Werror -Wno-unused-parameter -pthread -I../include "$<" -o "$@"
endif
