# Virtual UART interrupt latency

`firmware-irq-latency` measures a virtual device interrupt delivered to a VS
firmware handler. A control RTOS runs on the boot hart; a single-vCPU firmware
service owns the other hart. The guest interrupt controller is a **PLIC**.
This is a separate guest from `vs-irq-service`, which deliberately leaves
global interrupts disabled and reads the PLIC claim after WFI.

Build the guest artifacts in the example's workspace, then run the independent
hypervisor. There is no disk, network, Linux image or management VM involved.

```powershell
# From the OpenIon RTOS repository root:
cargo build --offline --locked --release --target riscv64imac-unknown-none-elf -p openion-vs-example
cd ../pauli
$fw = '../rtos/target/riscv64imac-unknown-none-elf/release'
cargo xtask smoke --suite firmware-irq-latency --rtos-elf "$fw/vs-control" --service-elf "$fw/vs-irq-latency" --timeout-secs 60
```

## What the interval includes

- **Start:** HS samples native TIME in the VM's IRQ-chip critical section,
  immediately before publishing the UART RX level change caused by this byte.
  This excludes serial transport to HS. The sample exists only in selftest.
- **Middle:** the pinned owner first reads the newly eligible external line
  under that same IRQ-chip lock. A waiting vCPU may do so in the wake check;
  a runnable vCPU does so on the way back into VS. Only a UART rise that
  changed the sole PLIC context from not pending to pending can be paired.
  A chip transaction before the owner samples cancels the candidate; a
  repeated read after the first observation cannot replace its timestamp.
- **End:** the first instruction at the VS trap vector reads TIME, before
  claim or UART MMIO. HS and VS use the same counter (`htimedelta=0`).
  The firmware checks the external trap cause, the PLIC source and the
  interrupted PC before reporting the sample. Printing happens after sampling.
- **Pairing:** the controller allows one input at a time. It matches the VM
  epoch, input byte, HS edge sequence, vCPU, physical owner and guest sample
  sequence, and requires `rise <= owner <= trap`. Mode commands
  cause real RX edges too, but are not latency samples. A cold restart must
  start fresh sequences.

The waiting case requires both a parked guest and a remote worker in a
timer-disarmed WFI bracket before input, and a fresh software-interrupt-only
return afterward. The busy case requires a saved guest PC in a register-only
compute loop. This shows *what the interrupt interrupted*, not that the hart
was executing VS at the exact instant the producer published the edge.

The first segment includes notification, any owner wake or guest exit, and
dispatch up to the line check; it does not isolate the physical IPI. The
second includes the remaining owner work, guest entry and the VS trap.
For the slowest end-to-end sample the gate reports both of *its* segments;
independent stage maxima or medians must not be added. These measurements
do not cover a physical device pin, Linux or IMSIC. A small QEMU sample is
not a p99 estimate or a real-time guarantee.

## First QEMU baseline

One diskless two-hart QEMU run (32 MiB, 10 MHz TIME) passed under the 60 s
deadline. The service ran on owner 1. Epoch 2 sampled wait / busy / busy /
wait, eight samples per round. P50 is the nearest-rank 50th percentile.

| Interrupted guest | Samples | P50 | Observed maximum |
| --- | ---: | ---: | ---: |
| WFI loop, worker wake verified | 16 | 1,112 ticks (111.2 us) | 2,417 ticks (241.7 us) |
| Busy loop, guest progress verified | 16 | 1,104 ticks (110.4 us) | 1,972 ticks (197.2 us) |

Cold restart 2 -> 7 passed with fresh wait and busy samples; the control RTOS
remained in epoch 1. The two medians are close. The single larger wait maximum
does not identify a WFI bottleneck; split owner wake from guest dispatch, or
repeat the run on hardware, before changing the idle policy.

## Paired owner observation

A subsequent run with the same firmware and topology, now recording the
middle timestamp, passed all 32 scored samples and the unscored cold-restart
checks (service epoch 2 -> 7, control epoch 1). Times below are microseconds
at 10 MHz, with 16 samples per mode.

| Interrupted guest | Rise to owner P50 / max | Owner to VS P50 / max | End-to-end P50 / max | Segments of the slowest end-to-end sample |
| --- | ---: | ---: | ---: | --- |
| Waiting | 98.3 / 160.7 | 42.8 / 116.3 | 144.7 / 277.0 | sample 28: 160.7 + 116.3 |
| Busy | 109.1 / 175.6 | 36.4 / 144.6 | 147.2 / 236.1 | sample 9: 175.6 + 60.5 |

In both modes the longer median segment preceded the owner's first positive
line check. This narrows the next investigation to notification and owner
dispatch; it does not distinguish SBI IPI delivery from emulator scheduling,
nor is it a before/after benchmark of the idle policy.

## OpenSBI-direct IPI control

From the `pauli` repository root, run:

```powershell
cargo xtask smoke --suite native-ipi --timeout-secs 40
```

This boots one native supervisor ELF directly under OpenSBI. It uses the
same two-hart QEMU virt CPU model, 32 MiB RAM, disabled networking and no
disks. There is no VM, virtual UART, PLIC claim or vCPU dispatcher. The boot
hart sends one SBI IPI at a time to the other hart discovered in the DTB.
Samples follow the same wait / busy / busy / wait order as the VM gate.

The start is native TIME just before sending the IPI. The receiver reads TIME
at its first supervisor trap instruction, before clearing SSIP. The sender
also records when its own SBI call returns: receipt can precede that return,
so these are overlapping intervals. Wait samples distinguish a receiver armed
at WFI from one interrupted at the post-WFI instruction; neither establishes
physical sleep residency. Busy samples require work in the compute loop.

Compare orders of magnitude, not differences between percentiles. The VM
measurement starts earlier (at a virtual device edge) and its middle
timestamp is later (an owner check of virtual interrupt eligibility).
The direct sender is not concurrently running the control RTOS. Neither
result isolates a physical IPI wire delay or removes QEMU/host scheduling.

One 40 s bounded run passed, hart 0 sending to hart 1 at 10 MHz. The receiver
stopped via SBI HSM before the sender reported completion. Values are in
microseconds (16 samples per mode, no warm-up discarded).

| Receiver | Send to S trap P50 / max | Send to SBI return P50 | Slowest delivery: sequence, matching sender return |
| --- | ---: | ---: | --- |
| Wait | 24.3 / 767.5 | 10.7 | #1, 674.6 |
| Busy | 18.8 / 38.0 | 6.3 | #10, 6.4 |

All wait samples interrupted at the post-WFI PC, and all 32 traps occurred
after the sampled SBI return in this run. The largest wait sample was the
first IPI: the sender itself took 674.6 us to return. That does not establish
why it was slow or identify a recurring tail. Native median delivery was
well below the VM rise-to-owner medians (98.3 and 109.1 us above), so do not
assume the native IPI alone explains the typical VM interval. A useful next
comparison is to hold hardware and guest workload fixed while varying the
number of idle vCPUs assigned to the target owner.

## Fixed-population WFI comparison

With the same two local ELFs and working directory as above:

```powershell
cargo xtask smoke --suite firmware-irq-scale --rtos-elf "$fw/vs-control" --service-elf "$fw/vs-irq-latency" --timeout-secs 120
```

The controller reserves eight service clones **once**. With both boot VMs
still present, the directory has ten allocated VMs throughout the measurement.
It then starts or stops clones on the active service's hart to compare
0, 4, 8, 4 and 0 waiting peers in that order. Zero means eight *stopped*
reservations, not an empty directory. Every plateau checks each clone's
epoch, owner, run state and serial transcript before and after sampling;
no input is sent to a clone. Creation, transitions and status checks are
outside the rise-to-trap interval.

The active service keeps its epoch and physical owner. Wait and busy samples
use the same ABBA sequence and timestamp checks as the original latency gate.
Each population contributes 16 samples per mode across its visits; compare
both visits to see drift rather than averaging their medians. After the sweep,
all eight reservations are removed and the service cold-restart check runs.
The ignored release-mode `pauli-core` parked-dispatch benchmark measures the
cost of a dispatcher iteration separately; its host nanoseconds are not
RISC-V interrupt latency.

For a replayable trace, append `--samples target/irq-samples.csv` to either
firmware IRQ latency command. Use a new filename each time, relative to the
invoking directory. Only a completed run (including restart checks and clean
QEMU exit) exports the CSV. Dry runs do not write it. This is a host output
option; no extra clocks, log calls or hooks run on the guest or hypervisor.

Each row is one **scored** input, in chronological order: plateau number,
reserved/waiting peer counts, service epoch, physical owner, TIME frequency,
guest mode, input and guest sample sequence numbers, and the absolute rise,
owner and VS trap ticks. Mode switches consume input numbers but are not scored;
neither they nor the two restart checks have rows. The scale suite has 96 rows
and the single-population suite 32. TIME origins from separate QEMU runs are
unrelated; compare intervals, not their absolute ticks.

Each phase now reports the actual events at the nearest-rank P50, P75 and
maximum **total**. Their two segments add to their own total. Equal totals
are ordered by guest sequence; `equal_totals` says how many rows share that
value. Compare the two zero-peer visits as a drift check. In particular, at
eight waiting peers the busy owner-to-trap P50 in the before/after pair below
also fell (30.9 -> 28.8 us), even though the end-to-end P50 rose. The old
summaries cannot reconstruct the event that set that median.

One matched before/after pair passed on diskless QEMU (2 harts, 32 MiB,
10 MHz TIME). The change checks a quiet WFI context in its slot instead of
taking and restoring the whole context. No interrupt check was removed.
Numbers are microseconds, **before -> after**, with separate rows for each
visit. The active service stayed on hart 1, epoch 2. In both runs it then
cold-restarted as epoch 23, while the control RTOS remained in epoch 1.

| Waiting peers | n per mode | Rise to owner P50, wait | Rise to owner P50, busy | End-to-end P50, wait | End-to-end P50, busy |
| --- | ---: | ---: | ---: | ---: | ---: |
| 0 ascending | 8 | 85.4 -> 80.2 | 103.9 -> 104.7 | 138.5 -> 125.8 | 142.2 -> 141.1 |
| 4 ascending | 8 | 108.7 -> 88.8 | 93.7 -> 74.7 | 181.0 -> 129.9 | 159.9 -> 120.0 |
| 8 | 16 | 117.4 -> 92.8 | 130.6 -> 104.1 | 152.6 -> 133.7 | 162.8 -> 170.0 |
| 4 descending | 8 | 103.4 -> 91.4 | 98.8 -> 94.6 | 134.9 -> 132.7 | 135.5 -> 132.7 |
| 0 descending | 8 | 98.4 -> 81.6 | 112.6 -> 86.3 | 151.1 -> 115.7 | 147.1 -> 135.7 |

At eight peers the observed end-to-end maxima fell from 260.7 to 178.9 us
(wait) and from 298.1 to 227.7 us (busy). The busy median nevertheless
*rose*. Both zero-peer controls also drifted. This supports reducing work in
the owner path, not a general end-to-end speedup, a p99 or a hardware bound.

In the separate host release benchmark with eight slots reserved, dispatch
P50 with 0/4/8 waiting peers was 180.3/362.0/655.6 ns before and
175.8/233.1/388.3 ns after. The same three target entries and 0/4/8 parked
IRQ checks per slice were observed in both versions; no parked peer entered.

## First per-event replay

A further two-hart run of the unchanged optimized hypervisor exported the
96 scored rows. Re-ranking the CSV matched all 30 printed joint observations
(P50/P75/max for both modes at five plateaus). The service stayed on owner
1, epoch 2, then cold-restarted as epoch 23; the RTOS kept epoch 1.

At eight waiting peers the busy group had 16 events. Its ranked events were:

| End-to-end rank | Guest sample | Rise to owner (us) | Owner to VS trap (us) | Total (us) |
| --- | ---: | ---: | ---: | ---: |
| P50 (8/16) | 49 | 131.8 | 28.1 | 159.9 |
| P75 (12/16) | 48 | 130.0 | 38.9 | 168.9 |
| Max (16/16) | 51 | 153.5 | 38.4 | 191.9 |

The first segment was longer for all 16 of those busy events (47/48 busy
events across the sweep). The zero-peer busy P50 was 112.8 us on the way up
and 128.9 us on the way down. This suggests distinguishing notification/physical
kick from owner dispatch **before** changing VS entry. It neither explains a
specific historical event for which the raw timestamps were not saved, nor
measures real hardware IPI latency.

## Observing the shared physical doorbell

The same suites can now split the first segment further in a selftest build.
The remote, single-vCPU PLIC firmware probe records the start of an actual
device-wake send attempt and the target hart's ACK sequence. It confirms the
ticket only on successful send, **without** holding the IRQ-chip lock across
the SBI call. Capture the baseline under the chip lock, take `send` after
unlocking immediately before the native kick call, and `returned` just after
success before reacquiring the lock. This is the host call through the
RISC-V SBI wrapper, not the time spent executing OpenSBI alone. The target
can acknowledge or even trap in VS before the sender observes the return.

On RISC-V an ACK is recorded only when an atomic read-and-clear of physical
`sip.SSIP` returns a set bit. `guest-exit` means the guest exit handler
cleared it; `dispatch-ack` means the dispatcher cleared it before scanning
published work. Neither is the first instruction of the trap, or proof of WFI
residency. The HS vector may have masked delivery before the eventual clear.
The normal build retains its original clear with no ACK clock or counter.

An exact-epoch/input/owner match, a single sequence advance, and
`rise <= send < ack <= owner <= trap` are required. Earlier observations,
multiple ACKs, coalesced device wakes and failed sends cannot stand in for the
missing timestamp. The physical bit is shared: even a matched ACK is a
time bracket around an observed SSIP clear, **not proof that this UART alone
caused that interrupt**.

The CSV appends `ack_path,kick_send_ticks,ssip_ack_ticks,kick_return_ticks`
to the original twelve columns. All four are empty if unmatched; the main latency distribution
still includes that event. Per-path reports show coverage and the four segments
of the same median-total event **within that subset**. In particular,
send-to-ACK includes send preparation, firmware, any scheduling by QEMU, and
receiver work up to the positive clear. ACK-to-owner runs from that clear to
the first eligible virtual-line observation. Neither is pure IPI wire time.
For matched observations, group counts distinguish ACK before, at the same
tick as, or after the sender's return. A positive `ack_minus_return_ticks`
means the ACK was observed later. Send-to-return overlaps send-to-ACK; do
not add it as a fifth stage. Older 15-column CSVs have no return sample:
it cannot be reconstructed from them, and the old send timestamp included
unlock and fanout setup.

One diskless two-hart QEMU scale run (32 MiB, 10 MHz TIME) matched 90 of
96 scored events. All 48 waiting events were dispatcher ACKs. Of 48 busy
events, 31 were guest-exit ACKs, 11 dispatcher ACKs and six unmatched.
The unmatched events remained in the end-to-end ranks. With eight actual
WFI peers, the median-total event **within each matched subset** was:

| Interrupted service / ACK path | Subset n | Guest sample | Rise to send (us) | Send to positive ACK (us) | ACK to owner (us) | Owner to VS trap (us) | Total (us) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Wait / dispatcher | 16 | 38 | 3.2 | 67.6 | 20.6 | 38.6 | 130.0 |
| Busy / guest exit | 7 | 54 | 5.2 | 72.0 | 44.2 | 32.4 | 153.8 |
| Busy / dispatcher | 7 | 51 | 3.4 | 54.6 | 25.2 | 26.1 | 109.3 |

Across all plateaus, send-to-positive-ACK was the longest segment in 78
of the 90 matched events. That points upstream of owner selection for most
of this workload, not to the final VS entry. It does not establish that SBI
itself is slow or that fewer IPIs are safe. The 30 main ranks and 14 nonempty
path-subset medians were independently replayed from CSV. The service
cold-restarted (2 -> 23) without restarting the control RTOS (epoch 1).

### Sender-return comparison

A second diskless run using the send/return boundary above matched 87/96
events: 48/48 waiting (dispatcher ACK), 39/48 busy (36 guest exits, three
dispatcher ACKs). In all 87 matched events the sender sampled its return
**before** the receiver sampled its positive clear. The return-to-ACK gap
exceeded the duration of the kick call in 70/87 events. The two separate
medians were 28.5 us for send-to-return and 42.5 us for return-to-ACK; do
not add them as if they were from one event.

For example, with eight WFI peers the median-total waiting event (sample 37)
spent 29.4 us in the kick call, then another 45.4 us before the positive
ACK. Its full rise-to-trap interval was 145.1 us. The busy guest-exit subset
(11 events) had a median-total event (sample 45) with 22.7 us in the call,
41.5 us from return to ACK, and 165.2 us end-to-end. These gaps include
receiver-side work and QEMU scheduling; they do not tell when an IPI was
asserted, or what portion of that time would remain on hardware.

This UART probe is **not** a benchmark of guest SBI IPI or RFENCE. Their
separate control path now releases the destination mailbox lock before
kicking its physical owner. Cross-thread tests exercise that overlap directly;
the Linux two-vCPU PLIC smoke exercises routing, shutdown and cold restart,
not RFENCE latency.
