# Guest IPI and remote fence latency

Run from the standalone hypervisor directory:

```sh
cargo xtask smoke --suite mailbox --timeout-secs 40
```

This boots one bare VS guest on two pinned vCPUs and, by default, two QEMU harts. It
uses the standard RV64 Image entry, not a Linux kernel or the single-vCPU
static-firmware ABI. No disk, initramfs, network or host profiling is needed.
Use `--dry-run` to check the compiled images, hashes and command without booting.

Use `--host-cpus 1` to place **both** vCPUs on hart 0. The guest Image,
its two virtual harts, 64 MiB of private RAM, and its workload stay the
same. The verifier requires [0, 0] owners for one host hart and [0, 1]
for two, both before sampling and after retirement. This also works with
`--features selftest`; a single physical hart does not turn off guest SMP.

| Operation | What stops the guest's TIME clock interval |
| --- | --- |
| `ipi` | The sender has returned from SBI and observed the exact receipt from its peer. The peer polls and clears VS SSIP; this is not interrupt-handler entry latency. |
| `vma` | The SBI remote SFENCE.VMA call for the other hart has returned successfully. |
| `fencei` | The SBI remote FENCE.I call for the other hart has returned successfully. |

Requests do not overlap. The guest rotates the three operations, skips four
warm-ups per operation and records 24 samples each before printing anything.
The verifier checks 28 IPI receipts, the vCPU owners, guest shutdown and
retirement. It prints raw ticks, the timebase, nearest-rank P50/P95 and the
observed maximum. P95 is the 23rd of 24; do not infer a P99 or worst-case
hardware bound. The fences exercise completion, not page-table mutation or
instruction-cache coherency under self-modifying code.

## Compare mailbox lock placement

```sh
python tools/mailbox-reference.py target/mailbox-ab-01
cargo xtask smoke --suite mailbox --host-elf target/mailbox-ab-01/inside.elf --timeout-secs 40
cargo xtask smoke --suite mailbox --host-elf target/mailbox-ab-01/outside.elf --timeout-secs 40
```

Choose a **new** output directory. The optional offline helper builds two
default release host ELFs from one snapshot. The production version
(`outside`) publishes work and drops the destination mailbox guard before
the native kick. The reconstructed control (`inside`) holds it until the
kick returns. The helper preserves the two-site diff, compiler version and
hashes; it neither changes the working tree's runtime code nor runs QEMU.
Only pass a trusted local host ELF; it runs in HS mode.

For repeated comparisons use inside, outside, outside, inside; check that
the guest image hash, timebase, vCPU placement and OpenSBI boot hart match.
Keep each boot's samples separate to expose run-to-run drift. Compare these
numbers to the same guest workload, not the UART-to-VS wakeup or native IPI
probes: they have different start and end points.

## Where does a remote fence spend its time?

```sh
cargo xtask smoke --suite mailbox --features selftest --timeout-secs 40
```

The diagnostic reuses the same guest and TIME reads. The target samples
TIME after fulfilling a remote fence, just before publishing the completion.
The guest retains its absolute begin and end; VS and HS share the clock
(`htimedelta=0`). Neither a kick nor a requester polling its ticket is
treated as completion. A translation fence can legitimately be satisfied
without a local hardware flush if the address space was not resident.

Only the existing selftest build keeps a bounded record of the first 32
generations of each fence kind per target mailbox. The host command
`vm fences NAME` reads a **joined** incarnation, not a running VM. Cold
replacement starts with empty records. There is no guest-facing tracing
API, extra clock read in the default build, or change to the fence ordering.
The diagnostic does not accept a prebuilt comparison host.

The verifier matches target, owner, requester, epoch, kind and exact
generation. It requires all 28 completions per kind, drops the four warm-ups
and checks that each scored midpoint falls inside its guest call. At each
reported total-latency rank it prints both segments **of the same call**:
begin to target completion, then completion to guest return. The latter
includes acknowledgement publication, any wakeup or scheduling and return
to VS; it does not measure scheduler time alone. Missing or ambiguous
records fail the diagnostic rather than disappearing from its distribution.

For a call with one remote target, the host also records **how the requester
observed its ticket**. `direct` means the first check in the SBI handler saw
the completed generation; `deferred` means it returned a pending fence to
the dispatcher and a later check saw completion. This needs no extra clock
read. Deferred does not mean the physical hart slept, and direct does not
mean that the vCPU re-entered VS immediately. A cancelled or ambiguous
operation cannot be substituted for either path. A partial multi-target ACK
is not classified as a reply for the whole operation.

Publishing a fence arms its wake registration under the target mailbox
guard; a drain consumes registrations before fulfilling and acknowledging
the covered requests under that same guard. Once every ticket has completed
there is nothing to cancel. The successful path therefore no longer scans
the VM and reacquires target mailboxes to clear their waiter bits. A failed
kick, partial publication or abandoned wait still revokes its registrations;
revoking a wake does not erase the outstanding fence obligation.

## First matched QEMU comparison

Four visits used the same guest Image (SHA-256 prefix `4067a06b`), 10 MHz
TIME, boot hart 0 and vCPU owners [0, 1]. The reconstructed inside host
(`cac55b81`) and production outside host (`2f80beb9`) had the same ELF
entry and load spans. All four runs delivered 28 remote IPI receipts and
shut down cleanly. Values below are **P50 / P95 in microseconds**, 24
scored events per cell, without pooling across boots.

| Visit | IPI round trip | Remote SFENCE.VMA | Remote FENCE.I |
| --- | ---: | ---: | ---: |
| 1 - inside | 108.2 / 168.4 | 161.2 / 250.5 | 161.3 / 225.7 |
| 2 - outside | 89.8 / 185.0 | 172.8 / 322.8 | 186.8 / 241.7 |
| 3 - outside | 104.8 / 194.5 | 165.3 / 258.7 | 159.4 / 248.4 |
| 4 - inside | 106.3 / 185.1 | 160.6 / 286.6 | 165.8 / 238.1 |

Unlocking before the kick had a lower IPI median on both visits, but not
a lower IPI P95. Neither fence shows a repeatable latency win across median
and tail. Keep the shorter lock scope so the peer can take published work
while the native kick is in flight; this result is not a reason to bypass
the fence acknowledgement or to claim an end-to-end hardware speedup.

## First paired target-completion run

Both the production smoke and selftest completed with the same guest Image
(`6e872f26`), 10 MHz TIME, boot hart 0, owners [0, 1] and 28 IPI receipts.
Selftest matched all 56 target fences, including eight warm-ups. These are
the two segments of the **same ranked event** in microseconds:

| Fence / total rank | Guest begin to target completion | Target completion to guest return | Total |
| --- | ---: | ---: | ---: |
| SFENCE.VMA P50 | 135.0 | 133.0 | 268.0 |
| SFENCE.VMA P95 | 182.6 | 220.6 | 403.2 |
| SFENCE.VMA max | 349.2 | 230.1 | 579.3 |
| FENCE.I P50 | 111.7 | 133.6 | 245.3 |
| FENCE.I P95 | 160.2 | 277.1 | 437.3 |
| FENCE.I max | 93.2 | 426.6 | 519.8 |

The return leg was longer in 13/24 SFENCE.VMA and 12/24 FENCE.I events.
Both P95 events spent more time on return, but the slowest SFENCE.VMA
spent more time getting to completion. This is one QEMU diagnostic boot;
it does not identify a universal bottleneck or establish tracing overhead.
The separate uninstrumented regression produced P50/P95 of 217.8/387.3 us
and 184.6/413.6 us respectively. The default host's 182108-byte `.text`
matched the pre-instrumentation production ELF byte for byte.

## Removing cancellation after a successful reply

Four uninstrumented runs used the same guest (`6e872f26`), boot hart 0,
owners [0, 1], and the same 10 MHz TIME. The old host (`2f80beb9`) already
unlocked before kick; the new host (`18b8fdef`) skips the redundant success
cleanup. P50 / P95 below are microseconds, 24 events per cell.

| Visit | IPI (control) | SFENCE.VMA | FENCE.I |
| --- | ---: | ---: | ---: |
| 1 - old | 117.0 / 178.0 | 222.0 / 289.9 | 233.3 / 301.6 |
| 2 - new | 116.9 / 186.4 | 206.9 / 333.0 | 194.7 / 285.7 |
| 3 - new | 117.6 / 167.9 | 199.6 / 271.0 | 217.2 / 302.3 |
| 4 - old | 108.3 / 184.2 | 224.6 / 309.3 | 204.2 / 303.8 |

Both new SFENCE.VMA medians were lower, but its P95 moved in both
directions. Keep the simpler cleanup contract, not a general tail-latency
claim. The scheduler model separately verifies zero disarms on complete,
including fan-out, and explicit disarms on interruption or partial failure.

In a fifth, selftest-only run all 48 scored remote fences actually took
the deferred path. For the total-ranked FENCE.I P95 event, generation 20,
143.5 us elapsed before target completion and 151.3 us after (294.8 us
total). Return was longer in 12/24 FENCE.I events. The direct subset was
empty: this run cannot estimate its latency. These are dedicated-hart QEMU
results, not same-hart overcommit or real-hardware measurements.

## Same-owner remote fences

A guest's remote hart may be a sibling vCPU pinned to the current physical
hart. In that case the sending vCPU is already in HS with its banks put.
The owner temporarily borrows the **target's** saved context, posts the
normal ticket and drains the target mailbox without entering VS. It puts
back only the context, not HSM state, a pending call or retirement state.
The active requester pins the VM epoch throughout. Other waiting requesters
still receive completion kicks; the current caller needs no physical
self-notification. A mixed local/remote fan-out still checks every ticket.

This is not an IPI shortcut: a guest IPI receipt requires the other vCPU to
execute guest instructions. HSM startup, virtual IPI delivery and cross-owner
RFENCE continue to use their normal paths. A RISC-V local physical kick sets
the HS software interrupt bit; only a kick to another hart requires OpenSBI.

The first shared-hart runs kept the guest (`6e872f26`), placement [0, 0],
and default 2 ms quantum fixed. P50 / P95 in microseconds, 24 samples per
cell, in the order of the four uninstrumented visits:

| Visit | IPI round trip | SFENCE.VMA | FENCE.I |
| --- | ---: | ---: | ---: |
| Old (1) | 2958.3 / 4063.8 | 151.9 / 207.5 | 154.5 / 231.7 |
| New (1) | 2701.7 / 4100.3 | 47.1 / 92.1 | 36.5 / 69.1 |
| New (2) | 2538.7 / 3846.8 | 38.9 / 70.5 | 33.1 / 75.7 |
| Old (2) | 2812.1 / 3924.3 | 182.0 / 305.5 | 162.2 / 286.7 |

In separate selftest runs, all 48 scored shared-hart fences were deferred
before this change and direct afterwards. With the new host on two distinct
harts all 48 were deferred in that run. Each of the seven boots delivered
28 guest IPI receipts and shut down cleanly. These results show that this
co-location optimization avoids a scheduling round trip; they do not bound
real hardware latency or make a spinning IPI peer respond without a slice.
