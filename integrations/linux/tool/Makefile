# Host tests do not require a Linux guest or a device. Keep generated files out of source.
BUILD_DIR ?= ../../../target/paulictl
CC = $(CROSS_COMPILE)gcc
HOSTCC ?= cc
CPPFLAGS += -I../include/uapi
CFLAGS += -O2 -std=gnu11 -Wall -Wextra -Werror

.PHONY: all test
all: $(BUILD_DIR)/paulictl

$(BUILD_DIR):
	mkdir -p "$@"

$(BUILD_DIR)/paulictl: paulictl.c ../include/uapi/linux/pauli.h | $(BUILD_DIR)
	$(CC) $(CPPFLAGS) $(CFLAGS) $< $(LDFLAGS) -o "$@"

$(BUILD_DIR)/test-paulictl: test-paulictl.c paulictl.c ../include/uapi/linux/pauli.h | $(BUILD_DIR)
	$(HOSTCC) $(CPPFLAGS) $(CFLAGS) $< -o "$@"

test: $(BUILD_DIR)/test-paulictl
	"$<"
