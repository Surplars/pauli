# Copied Ethernet between guests

Two boot VMs with reciprocal `net_peer` slots each see a modern VirtIO-MMIO
Ethernet device. The host validates and copies every frame. No physical NIC,
shared guest mappings or Linux control-device permission is needed. On
Valence, the native manager profile opts in with an empty
`/chosen/openion,peer-net` property. The supplied heartbeat firmware uses
UART; a service that uses Ethernet needs its own VirtIO driver.
[Valence setup](wave8-external-guest.md#optional-ethernet-link-to-the-fixed-service)

## Delivery and lifecycle

Each direction holds up to 16 Ethernet frames of 14–1514 bytes. There is one
RX and one TX split virtqueue per guest (up to 64 descriptors each).
The copy path checks scatter/gather segments and ring metadata against the
guest RAM assignment. For firmware, RX buffers and device-written ring entries
must fit between the RX/RO image pages and the reserved DTB. Linux retains
its full private RAM window for device buffers.

A TX pass takes at most 16 frames, then releases the sending device and link
locks. A successful enqueue identifies the receiving endpoint, both VM epochs
and the queue generation. The receiver rechecks those under its own device
and link locks, copies queued frames into posted RX buffers, updates its IRQ
level, then releases its locks before kicking an eligible physical owner.
The destination pass handles RX only, so simultaneous TX in both directions
cannot nest calls back into the sender. A failed kick leaves the lane marked
for retry. Driver notification suppression and guest IRQ masks still apply.

An absent peer or a full queue consumes the TX descriptor and counts the
drop. A device reset or endpoint retirement clears both queues and invalidates
in-flight notifications. A new binding starts fresh counters. The host monitor
command `vm status NAME` reports `up`, `tx` (enqueues), `rx` (dequeues),
`full`, `down`, their sum `dropped`, and the current inbound count `queued`.
Frames erased by lifecycle transitions are outside the two TX drop counters.
Guest socket and driver drops can occur after a successful dequeue.

## Leases for dynamic guests

The host monitor can give two new children the opposite ends of a copied
channel. Both templates must be cloneable and the board must provide a
`DomainPool`. Linux and firmware children use the same VirtIO device and
private RAM protections as the fixed pair. A child gets a newly rendered DTB
with the network device at GPA `0x10002000`. Creation fails if the template
DTB contains opaque data that the renderer cannot preserve.

```text
hv> vm create linux left --net-lease 3:0
hv> vm create linux right --net-lease 3:1
hv> vm start left
hv> vm start right
hv> vm status left
```

Channels are numbered 0–3 and sides are 0 and 1. Channel 3 gives the example
guests MAC addresses `06:00:00:00:03:00` and `06:00:00:00:03:01`.
The VM slot and epoch may change when a side is reused. Each receive
notification therefore identifies the channel allocation and the endpoint
incarnations. Delivery pins the current target VM and revalidates the
notification under the receiving device lock before accessing guest memory.

The first endpoint reserves a page-rounded host allocation for both queues
and their ownership information (48 KiB, or 12 pages, on RV64). A guest sponsor pays for this allocation
once in its memory budget, in addition to each child's guest RAM. The other
side must have the same sponsor. Host-created endpoints form a separate
ownership group. Each side stays reserved while its child is stopped or
faulted. Device reset or retirement clears both queues. Joined destruction
releases the side. The last release scrubs the entire channel extent and
returns it to the pool. A failed create leaves the prior lease and quota
intact.

The host monitor accepts `--net-lease CHANNEL:SIDE`. Internal guest requests
require `create`, `shape` and the exact side right on their template, for
example `net3:0`. The scalar Linux create interface has no lease argument.
Valence's automatic manager grant supplies no channel-side rights. Static
boot manifests continue to use `net_peer` for fixed pairs.

The dynamic-path QEMU check started Linux children in VM slots 4 and 5
(monitor IDs 5 and 6) from networkless templates. Both directions exchanged
sequenced Ethernet requests and replies. The same check cold-restarted the
second child, destroyed it, and assigned its side to a new child while the
first kept running. Across these phases, 160 of 160 requests received the
expected reply. Creation on an occupied side failed even while its child was
stopped. The throughput results below use the earlier fixed pair.

## QEMU measurement of the fixed pair

The test runs two 128 MiB Linux guests under Windows QEMU 10.2.90
(`g82082b8e38-dirty`, TCG) on the `virt` board with
two emulated RV64H harts, a virtual PLIC, and one vCPU pinned to each hart.
Slot 0 sends and reads both owners' HS statistics. Slot 1 receives and
replies. Both use the same Wave9 Linux source, the stock VirtIO-MMIO and
VirtIO-net drivers, and the same initramfs. The kernel Image SHA-256 is
`ee014fda19916f075147ff278361aa350fbffe1f2eaa5cc2ee930ebe20742e5e`.
The pair has private memory, an MTU of 1500 and no host NIC. The command
below uses the manifest and guest artifacts included with the measurement
record.

The benchmark kernel used `loglevel=8 keep_bootcon initcall_debug` for
both revisions. Attempts with quiet boot arguments on both revisions timed
out waiting for PID 1. The timed phases start after both guests report ready.

```sh
cargo xtask run --vm /path/to/benchmark/pair.toml --host-cpus 2 --features guest-control,selftest
```

- **Goodput.** PID 1 uses an AF_PACKET raw socket, EtherType `0x88b5`.
  Each burst offers 4,096 sequenced Ethernet frames with 1,400 checked
  payload bytes per frame. The receiver reports unique frames, duplicates,
  reordering, byte mismatches and socket drops. Divide unique payload MiB
  and unique frames by elapsed time from the first send to the receiver's
  final acknowledgment. An accepted sender syscall alone earns no credit.
- **Latency.** A single 64-byte request and matching 64-byte reply are in
  flight at a time. The sender measures `CLOCK_MONOTONIC_RAW`, discards 16
  warmups and records 256 trials. A time-out is counted separately. P99 is
  the nearest-rank percentile of completed exchanges.
- **Exit and idle cost.** The manager reads `PAULI_HS_STATS` v2 through
  `/dev/pauli`. Sum the two owners' `vm_exits` deltas around each burst
  and divide by verified payload MiB. Counters cover the whole host,
  including statistics reads and control traffic. For a separate two-second
  interval with both guests idle, count each owner's `idle_wfi` delta.
  These count returns from host WFI, including maintenance timers.
- **Lifecycle.** Take the receiving guest's interface down, send tagged
  traffic and inspect both endpoints with `vm status`. Stop the receiving
  VM, inspect its empty queue, send while it is absent and start it again.
  Check the receiver for old tags before and after the first new burst.

### Observations

Each row is a separate boot. Throughput, exits/MiB and the two percentiles
are medians of three pre-stop runs. The maximum covers all three RTT runs.
The verified column counts distinct valid payloads from 12,288 nonblocking
send attempts. The quiet column sums both owners.

| Revision / boot | Verified | MiB/s | Frames/s | Exits/MiB | RTT P50 / P99 / max (ms) | Quiet host WFI/s |
| --- | ---: | ---: | ---: | ---: | --- | ---: |
| C1 / 1 | 12,288 | 3.013 | 2,257 | 3,441 | 3.937 / 6.485 / 12.709 | 1,366 |
| `d06f68a` / 1 | 607 | 2.035 | 1,524 | 3,961 | 4.440 / 7.543 / 9.674 | 1,433 |
| C1 / 2 | 12,288 | 4.083 | 3,058 | 3,488 | 2.899 / 4.250 / 12.974 | 1,391 |
| `d06f68a` / 2 | 434 | 2.510 | 1,880 | 3,977 | 3.849 / 5.902 / 7.693 | 1,454 |

Both C1 boots filled the 16-frame queue deliberately. With the receiving
interface down, 1,024 sends produced 948 full-queue drops in each boot. The
peer had consumed 60 and still held 16 when stopped. After retirement both
queue depths were zero. During absence, Linux accepted 1,024 ordinary sends
locally and rejected all 1,024 qdisc-bypass sends with `ENOBUFS`. The host
TX and down-drop counters held steady throughout both attempts. A direct
backend regression separately exercises the down-drop branch. On each
restart, the new receiver saw zero old tags and verified all 4,096 fresh
frames. Counting the post-restart runs, C1 delivered 32,768 verified payloads
with zero duplicates, reordering, corruption or socket drops.

The earlier implementation encountered backpressure at the nonblocking sender.
Every bulk frame accepted by its socket was verified at the receiver.
The same offered burst therefore produced different delivered volumes and
durations. All 1,536 measured echo requests completed on each revision.
The largest single RTT occurred on C1 despite its higher bulk goodput.
On C1, MMIO accounted for 80–84% of host exits in these pre-stop bulk runs.
This measures exit frequency; it does not isolate copy time. Quiet WFI
counts include the two guest kernels and the existing mixed/SMP maintenance
schedule. These QEMU timings cannot establish physical Valence latency.

## Static memory of the fixed pair

The release ELF check includes the preallocated frame store for all four boot
slots. In the default Valence build, the RAM span rises from 823,296 to
905,216 bytes in a 14 MiB grant. On default QEMU it rises from 623,104 to
696,928 bytes in a 6 MiB grant. Across the six checked feature configurations
the largest spans are 905,216 bytes on Valence and 771,696 bytes on QEMU.
The boot stack does not allocate the frame store when binding a port.

## Shared-memory alternative

A fixed shared-memory aperture merits a separate experiment if a target-board
workload demonstrates that copying dominates its latency or bandwidth budget.
Measure HS cycles spent on copying, descriptor/IRQ work and scheduling first.
An aperture also needs a shared driver protocol, per-side mapping permissions,
translation retirement on both sides and a reuse barrier before reassigning
pages. Those costs would apply to each shared region even when its peer stops.
