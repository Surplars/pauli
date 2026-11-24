#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Build against an Image-only kernel output without modifying it, its source
# tree, or running modules_install. This driver imports only built-in symbols.
set -euo pipefail
if [[ $# != 3 ]]; then
	echo "usage: bash $0 LINUX_SOURCE MATCHING_KERNEL_OUTPUT NEW_WORK_DIR" >&2
	exit 2
fi
src=$(realpath "$1")
built=$(realpath "$2")
out=$(realpath -m "$3")
driver=$(cd "$(dirname "$0")" && pwd)
release=$(cat "$built/include/config/kernel.release")
cross=${CROSS_COMPILE:-riscv64-linux-gnu-}

# Never substitute stock headers or guess a private module layout.
grep -qx 'CONFIG_MODULES=y' "$built/.config"
if grep -Eq '^CONFIG_(RANDSTRUCT_FULL|RANDSTRUCT_PERFORMANCE)=y' "$built/.config"; then
	echo "RANDSTRUCT needs the original prepared kernel build and layout seed" >&2
	exit 1
fi
symbols="$built/Module.symvers"
[[ -s $symbols ]] || symbols="$built/vmlinux.symvers"
[[ -s $symbols ]] || { echo "missing exports from the matching kernel" >&2; exit 1; }
mkdir "$out" # refuse to reuse/clobber someone else's build
mkdir "$out/kernel" "$out/module" "$out/tmp"
export TMPDIR="$out/tmp"
cp "$built/.config" "$out/kernel/.config"
kargs=(-C "$src" O="$out/kernel" ARCH=riscv CROSS_COMPILE="$cross" KERNELRELEASE="$release")
make "${kargs[@]}" -s -j4 modules_prepare
# Kbuild can update an old config. Do not ship against a different layout.
cmp "$built/include/generated/autoconf.h" "$out/kernel/include/generated/autoconf.h"
cmp "$built/include/generated/utsrelease.h" "$out/kernel/include/generated/utsrelease.h"
cp "$symbols" "$out/kernel/Module.symvers"
make "${kargs[@]}" M="$driver" MO="$out/module" modules
printf 'Module for %s: %s/module/pauli.ko\n' "$release" "$out"
