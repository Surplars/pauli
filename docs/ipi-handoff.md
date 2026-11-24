# Guest IPI handoff: shared vs dedicated harts

From the standalone hypervisor workspace:

```sh
cargo xtask smoke --suite ipi-handoff --host-cpus 1 --timeout-secs 40
cargo xtask smoke --suite ipi-handoff --host-cpus 2 --timeout-secs 40
```

Both runs use the same uninstrumented host and diskless two-vCPU VS guest.
The host checks the actual vCPU owners, guest epoch and clean shutdown. There
is one outstanding IPI at a time; no UART or host tracing in the timed path.

For each generation the sender timestamps its SBI send (**begin**); the
receiver consumes the actual VS SSIP, checks the generation and timestamps
its acknowledgement (**received**); the sender timestamps the first matching
receipt it observes (**end**). The second leg is shared-memory visibility
*and time until the sender runs*: there is no return interrupt. The first leg
includes the SBI call path and any wait for the receiver to run, not just
physical interrupt latency.

The receiver changes what it does **after** acknowledgement: busy, WFI, WFI,
busy. Each visit has four warm-ups followed by twelve scored events. Enabling
SSIE lets the next IPI wake WFI; the 32 WFI returns prove those instructions
returned, not that a physical hart slept. The first sample of a visit can
inherit the previous visit's wait policy and is never scored.

The report retains every raw timestamp. Each visit's P50 and maximum give
the two legs of the **same** total-ranked event, not independently ranked
percentiles. At twelve scored samples nearest-rank P95 equals the maximum.
Divide ticks by the printed timebase to get seconds. Accept a run only after
the final `smoke ipi-handoff: PASS`; QEMU timing is not a hardware bound.

## First shared/dedicated comparison

One boot per placement, same default host (`608bdd2d`), same guest
(`e4d0e704`), 10 MHz TIME, 2 ms guest quantum. Entries are microseconds
for the total-ranked P50 event: outbound + return = round trip.

| Placement | Busy 1 | WFI 1 | WFI 2 | Busy 2 |
| --- | ---: | ---: | ---: | ---: |
| Both on hart 0 | 152.1 + 2332.6 = 2484.7 | 208.8 + 97.0 = 305.8 | 221.3 + 90.5 = 311.8 | 224.9 + 2809.0 = 3033.9 |
| Separate harts | 111.2 + 0.3 = 111.5 | 181.6 + 0.3 = 181.9 | 103.3 + 0.2 = 103.5 | 110.2 + 0.3 = 110.5 |

With both vCPUs on hart 0, the return leg was longer in 24/24 scored
busy events but only 4/24 WFI events. On separate harts it was longer
in 0/48. This points to the sender waiting to run after its peer's
ordinary guest store. Faster interrupt injection alone would not address
that interval. Keep the production quantum and fairness policy unchanged;
these two boots do not establish a general tail distribution.
