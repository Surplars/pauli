# From a VS timer compare to the guest trap vector

Use this diskless single-hart gate to distinguish virtual timer delivery from
RTOS task-release lateness. Both guests are bare RV64 firmware; the HS binary
is the normal, uninstrumented build. From the OpenIon RTOS repository root:

```powershell
cargo build --locked --offline --release --target riscv64imac-unknown-none-elf -p openion-vs-example --bin vs-timer-latency --bin vs-service
cd ../pauli
$fw = '../rtos/target/riscv64imac-unknown-none-elf/release'
cargo xtask smoke --suite firmware-timer --timer-elf "$fw/vs-timer-latency" --service-elf "$fw/vs-service" --timeout-secs 40
```

Add `--dry-run` to stage and check the launch without running QEMU. For a
version comparison, supply `--host-elf PATH` and keep both guest ELFs and
the machine configuration identical. The gate checks the host's 10 MHz
TIME and the two VM placements; it needs no disk, network or Linux guest.

Each request arms SBI TIME for **1 ms after a new guest TIME read**, with
global guest interrupts masked. `armed` is read immediately after the SBI
call returns. The guest enables timer interrupts and waits; `trap` is the
TIME read by the vector's **first instruction**. Only a supervisor timer
interrupt for the current generation can finish the wait. Every sample has
its own deadline; this measures one-shot delivery, not periodic throughput.

| Value in native TIME ticks | What it measures |
| --- | --- |
| `trap - due` | Total lateness observed by the guest handler |
| `max(armed - due, 0)` | Lateness already present at the SBI return observation |
| `trap - max(armed, due)` | Remaining interval, including any scheduling, wake and VS entry |

The timer's pending state may already be visible before WFI. `pc=pre`
means the interrupted guest PC points to WFI; `pc=post` means it points
to the following branch. Neither proves physical sleep. The gate retains
early-arm and late-arm observations, and also reports the subset armed
before expiry with `pc=post`. There is no HS timestamp or claim that
the final interval is pure hypervisor execution time.

The peer runs idle, busy, busy, idle in the same epochs. Each visit has
four warm-ups and 20 scored events, buffered before any guest UART output.
One unit at 10 MHz is 0.1 us. P50, P95 and max preserve the components of
the same total-ranked event. Peer work counters bracket the visit, not
individual interrupt handlers. The probe then shuts down, cold-boots and
delivers another 24 timers starting at generation one. These last 24 do not
enter the distributions. Accept results only after the final smoke PASS.

## First shared-hart comparison

Same probe (`5b00251b`) and peer (`763813d1`), one boot per host, default
2 ms quantum. Each cell is P50 / P95 of **lateness after the requested
deadline**, in microseconds (not time since the call to arm the timer).

| Round | Old host `608bdd2d` | Peer-deadline host `f754bafe` |
| --- | ---: | ---: |
| Idle 1 | 560.5 / 1701.0 | 855.7 / 2004.9 |
| Busy 1 | 1240.1 / 1395.4 | 224.4 / 1281.4 |
| Busy 2 | 1193.4 / 1316.0 | 170.1 / 1264.1 |
| Idle 2 | 1117.5 / 2096.6 | 1006.3 / 1585.4 |

All 160 scored events were armed before expiry and interrupted the PC
following WFI. On the old host all 40 busy events were at least 1 ms late.
On the new host each busy visit produced ten successive fast/slow pairs.
Across both visits the fast
range was 60.0-224.4 us and the slow range 1113.3-1362.7 us. Thus the
nearest-rank P50 (10th of 20) picks the fast group, but P95 stays in the
slow one. This agrees with the scheduler rule granting a full turn after
a clipped turn; the guest does not observe that decision directly. Idle
results moved in both directions. This single pair of emulator boots does
not establish a percentile for hardware. Both hosts passed clean shutdown,
probe cold restart and independent peer continuity.

## Repaying only the unused slice

The previous host protected the entire next turn after a peer deadline cut a
slice short. The new host protects only the unused part. For example, a vCPU
that used 100 us of a 300 us slice is owed 200 us, not 300 us. This is a
minimum on its next runnable visit, not a cap if no peer timer is due.
Parking or retiring the vCPU clears the balance; a new epoch starts at zero.

The single-hart comparison below uses the same guest binaries as above, a
2 ms quantum, and four independent uninstrumented host boots in A/B/B/A
order. Each busy P95 is the 19th of 20 events within that visit; the last
two columns count events across both visits of the indicated peer mode.

| Boot | Host (SHA-256 prefix) | Busy 1 P95 (us) | Busy 2 P95 (us) | Busy >= 1 ms | Idle >= 1 ms |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | Full next turn, `f754bafe` | 1238.7 | 1221.6 | 20/40 | 12/40 |
| 2 | Unused part, `6600e63d` | 261.9 | 310.7 | 0/40 | 20/40 |
| 3 | Unused part, `6600e63d` | 371.6 | 279.3 | 0/40 | 19/40 |
| 4 | Full next turn, `f754bafe` | 1247.5 | 1326.4 | 20/40 | 19/40 |

The old host alternated below/above 1 ms throughout all four scored busy
visits. On the new host none of the 80 busy events reached 1 ms; the worst
was 425.7 us. All 320 scored events across both hosts armed before expiry
and had `pc=post`. Busy peer work advanced in every visit. Idle events at
least 1 ms late increased from 31/80 to 39/80: these runs do not show an idle
latency improvement, establish a throughput figure, or set a hardware bound.
Each probe cold restart delivered 24 fresh interrupts from generation one
without restarting the peer.

A separate run on the new host used the real RTOS control guest. Across four
64-release visits there were zero missed periods; worst task-release lateness
was 6.549 ms (idle peer) and 4.110 ms (busy peer). The peer stopped and cold
restarted while the control guest continued. These are task-release results,
not VS trap-entry times. Raw timepoints, host and guest hashes and gate results
for this run are in the local artifact `target/vstip-credit-01.json` (under the workspace root); generated artifacts are not required to run the gate.

## Three or more vCPUs on one hart

Two guests alone cannot detect the next runnable guest taking an entire
quantum *after* a different guest's compare has expired. The deterministic
dispatcher test drives one 1 MHz clock through 120 real dispatch calls. Each
modeled guest entry costs 100 us, the quantum is 300 us, and the waiter
rearms 100 us after parking. These are model times, not QEMU measurements:

| Placement | Worst waiter admission lateness, before / after | Busy service, before / after |
| --- | ---: | --- |
| Waiter first, two busy peers | 400 / 100 us | 6000:12000 / 4500:4500 us |
| Waiter last, four busy peers across two placement groups | 1000 / 100 us | 3700:7200:7200:7200 / 2500:2500:2400:2400 us |

The test fixes the number of dispatches, **not** the duration; compare the
distribution between peers, not total throughput between hosts. Five
always-busy guests each receive 24 full quanta with or without handoff.
Two competing timers can still be late by 500 us in this model.

To measure three real VS guests, build the isolated worker and timer probe
from the repository root (neither requires Linux):

```powershell
cargo build --locked --offline --release --target riscv64imac-unknown-none-elf -p openion-vs-example --bin vs-timer-latency --bin vs-fair-worker
cd ../pauli
$fw = '../rtos/target/riscv64imac-unknown-none-elf/release'
cargo xtask smoke --suite firmware-timer-multi --timer-elf "$fw/vs-timer-latency" --service-elf "$fw/vs-fair-worker" --timeout-secs 40
```

The suite clones one worker and checks the same probe with both peers parked,
busy, busy and parked. An idle worker waits only for its virtual UART. A busy
worker samples TIME after every 16,384 loop iterations; its maximum gap
includes batch execution **and** time off the hart. Each worker reports an
outer window enclosing the probe's 20 scored events. If the scored duration
is `d` and the largest enclosing checkpoint gap is `g`, there must be at
least `ceil(d/g) - 1` interior checkpoints. The gate requires that number
to be positive for *each* worker. It also stops a busy clone, cold-boots it,
and checks that the surviving worker and timer continue in their original
epochs. Neither batch counts nor this progress bound measure CPU share.

QEMU observed results (one uninstrumented boot per host, guest ELFs held
fixed, 20 scored requests per visit, first four excluded):

| 3-vCPU host | Two busy peers: P95 visit 1 / 2 | Busy events >= 1 ms late | Parked events >= 1 ms late | Largest busy-worker checkpoint gap |
| --- | ---: | ---: | ---: | ---: |
| Before `6600e63d` | 2350.9 / 2329.8 us | 40/40 | 16/40 | 4.7564 ms |
| Handoff `0d58010d` | 265.1 / 309.3 us | 0/40 | 19/40 | 4.9501 ms |

Each busy worker proved strictly interior progress in each scored visit,
including on the faster host where the common timer window was shorter.
Guest trap latency improved; neither the idle result nor the worker maximum
supports a general scheduling-latency improvement. Both three-vCPU runs
stopped and restarted the clone (epoch 7 -> 9), with control and source
remaining in epochs 1 and 2. The existing two-vCPU suite also passed on
the new host (busy P95 263.3 / 316.8 us, 0/40 at least 1 ms late; probe
cold restart delivered 24 fresh interrupt receipts). The local run artifact
`target/vstip-multi-01.json` holds all 288 reported raw timepoints,
worker windows, hashes and final gate results. These are not hardware bounds.

## Simultaneous periodic timers

`firmware-timer-cohort` runs two periodic VS guests and three independent
workers on one physical hart. Slot order is timer, worker, worker, timer,
worker: simple round-robin can insert unrelated slices between two handlers
whose deadlines are identical. From the OpenIon RTOS repository root:

```powershell
cargo build --locked --offline --release --target riscv64imac-unknown-none-elf -p openion-vs-example --bin vs-timer-cohort --bin vs-fair-worker
cd ../pauli
$fw = '../rtos/target/riscv64imac-unknown-none-elf/release'
cargo xtask smoke --suite firmware-timer-cohort --timer-elf "$fw/vs-timer-cohort" --service-elf "$fw/vs-fair-worker" --timeout-secs 40
```

Each visit uses a shared future start and a 10 ms period. Both guests must
return from SBI TIME before **every** matching absolute compare; a stale,
late or missing side invalidates the visit. Four warm-ups precede 20 scored
pairs, followed by one unscored guard before either guest prints. The gate
reports each guest's trap lateness and the absolute difference between the
two trap timestamps. It repeats with the workers parked, then busy twice
with opposite arm-command order. Each busy worker must prove progress
inside the common scored window. Statistics are observations, not pass
thresholds or hardware bounds.

Separately, the real dispatcher on the 1 MHz deterministic clock receives
24 pairs for each of ten slot pairs at periods 1700, 2000, 2100 and 2350 us:
960 pairs per policy. Worst admission lateness across this sweep dropped
from 750 us to 150 us. Each busy lane made progress between successive
completed pairs. A CPU-bound handler and a guest that re-parks without
consuming its timer cannot extend the shared turn. Four coincident short
handlers do not receive four fresh quanta: in the 300 us budget only three
100 us entries fit before the next regular busy guest. These are model
admission times, not guest trap observations.

One 32 MiB QEMU boot per host, same probe and worker ELFs. The two busy
visits below reverse which timer receives the arm command first. Values are
P95 in **microseconds**, calculated independently over each visit's 20 pairs:

| Busy metric | Reference `0d58010d` | Shared budget `6511f325` |
| --- | ---: | ---: |
| Timer A lateness | 125.3 / 86.2 | 88.9 / 88.7 |
| Timer B lateness | 4252.9 / 4253.7 | 279.2 / 247.4 |
| Absolute skew between handlers | 4169.1 / 4181.3 | 148.9 / 158.7 |

Skew reached 1 ms in 26/40 busy pairs on the reference host, 0/40 on the
candidate. Each of the three workers made progress in each measured busy
window. Parked-worker skew P95 was 189.4 / 219.7 us (reference / candidate):
no idle improvement was observed. The candidate also passed the independent
three-VM regression, including clone stop and cold restart. The local
`target/vstip-cohort-01.log` retains both clocks per pair, worker
evidence, binary hashes and pass markers. These few emulator visits do not
establish a hardware latency bound or a CPU-throughput improvement.

## One periodic timer with three workers on the other hart

`firmware-timer-smp` asks whether the periodic VS guest continues to receive
timer interrupts while another hart is overcommitted and one of its VMs is
cold-restarted. The timer runs on the OpenSBI-selected boot hart (also the
management hart); three workers share the other hart. Only the probe and one
worker firmware image are loaded: the monitor creates two private worker VMs
from that template. From the OpenIon RTOS repository root:

```powershell
cargo build --locked --offline --release --target riscv64imac-unknown-none-elf -p openion-vs-example --bin vs-timer-cohort --bin vs-fair-worker
cd ../pauli
$fw = '../rtos/target/riscv64imac-unknown-none-elf/release'
cargo xtask smoke --suite firmware-timer-smp --timer-elf "$fw/vs-timer-cohort" --service-elf "$fw/vs-fair-worker" --timeout-secs 40
```

The default host uses two harts, 32 MiB, no disk and no network. Add
`--dry-run` to check placement inputs and stage the images without launching
QEMU. The live gate verifies the owners and runs the same 10 ms grid three
times: all workers parked, all busy, then all busy with one cold restart.
Each visit records 4 warm-ups, 20 scored releases and a guard. The timer
must arm each release before its deadline; busy workers must show progress
inside the scored window (inside its remaining part for the restarted worker).

In the restart visit the workers' `n` command reads the same unoffset 10 MHz
TIME as the timer guest, before writing its UART reply. Four samples bracket
the monitor operations: the old worker is live; a survivor observes TIME
after the old epoch is stopped and joined; the survivor samples again before
start; the freshly booted worker samples after starting work. The gate
requires a complete scored due/trap pair **before stop**, **while joined**
and **after restart**. A changed epoch or a successful command alone does
not prove that a timer interrupt ran during downtime. Host sleeps only pace
requests and do not count as timing evidence.

`cargo xtask test` also exercises two dispatchers on one deterministic model
clock, advancing them in sequence. That checks owner selection and lifecycle
rules, not simultaneous execution or guest trap latency. In QEMU, use the
per-visit P50/P95/max (`trap - due`) only after the final smoke PASS; keep
the restart visit separate because monitor and witness traffic are part of
that workload. Neither the model nor an emulator run sets a hardware bound.
