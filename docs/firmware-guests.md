# Diskless RTOS + bare firmware

OpenSBI enters HS; the native monitor starts two independent VS images. There
is no Linux kernel, initramfs, root disk, network, or privileged management VM.
The HS workspace takes ordinary ELF artifacts, not a dependency on an RTOS.

## Try the pair

From the OpenIon RTOS **repository root**, build the example ELFs. This guest-only
package uses the existing S-mode port, scheduler, trap code and small-kernel
profile; no change to the RTOS kernel, configuration or application ABI is
needed. The second binary is just a stackless UART service.

```powershell
cargo build --offline --locked --release --target riscv64imac-unknown-none-elf -p openion-vs-example
cd ../pauli
$fw = '../rtos/target/riscv64imac-unknown-none-elf/release'
cargo xtask smoke --suite firmware --rtos-elf "$fw/vs-control" --service-elf "$fw/vs-service" --timeout-secs 40
```

Add `--dry-run` to stage both images, print load sizes, independently validate
the manifest and build HS without starting QEMU. To use the resident monitor,
replace `smoke --suite firmware` with `run` and omit `--timeout-secs`.
Both VMs autostart. Try `vm attach service`, press `p` for a pong, then
Ctrl+] to detach. `vm stop service` is asynchronous; wait until
`vm status service` shows `Stopped`, then `vm start service`.

### Clone a running service

On a manifest board with a runtime RAM pool, use the resident `hv>` monitor.
No Linux, management VM, disk or new guest binary is required:

```text
hv> vm create service worker
hv> vm start worker --attach
     (press p for a pong, then Ctrl+] to detach)
hv> vm stop worker
hv> vm status worker
hv> vm destroy worker
```

Wait for `Stopped` with no join error before destroying it. The original service
keeps running. The child starts from the immutable firmware and DTB, not a
snapshot of the live service. It gets private RAM, page tables, UART and IRQ
state. Its single vCPU, linked addresses, RX/RO/RW layout and RAM size are
unchanged; firmware cannot use the Linux reshape or disk-assignment facilities.
`vm capacity` reports the ID ceiling, not a promise that more RAM is available.

## Interrupt-only UART service

For a service with **no guest timer**, use the third ELF from the same build.
From the `pauli` repository root, with `$fw` as above:

```powershell
cargo xtask smoke --suite firmware-irq --rtos-elf "$fw/vs-control" --service-elf "$fw/vs-irq-service" --timeout-secs 40
```

The service uses the default virtual PLIC and 16550 UART. It enables only
external interrupts locally, executes WFI, then claims the UART interrupt,
drains RX, and completes the claim. It does not program TIME or poll the
UART for input while idle. The peer RTOS still has its own periodic timer.

To try it interactively, replace `smoke --suite firmware-irq` with `run`
and omit the timeout. At `hv>`, `vm status service` shows both the SBI
lifecycle (`state=Started`) and the scheduler state (`run=Waiting` when
parked). Use `vm attach service`, type a printable byte without Enter and
watch its sequence number. Ctrl+] returns to the monitor without stopping
the VM. For the restart check, type `!`, wait for `hold-next`, then type
another byte. After `held-unread`, the service deliberately leaves that byte
in its old UART until stopped. Cold start must restore an empty FIFO and
restart the sequence at one without resetting the RTOS peer.

The single-hart gate also creates a stopped clone of that running service.
It checks that the clone acknowledges its first byte at sequence 1 and the
original continues at sequence 3, then stops and destroys the clone.

In a single-hart, 32 MiB QEMU run with networking disabled and no disk,
the service cold-restarted from epoch 2 to 7. Its clone (epoch 8) acknowledged
its first byte at sequence 1; the original then acknowledged sequence 3.
The clone was stopped and destroyed. The RTOS stayed in epoch 1 with release
counts 64, 128 and 192 across the original service's stop and restart.
The gate reported `smoke firmware-irq: PASS` and QEMU exited normally.
This verifies same-hart wakeup and instance isolation, not IRQ latency.

### Put the service on another hart

```powershell
cargo xtask smoke --suite firmware-irq-smp --rtos-elf "$fw/vs-control" --service-elf "$fw/vs-irq-service" --timeout-secs 40
```

Same ELFs and private RAM, two physical harts. The monitor and RTOS run on
whichever hart OpenSBI boots; the service uses the other one. The gate reads
the actual owners from the host, not from a presumed boot hart number.
It includes the existing host `selftest` observations without changing guest
ISA or booting the standalone selftest payload.

Before each input, the gate requires both a waiting guest vCPU and a native
worker parked with its timer disarmed. After the correct byte acknowledgment,
the counter of software-interrupt-only returns from that worker's timerless
WFI must have advanced. A response merely noticed at the next periodic tick
does not pass. A count is not an IRQ latency measurement or a one-to-one
count of guest bytes. Stop, an intentionally unread byte, cold restart, and
continued RTOS releases use the same checks as the single-hart case.

For a resident run, use `cargo xtask run --host-cpus 2 --rtos-elf "$fw/vs-control" --service-elf "$fw/vs-irq-service"`.
The normal build has no worker observation counters. Add `--dry-run` to a
launch to validate the topology and build without starting QEMU.

In the first two-hart QEMU run OpenSBI booted hart 1; hart 0 hosted the
service. Epoch 2 acknowledged all four inputs with four software-only returns
from timerless WFI. After cold restart, epoch 7 acknowledged both new inputs
with two such returns. The RTOS stayed in epoch 1 (64, 128, 192 releases);
`smoke firmware-irq-smp: PASS` was followed by a clean exit. This is the
baseline **before** device notifications were narrowed to affected vCPUs,
not a measurement of latency, multi-vCPU fan-out or hardware IMSIC delivery.

For a paired measurement from HS publishing a device IRQ to the first VS trap
instruction, use the separate [UART interrupt latency gate](irq-latency.md).

## Shared-hart scheduling gate

The default smoke uses one physical hart and two single-vCPU VMs. In the
same VM epochs it measures **idle, busy, busy, idle** in that order. The bare
service normally waits on a timer; `b` starts integer work, `i` stops it and
`?` returns a fixed-width work counter. It does not print continuously.

After the service acknowledges its mode, the controller requests a timing
anchor from the RTOS. Standalone continuity checks still use `s`.
The periodic task returns an anchor at its next
32-release boundary. Only the **next two consecutive full windows** count.
Each round reports both window maxima (TIME ticks), maximum lateness in
whole microseconds and newly missed 20 ms periods. Bracketing counter
queries verify that the busy peer made
progress and the idle peer did no counted work; they are not a throughput
measurement. After the scored rounds the gate commands busy mode and reads
the old epoch's work counter again: it must actually advance before stop.
Cold boot must reset that nonzero counter to zero while the original control
epoch continues producing fresh periodic windows.

Current firmware timing gates require the `[rtos] timing-v1` boot marker;
rebuild the local `vs-control` ELF if an older probe is rejected. Before a
timing anchor (`t`), HS must observe the current control vCPU on hart 0 in
`run=Waiting`. This proves the real RTOS idle path returned control to HS
via WFI at least once in that peer mode, **not** on every release or for a
particular duration. It does not require selftest counters. The two sets of
32 guest observations are published after both windows. The reported bins
are disjoint: <2 ms, 2 to <10 ms, 10 to <20 ms, and >=20 ms, relative to
the guest's scheduled release time. They count **observations**, whereas one
late observation can skip multiple 20 ms periods. A release may be late
without skipping a period. Neither count measures a host context switch.

Add `--features selftest` to the same command for a diagnostic build. On a
single hart it also reports the elapsed native TIME ticks and change in WFI return counts with
timer, software, external, multiple, or no enabled IRQ pending. The snapshots
bracket the round, including the serial handshake; they are not per-release
wake attribution, nor do they count guest exits while HS was not in WFI.
Use the default build to compare guest latency, and compare like builds.
There is no hard-real-time threshold. Guest tick quantization, virtual UART
and emulator/host scheduling are all included.

With the uninstrumented HS (`608bdd2d`), RTOS (`28f0dfa4`) and service
(`763813d1`), a single-hart, 32 MiB QEMU run per suite gave the following
per-round peaks in ms (64 task releases per round, 10 MHz TIME):

| Peer policy | Round 1 | Round 2 | Round 3 | Round 4 | Skipped periods |
| --- | ---: | ---: | ---: | ---: | ---: |
| Idle throughout | 7.024 | 7.301 | 7.409 | 7.236 | 0/256 releases |
| Idle, busy, busy, idle | 8.094 | 8.481 | 8.436 | 8.605 | 0/256 releases |

All 512 releases were 2 to <10 ms late. The RTOS was observed waiting
before every round; the two busy rounds separately advanced the peer by
679,903,232 and 667,451,392 loop iterations. Both boots passed the peer
stop/cold-restart check. Busy work was not necessary for release lateness,
and the highest peak in this ABBA run was in its final idle round.
These are two emulator runs, not a hardware timing guarantee.

A separate deterministic 1 MHz dispatcher test removes both native sleep
and the RTOS tick. With the old host a waiter due at 500 us had to wait
for its peer's 300 us slice to end at 700 us. The
[shared-hart slice policy](architecture.md) now returns to dispatch at
500 us; with these two vCPUs the waiter reenters at 500 us. This eliminates
200 us **in the model**, not 200 us from every release in QEMU or on silicon.
To compare the same workload with a preserved local host binary, add
`--host-elf PATH` to `firmware` or `firmware-idle` without `--features`.

One old/new QEMU comparison used those same two guest ELFs and 2 ms
quanta. Each cell below is a round's worst release lateness in ms over 64
releases; host ELF prefixes identify the binaries, not source revisions.

| Host and suite | Round 1 | Round 2 | Round 3 | Round 4 |
| --- | ---: | ---: | ---: | ---: |
| Old `608bdd2d`, idle throughout | 8.433 | 8.609 | 8.375 | 8.566 |
| New `f754bafe`, idle throughout | 7.768 | 8.181 | 7.677 | 7.536 |
| Old, idle/busy/busy/idle | 7.868 | 8.112 | 8.090 | 10.884 |
| New, idle/busy/busy/idle | 8.918 | 6.626 | 6.911 | 8.222 |

All four runs completed peer stop and cold restart, with zero skipped periods
over 1,024 observed releases. Both busy rounds had lower maxima with the new
host, but the first idle round of that same run was worse. One run per cell
cannot establish a repeatable tail-latency improvement. These task-level
timestamps do not identify the instant that a particular VS timer fired.
Use the [VS timer-vector probe](vs-timer-latency.md) for a same-event
comparison of a programmed deadline with entry into the guest timer handler.

### Single-hart idle policy

With the original 100 us maintenance poll, one run per build gave the following
baseline (64 releases per round, two idle and two busy rounds per build):

| Build | Idle round maxima (ms) | Busy round maxima (ms) | Missed periods |
| --- | --- | --- | --- |
| Default | 10.653 / 10.704 | 10.325 / 10.396 | 0 |
| Selftest | 10.185 / 10.358 | 10.342 / 10.202 | 0 |

Selftest counted 2,506 timer-only and 57 external-only HS WFI returns during
3.740 s of idle-peer sampling, and no HS WFI returns in 3.721 s with a busy
peer. The idle peer itself requests a 1 ms timer; these are not counts of
wasted wakeups. The RTOS tick is 10 ms, so these window maxima do not isolate
HS preemption latency.

On an IRQ-driven console board with one hart and only diskless firmware,
HS chooses the earliest guest timer or unmasked RTC alarm of the waiting
VMs. A stopped VM contributes neither; the monitor still drains its UART
output. With everyone waiting or fully stopped and no device work due,
HS checks back within 1 ms even if there is no guest deadline. Outstanding
TX or a live guest's RX timeout shortens that bound to 100 us, without delaying
an earlier compare. Retirement still gets fast service. On QEMU an already
buffered native input byte skips the longer wait altogether.

In the 1 MHz dispatcher fixture, three unstarted clones alongside a peer due
in 800 us used to cause eight timer waits (100 us each); the same fixture now
programs just the peer's compare. This is a count of programmed deadlines,
not measured power or physical wake latency. RTC-only compares do not count
toward guest-timer lateness. Busy-guest slices, SMP, Linux and device-backed
profiles are unchanged.

Repeating the same ABBA smoke once per build with guest-timer-based idle
(before RTC alarms also supplied deadlines) gave:

| Build | Idle round maxima (ms) | Busy round maxima (ms) | Missed periods |
| --- | --- | --- | --- |
| Default | 10.395 / 15.286 | 10.219 / 10.315 | 0 |
| Selftest | 10.702 / 16.079 | 10.420 / 10.257 | 0 |

The idle-peer timer-only rate fell from 670 to 274 returns/s (1,018 over
3.717 s; sum counts and intervals before dividing). The two new idle rounds
differed: 472/s before the busy rounds, 64/s after them. Both builds had
their largest lateness in that last idle round. Both passed peer stop and
cold restart. Fewer timer returns alone do not establish a latency or CPU
utilization improvement; one run per build cannot establish the cause.

To distinguish native wake lateness from time spent after HS wakes, subsequent
selftest runs also report four counts: less than 1 ms, 1 to <5 ms, 5 to <10 ms,
and at least 10 ms after the programmed guest deadline. These are disjoint
subsets of timer-only WFI returns: the compare had to match the earliest
enabled guest timer and be due when sampled. A maintenance-only arm, early
return, or mixed pending interrupt does not enter these bins. They describe
the shared hart, not which VM was resumed or when its RTOS task ran.

On a repeat with the same two ELFs (Windows QEMU, 10 MHz TIME), the default
ABBA round maxima were 10.858 / 10.305 / 10.416 / 15.680 ms; selftest measured
10.405 / 10.709 / 10.542 / 15.888 ms. Neither run missed a period.

| Selftest round | Timer-only returns/s | <1 ms | 1 to <5 ms | 5 to <10 ms | >=10 ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| First idle | 479 | 564 | 352 | 0 | 0 |
| Last idle | 64 | 0 | 0 | 1 | 115 |

In this sample the bins cover all 916 and 116 timer-only returns respectively.
In the last round, the delay was already visible in HS before the next
dispatch. This does not separate SBI programming, the native WFI path and
emulator/host scheduling, or pair an individual return with a task release.

For a control run, change only `--suite firmware` to `--suite firmware-idle`.
It uses the same one-hart launch, four windows of 64 releases, mode ACKs and
post-measurement busy-stop/cold restart, but keeps the peer idle through all
four rounds. A nonzero work count before the first busy command fails the run.
Compare the last idle rounds from the two suites with the same build and ELFs.
Selftest prints native `start_ticks` and `interval_ticks` to check their time
placement. A similar tail without preceding busy work would show that a busy
peer is not necessary for the delay; it would not by itself identify its cause.

One paired selftest run used identical ELFs, hart count, QEMU arguments and
10 MHz TIME. The last two columns count returns at least 10 ms late over all
classified guest-deadline returns; a busy peer gives HS no idle samples.

| Round | AAAA interval (s) | ABBA interval (s) | AAAA / ABBA task maxima (ms) | AAAA >=10 ms | ABBA >=10 ms |
| --- | --- | --- | --- | ---: | ---: |
| 1 | 0.10-2.01 | 0.16-2.05 | 10.377 / 9.322 | 0/943 | 0/863 |
| 2 | 2.03-3.95 | 2.11-4.00 | 23.223 / 13.721 | 75/447 | n/a |
| 3 | 4.00-5.87 | 4.13-5.92 | 14.972 / 8.495 | 118/121 | n/a |
| 4 | 5.91-7.79 | 6.01-7.81 | 15.713 / 15.770 | 121/122 | 116/117 |

AAAA recorded **one missed period** (round 2); ABBA recorded none, over 256
scored releases each. Both completed peer stop/cold restart. The late returns
therefore do not require preceding busy work. This is an emulator measurement,
not a bound for silicon. The smoke prints timing misses separately from the
lifecycle result; neither late samples nor misses are silently discarded.

### OpenSBI-native timer reference

From the `pauli` repository root:

```powershell
cargo xtask smoke --suite native-timer --timeout-secs 40
```

This is a separate one-hart S-mode payload, not an HS monitor command. It
boots on the same 32 MiB QEMU virt CPU/OpenSBI setup with no guest ELFs, disk,
network or VMM. Use `--dry-run` to build and inspect the ELF/command without
starting QEMU. Each iteration programs an absolute TIME deadline 1 ms ahead
with global interrupts masked, reads TIME after SBI returns, executes WFI,
then samples the timer pending bit and TIME. The four reporting windows end
at 2/4/6/8 s of machine TIME, not after a prescribed number of samples.
The initial window starts after boot; serial summaries are outside the loop.

Compare native `ge10ms` to the firmware late bins over similar intervals.
`arm_ge10ms` is the subset already at least 10 ms late when SBI returned.
If native wakeups also show the tail, the VMM is not necessary to reproduce
it. If they do not, the guest/monitor workload and the remaining platform
differences still need to be separated before attributing it to HS scheduling.

The direct probe reproduced the tail on the same Windows QEMU/OpenSBI setup
without booting Pauli: 0/908, 72/471, 129/129 and 129/129 returns were at
least 10 ms late in the four successive windows. All 1,637 returns had STIP
pending, with no early wakeups. None of the 330 long returns was already
10 ms late at the post-SBI sample. This rules out VM dispatch as a necessary
cause on this setup, not SBI time below that threshold, emulator/host
scheduling, or a separate VM-specific cost. Do not shorten vCPU slices on
that evidence or use these timings as a board deadline guarantee.

## Port a firmware

- Supply a static little-endian RV64IMAC ET_EXEC with soft-float ABI. F/D
  is neither advertised nor switched for firmware. Entry is the first RX byte at
  `guest_base`; load and virtual addresses must agree. Loadable segments
  are ordered RX, optional R, optional RW; permission changes require 4 KiB
  boundaries. BSS and unused private RAM are zero at each cold boot.
- Entry ABI: `a0=0` (guest hart), `a1=guest DTB GPA`, VS, paging off.
  The final 16 KiB of private RAM is reserved read-only for the DTB. Use
  the virtual UART and SBI TIME, not physical SoC registers or M-mode CSRs.
- The diskless ABI supports single-vCPU boot VMs and cold clones of their
  immutable firmware on boards with a runtime RAM pool. No initramfs, block
  or network for firmware. Linux profiles and the trusted Wave8 OoO embedded ABI
  are separate.
- For your own images, use `cargo xtask run --vm my.toml`. Each firmware VM
  specifies `firmware` (path relative to the TOML), `firmware_sha256` of the
  **original ELF**, `guest_base` and either `memory_kib` or `memory_mib`.
  Firmware RAM is 4 KiB granular, up to 2 MiB, and must fit the ELF's
  memory spans, a writable page and the DTB. Linux retains its 64 MiB minimum.
- The example assigns 256 KiB to control and 32 KiB to service. The default
  QEMU firmware-only launch supplies 32 MiB **physical** RAM, not 32 MiB per
  VM or a measured minimum for silicon. Printed load spans include padding;
  linked RAM excludes native tables, device state and firmware reservations.
